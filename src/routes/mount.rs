use actix_web::{post, web, HttpResponse, Responder};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::capture_solve::{make_initial_guess, CaptureSettings};
use crate::goto_closed_loop::{goto_closed_loop, init_eqmod_disconnect, init_eqmod_goto};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct GoToPayload {
    pub ra: String,
    pub dec: String,
    pub closed_loop: Option<bool>,
}

#[derive(Deserialize)]
pub struct ManualMovePayload {
    /// One of "north", "south", "east", "west"
    pub direction: String,
    /// "start" to begin moving, "stop" to cease
    pub action: String,
    /// Slew-rate index 0-9 (optional, only applied on "start")
    pub rate: Option<u8>,
}

#[derive(Deserialize)]
pub struct MeridianFlipPayload {
    /// Target RA in "HH:MM:SS" format.
    pub ra: String,
    /// Target Dec in "DD:MM:SS" format.
    pub dec: String,
}

#[post("/goto")]
pub async fn handle_goto(
    payload: web::Json<GoToPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!(
        "Received GoTo request: RA={}, DEC={}",
        payload.ra, payload.dec
    );
    let _ = data.event_sender.send(format!(
        "Received GoTo request: RA={}, DEC={}",
        payload.ra, payload.dec
    ));

    data.is_running.store(true, Ordering::Relaxed);

    let client = {
        let client_opt = data.indi_client.read().await;
        match client_opt.as_ref() {
            Some(c) => c.clone(),
            None => {
                data.is_running.store(false, Ordering::Relaxed);
                return HttpResponse::InternalServerError().body("EQMod not connected");
            }
        }
    };

    let camera_opt = data.camera.lock().await;
    let camera = camera_opt.as_ref();
    if camera.is_none() {
        println!("Camera not connected. Proceeding without camera.");
    }

    let parse_time = |s: &str| -> Option<(u8, u8, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    };

    let parse_dec = |s: &str| -> Option<(i64, i64, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        let d = parts[0].parse().ok()?;
        let m = parts[1].parse().ok()?;
        let s_val = parts[2].parse().ok()?;
        Some((d, m, s_val))
    };

    let ra = match parse_time(&payload.ra) {
        Some(v) => v,
        None => return HttpResponse::BadRequest().body("Invalid RA format. Expected HH:MM:SS"),
    };
    let dec = match parse_dec(&payload.dec) {
        Some(v) => v,
        None => return HttpResponse::BadRequest().body("Invalid DEC format. Expected DD:MM:SS"),
    };

    let settings = data.camera_settings.lock().await;
    let platesolve_settings = CaptureSettings {
        iso: settings.iso,
        aperture: None,
        exposure_seconds: settings.platesolving_exposure,
        save_directory: PathBuf::from("imgs/goto/captures"),
    };
    let cam_config = settings.to_camera_config();
    drop(settings);

    let target = make_initial_guess(ra.0 as i64, ra.1 as i64, ra.2, dec.0, dec.1, dec.2);
    let use_closed_loop = payload.closed_loop.unwrap_or(false);

    let result = goto_closed_loop(
        &client,
        camera,
        platesolve_settings,
        target,
        use_closed_loop,
        &data.event_sender,
        &data.is_running,
        &cam_config,
    )
    .await;

    data.is_running.store(false, Ordering::Relaxed);

    match result {
        Ok(_) => HttpResponse::Ok().body("GoTo completed successfully"),
        Err(e) => {
            eprintln!("GoTo failed: {}", e);
            let _ = data.event_sender.send(format!("GoTo failed: {}", e));
            HttpResponse::InternalServerError().body(format!("GoTo failed: {}", e))
        }
    }
}

#[post("/manual_move")]
pub async fn handle_manual_move(
    payload: web::Json<ManualMovePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    let dir = payload.direction.to_lowercase();
    let start = payload.action.to_lowercase() == "start";

    let client_opt = data.indi_client.read().await;
    let client = match client_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("EQMod not connected"),
    };

    if start {
        if let Some(rate) = payload.rate {
            if let Err(e) = client.set_slew_rate(rate.min(8)).await {
                eprintln!("Failed to set slew rate: {}", e);
            }
        }
    }

    match client.manual_move(&dir, start).await {
        Ok(_) => {
            let verb = if start { "started" } else { "stopped" };
            let msg = format!("Manual move {} {}", dir, verb);
            println!("{}", msg);
            let _ = data.event_sender.send(msg.clone());
            HttpResponse::Ok().body(msg)
        }
        Err(e) => {
            let msg = format!("Manual move failed: {}", e);
            eprintln!("{}", msg);
            let _ = data.event_sender.send(msg.clone());
            HttpResponse::InternalServerError().body(msg)
        }
    }
}

#[post("/abort")]
pub async fn handle_abort(data: web::Data<AppState>) -> impl Responder {
    println!("Received Abort request");
    let _ = data.event_sender.send("Received Abort request".to_string());

    data.is_running.store(false, Ordering::Relaxed);

    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        println!("Aborting motion...");
        let _ = data.event_sender.send("Aborting motion...".to_string());
        if let Err(e) = client.abort_motion().await {
            eprintln!("Failed to abort motion: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to abort motion: {}", e));
        } else {
            println!("Abort command sent.");
            let _ = data.event_sender.send("Abort command sent.".to_string());
        }
    }

    HttpResponse::Ok().body("Abort signal sent")
}

#[post("/park")]
pub async fn handle_park(data: web::Data<AppState>) -> impl Responder {
    println!("Received Park request");
    let _ = data.event_sender.send("Received Park request".to_string());

    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client.park().await {
            eprintln!("Failed to park: {}", e);
            let _ = data.event_sender.send(format!("Failed to park: {}", e));
            return HttpResponse::InternalServerError()
                .body(format!("Failed to park: {}", e));
        } else {
            println!("Park command sent.");
            let _ = data.event_sender.send("Park command sent.".to_string());
        }
    } else {
        return HttpResponse::InternalServerError().body("EQMod not connected");
    }

    HttpResponse::Ok().body("Park signal sent")
}

#[post("/unpark")]
pub async fn handle_unpark(data: web::Data<AppState>) -> impl Responder {
    println!("Received Unpark request");
    let _ = data
        .event_sender
        .send("Received Unpark request".to_string());

    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client.unpark().await {
            eprintln!("Failed to unpark: {}", e);
            let _ = data.event_sender.send(format!("Failed to unpark: {}", e));
            return HttpResponse::InternalServerError()
                .body(format!("Failed to unpark: {}", e));
        } else {
            println!("Unpark command sent.");
            let _ = data.event_sender.send("Unpark command sent.".to_string());
        }
    } else {
        return HttpResponse::InternalServerError().body("EQMod not connected");
    }

    HttpResponse::Ok().body("Unpark signal sent")
}

#[post("/meridian_flip")]
pub async fn handle_meridian_flip(
    payload: web::Json<MeridianFlipPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Force Meridian Flip request");
    let _ = data
        .event_sender
        .send("Received Force Meridian Flip request".to_string());

    let parse_hms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 {
            return None;
        }
        let h: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        Some(h + m / 60.0 + s / 3600.0)
    };
    let parse_dms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 {
            return None;
        }
        let d: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        let sign = if d < 0.0 { -1.0 } else { 1.0 };
        Some(sign * (d.abs() + m / 60.0 + s / 3600.0))
    };

    let ra_h = match parse_hms(&payload.ra) {
        Some(v) => v,
        None => return HttpResponse::BadRequest().body("Invalid RA format. Expected HH:MM:SS"),
    };
    let dec_deg = match parse_dms(&payload.dec) {
        Some(v) => v,
        None => return HttpResponse::BadRequest().body("Invalid DEC format. Expected DD:MM:SS"),
    };

    let client_opt = data.indi_client.read().await;
    let client = match client_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("EQMod not connected"),
    };

    let _ = data.event_sender.send(format!(
        "Force meridian flip to RA={:.4}h, Dec={:.4}°",
        ra_h, dec_deg
    ));

    match client.execute_meridian_flip(ra_h, dec_deg, None).await {
        Ok(new_side) => {
            let msg = format!("Meridian flip complete. New pier side: {}", new_side);
            println!("{}", msg);
            let _ = data.event_sender.send(msg);
            HttpResponse::Ok().body(format!("Flip complete. Pier side: {}", new_side))
        }
        Err(e) => {
            let msg = format!("Meridian flip failed: {}", e);
            eprintln!("{}", msg);
            let _ = data.event_sender.send(msg.clone());
            HttpResponse::InternalServerError().body(msg)
        }
    }
}

#[post("/disconnect")]
pub async fn handle_disconnect(data: web::Data<AppState>) -> impl Responder {
    println!("Received Disconnect request");
    let _ = data
        .event_sender
        .send("Received Disconnect request".to_string());

    let mut client_opt = data.indi_client.write().await;
    if let Some(client) = client_opt.take() {
        if let Err(e) = init_eqmod_disconnect(&client).await {
            eprintln!("Error disconnecting EQMod: {}", e);
        } else {
            println!("EQMod disconnected.");
            let _ = data.event_sender.send("EQMod disconnected.".to_string());
        }
    } else {
        println!("EQMod not connected.");
    }

    let mut camera_opt = data.camera.lock().await;
    if let Some(_camera) = camera_opt.take() {
        println!("Camera disconnected (dropped).");
        let _ = data.event_sender.send("Camera disconnected.".to_string());
    } else {
        println!("Camera not connected.");
    }

    HttpResponse::Ok().body("Disconnected successfully")
}

#[post("/restart_indi")]
pub async fn handle_restart_indi(data: web::Data<AppState>) -> impl Responder {
    println!("Received Restart INDI request");
    let _ = data
        .event_sender
        .send("Received Restart INDI request".to_string());

    let mut process_guard = data.indi_server_process.lock().await;

    if let Some(mut child) = process_guard.take() {
        println!("Killing existing INDI server process...");
        let _ = data
            .event_sender
            .send("Killing existing INDI server process...".to_string());
        if let Err(e) = child.kill() {
            eprintln!("Failed to kill process: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to kill process: {}", e));
        }
        let _ = child.wait();
    } else {
        let _ = std::process::Command::new("pkill")
            .arg("indiserver")
            .output();
        let _ = data
            .event_sender
            .send("Sent pkill indiserver command as fallback".to_string());
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    println!("Starting new INDI server...");
    let _ = data
        .event_sender
        .send("Starting new INDI server...".to_string());
    match std::process::Command::new("indiserver")
        .arg("indi_eqmod_telescope")
        .spawn()
    {
        Ok(child) => {
            let pid = child.id();
            *process_guard = Some(child);
            println!("INDI server started with PID: {}", pid);
            let _ = data
                .event_sender
                .send(format!("INDI server started with PID: {}", pid));
        }
        Err(e) => {
            eprintln!("Failed to start INDI server: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to start INDI server: {}", e));
            return HttpResponse::InternalServerError()
                .body(format!("Failed to start INDI server: {}", e));
        }
    }

    drop(process_guard);

    let _ = data
        .event_sender
        .send("Waiting 2s for INDI server initialization...".to_string());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let _ = data
        .event_sender
        .send("Reconnecting EQMod client...".to_string());
    let mut client_opt = data.indi_client.write().await;

    if let Some(client) = client_opt.take() {
        drop(client);
    }

    let mut retry_count = 0;
    while retry_count < 3 {
        match init_eqmod_goto(0.0, 0.0, 0.0, data.event_sender.clone()).await {
            Ok(c) => {
                println!("EQMod reconnected successfully.");
                let _ = data
                    .event_sender
                    .send("EQMod reconnected successfully.".to_string());
                *client_opt = Some(c);
                return HttpResponse::Ok().body("INDI server restarted and EQMod reconnected.");
            }
            Err(e) => {
                let msg = format!(
                    "Failed to reconnect to EQMod (attempt {}): {}",
                    retry_count + 1,
                    e
                );
                eprintln!("{}", msg);
                let _ = data.event_sender.send(msg);
                retry_count += 1;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    let _ = data
        .event_sender
        .send("Failed to reconnect to EQMod after multiple attempts.".to_string());
    HttpResponse::InternalServerError()
        .body("INDI restarted but EQMod reconnect failed after multiple attempts.")
}
