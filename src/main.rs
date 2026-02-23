mod capture_solve;
mod goto_closed_loop;
use actix_cors::Cors;
use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_stream::wrappers::BroadcastStream;

use crate::capture_solve::{make_initial_guess, CaptureSettings, run_sequence, SequenceItem};
use astro_pi_plate_solving::cr3_to_png;
use camera_control::CameraController;
use goto_closed_loop::{goto_closed_loop, init_eqmod_disconnect, init_eqmod_goto};

#[derive(Serialize, Deserialize, Clone)]
struct Location {
    latitude: f64,
    longitude: f64,
    elevation: f64,
}

#[derive(Serialize, Deserialize, Clone)]
struct CameraGlobalSettings {
    pub iso: u64,
    pub platesolving_exposure: f64,
}

impl Default for CameraGlobalSettings {
    fn default() -> Self {
        Self {
            iso: 800,
            platesolving_exposure: 2.0,
        }
    }
}

struct AppState {
    indi_client: RwLock<Option<eqmod_communication::IndiClient>>,
    camera: Mutex<Option<CameraController>>,
    event_sender: broadcast::Sender<String>,
    is_running: Arc<AtomicBool>,
    location: Mutex<Location>,
    indi_server_process: Mutex<Option<std::process::Child>>,
    camera_settings: Mutex<CameraGlobalSettings>,
    should_pause: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct GoToPayload {
    ra: String,
    dec: String,
}

#[derive(Deserialize)]
struct StartSequencePayload {
    sequence: Vec<SequenceItem>,
    target: String,
    date: String,
    resume_from: Option<u32>,
    subfolder: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LocationPayload {
    latitude: f64,
    longitude: f64,
    elevation: f64,
}

#[derive(Deserialize)]
struct CameraSettingsPayload {
    iso: String,
    platesolving_exposure: String,
}

#[get("/location")]
async fn get_location(data: web::Data<AppState>) -> impl Responder {
    let loc = data.location.lock().await;
    HttpResponse::Ok().json(LocationPayload {
        latitude: loc.latitude,
        longitude: loc.longitude,
        elevation: loc.elevation,
    })
}

#[post("/location")]
async fn update_location(
    payload: web::Json<LocationPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!(
        "Received location update: lat={}, lon={}, elev={}",
        payload.latitude, payload.longitude, payload.elevation
    );
    let mut loc = data.location.lock().await;
    loc.latitude = payload.latitude;
    loc.longitude = payload.longitude;
    loc.elevation = payload.elevation;

    // Send event to listening clients
    let _ = data.event_sender.send(format!(
        "Location updated: {}, {}, {}",
        payload.latitude, payload.longitude, payload.elevation
    ));

    // Update hardware if necessary (e.g. tell INDI/EQMod new coords)
    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client
            .set_location(payload.latitude, payload.longitude, payload.elevation)
            .await
        {
            eprintln!("Failed to update location on EQMod: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to update location on EQMod: {}", e));
        } else {
            println!("Location updated on EQMod.");
            let _ = data
                .event_sender
                .send("Location updated on EQMod.".to_string());
        }
    }

    HttpResponse::Ok().body("Location updated successfully")
}

#[derive(Serialize, Deserialize)]
struct TimePayload {
    utc_datetime: String, // Format: YYYY-MM-DDTHH:MM:SS
}

#[post("/time")]
async fn update_time(payload: web::Json<TimePayload>, data: web::Data<AppState>) -> impl Responder {
    println!("Received time update: {}", payload.utc_datetime);

    // Send event to listening clients
    let _ = data
        .event_sender
        .send(format!("Time updated: {}", payload.utc_datetime));

    // Update hardware if necessary
    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client.set_time(&payload.utc_datetime).await {
            eprintln!("Failed to update time on EQMod: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to update time on EQMod: {}", e));
            return HttpResponse::InternalServerError()
                .body(format!("Failed to update time on EQMod: {}", e));
        } else {
            println!("Time updated on EQMod.");
            let _ = data.event_sender.send("Time updated on EQMod.".to_string());
        }
    }

    HttpResponse::Ok().body("Time updated successfully")
}

#[derive(Deserialize)]
struct TakepreviewPayload {
    exposure: String,
    aperture: String,
    iso: String,
}

#[derive(Deserialize)]
struct CommandCheck {
    action: String,
}

#[post("/take_preview")]
async fn take_preview(
    data: web::Data<AppState>,
    payload: web::Json<TakepreviewPayload>,
) -> impl Responder {
    println!("Received preview command");
    
    // Use ISO from payload (sent from frontend global settings)
    let iso = payload.iso.parse::<u64>().unwrap_or(800);

    let camera_opt = data.camera.lock().await;
    let camera = match camera_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("Camera not connected"),
    };

    let exposure_seconds = payload.exposure.parse::<u64>().unwrap_or(5);
    let aperture_str = payload.aperture.trim().replace("f/", "");
    let aperture = if aperture_str.is_empty() {
        None
    } else {
        aperture_str.parse::<f64>().ok()
    };

    println!("Preview params: ISO={}, exposure={}s, aperture={:?}", iso, exposure_seconds, aperture);
    let _ = data.event_sender.send(format!("Preview params: ISO={}, exposure={}s, aperture={:?}", iso, exposure_seconds, aperture));

    let preview_dir = PathBuf::from("imgs/previews");
    if !preview_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&preview_dir) {
            eprintln!("Failed to create preview directory: {}", e);
            return HttpResponse::InternalServerError()
                .body(format!("Failed to create directory: {}", e));
        }
    }

    match camera.take_photo(iso, aperture, exposure_seconds, &preview_dir, None).await {
        Ok(path) => {
            let jpg_path = path.with_extension("jpg");
            let result_response = if let Err(e) = cr3_to_png(&path, &jpg_path) {
                eprintln!("Failed to convert preview to PNG: {}", e);
                let _ = data
                    .event_sender
                    .send(format!("Failed to convert preview to PNG: {}", e));
                HttpResponse::InternalServerError().body(format!("Conversion failed: {}", e))
            } else {
                println!("Preview saved at: {}", jpg_path.display());
                let _ = data
                    .event_sender
                    .send(format!("Preview saved at: {}", jpg_path.display()));

                match std::fs::read(&jpg_path) {
                    Ok(bytes) => {
                        // Clean up the converted file after reading it
                        std::fs::remove_file(&jpg_path).ok();
                        HttpResponse::Ok().content_type("image/jpeg").body(bytes)
                    }
                    Err(e) => {
                        eprintln!("Failed to read preview file: {}", e);
                        // Clean up if read failed
                        std::fs::remove_file(&jpg_path).ok();
                        HttpResponse::InternalServerError().body("Failed to read preview file")
                    }
                }
            };

            // Clean up original CR3 file (path)
            std::fs::remove_file(path).ok();

            result_response
        }
        Err(e) => {
            eprintln!("Preview failed: {}", e);
            let _ = data.event_sender.send(format!("Preview failed: {}", e));
            HttpResponse::InternalServerError().body(format!("Preview failed: {}", e))
        }
    }
}
#[post("/command")]
async fn receive_command(info: web::Json<CommandCheck>) -> impl Responder {
    println!("Received command: {}", info.action);
    if info.action == "launch_function" {
        return HttpResponse::Ok().body("Function launched successfully");
    }
    HttpResponse::BadRequest().body("Unknown command")
}

#[post("/stop")]
async fn handle_stop(data: web::Data<AppState>) -> impl Responder {
    println!("Received Stop request");
    let _ = data.event_sender.send("Received Stop request".to_string());
    data.is_running.store(false, Ordering::Relaxed);
    HttpResponse::Ok().body("Stop signal sent")
}

#[post("/pause")]
async fn handle_pause(data: web::Data<AppState>) -> impl Responder {
    println!("Received Pause request");
    let _ = data.event_sender.send("Received Pause request".to_string());
    data.should_pause.store(true, Ordering::Relaxed);
    HttpResponse::Ok().body("Pause signal sent")
}

#[get("/events")]
async fn sse_events(data: web::Data<AppState>) -> impl Responder {
    let rx = data.event_sender.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(msg) => Some(Ok::<_, actix_web::Error>(web::Bytes::from(format!(
                "data: {}\n\n",
                msg
            )))),
            Err(_e) => None, // Ignore errors (lagged/closed)
        }
    });

    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .streaming(stream)
}

#[post("/connect_camera")]
async fn handle_connect_camera(data: web::Data<AppState>) -> impl Responder {
    println!("Received Connect Camera request");
    let _ = data
        .event_sender
        .send("Received Connect Camera request".to_string());

    let mut camera_opt = data.camera.lock().await;
    if camera_opt.is_some() {
        let _ = data
            .event_sender
            .send("Camera already connected.".to_string());
        return HttpResponse::Ok().body("Camera already connected");
    }

    match CameraController::connect().await {
        Ok(c) => {
            *camera_opt = Some(c);
            println!("Camera connected successfully.");
            let _ = data
                .event_sender
                .send("Camera connected successfully.".to_string());
            HttpResponse::Ok().body("Camera connected successfully")
        }
        Err(e) => {
            eprintln!("Failed to connect to camera: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to connect to camera: {}", e));
            HttpResponse::InternalServerError().body(format!("Failed to connect to camera: {}", e))
        }
    }
}

#[post("/disconnect")]
async fn handle_disconnect(data: web::Data<AppState>) -> impl Responder {
    println!("Received Disconnect request");
    let _ = data.event_sender.send("Received Disconnect request".to_string());

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

#[post("/goto")]
async fn handle_goto(payload: web::Json<GoToPayload>, data: web::Data<AppState>) -> impl Responder {
    println!(
        "Received GoTo request: RA={}, DEC={}",
        payload.ra, payload.dec
    );
    let _ = data.event_sender.send(format!(
        "Received GoTo request: RA={}, DEC={}",
        payload.ra, payload.dec
    ));

    // DEBUG: print to verify execution flow
    println!("Processing GoTo request...");

    // Set running flag
    data.is_running.store(true, Ordering::Relaxed);

    let client = {
        let client_opt = data.indi_client.read().await;
        match client_opt.as_ref() {
            Some(c) => c.clone(),
            None => {
                data.is_running.store(false, Ordering::Relaxed);
                return HttpResponse::InternalServerError().body("EQMod not connected")
            },
        }
    };

    let camera_opt = data.camera.lock().await;
    // Modified to allow proceeding without camera (for open loop goto)
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

    // Get current settings
    let settings = data.camera_settings.lock().await;

    let platesolve_settings = CaptureSettings {
        iso: settings.iso,
        aperture: None,
        exposure_seconds: settings.platesolving_exposure as u64,
        save_directory: PathBuf::from("imgs/goto/captures"),
    };

    // Cast ra parts to i64 as required by make_initial_guess
    let target = make_initial_guess(ra.0 as i64, ra.1 as i64, ra.2, dec.0, dec.1, dec.2);

    let result = goto_closed_loop(
        &client,
        camera,
        platesolve_settings,
        target,
        false,
        &data.event_sender,
        &data.is_running,
    )
    .await;

    // Reset running flag
    data.is_running.store(false, Ordering::Relaxed);

    match result
    {
        Ok(_) => HttpResponse::Ok().body("GoTo completed successfully"),
        Err(e) => {
            eprintln!("GoTo failed: {}", e);
            let _ = data.event_sender.send(format!("GoTo failed: {}", e));
            HttpResponse::InternalServerError().body(format!("GoTo failed: {}", e))
        }
    }
}

#[post("/update_camera_settings")]
async fn handle_update_camera_settings(
    payload: web::Json<CameraSettingsPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    let mut settings = data.camera_settings.lock().await;

    settings.iso = payload.iso.parse().unwrap_or(800);
    settings.platesolving_exposure = payload.platesolving_exposure.parse().unwrap_or(2.0);

    println!("Updated settings: ISO={}, Plate Expose={}", settings.iso, settings.platesolving_exposure);
    let _ = data.event_sender.send(format!("Camera settings updated: ISO={}, Plate Expose={}", settings.iso, settings.platesolving_exposure));

    HttpResponse::Ok().body("Settings updated successfully")
}

#[post("/start_sequence")]
async fn handle_start_sequence(
    payload: web::Json<StartSequencePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Start Sequence request");
    let _ = data
        .event_sender
        .send("Received Start Sequence request".to_string());

    let settings_guard = data.camera_settings.lock().await;
    let iso = settings_guard.iso;
    drop(settings_guard);

    let capture_settings = CaptureSettings {
        iso,
        aperture: None,
        exposure_seconds: 0,
        save_directory: PathBuf::from("imgs/astro_captures"),
    };

    // Set running state to true before starting
    data.is_running.store(true, Ordering::Relaxed);
    data.should_pause.store(false, Ordering::Relaxed);

    let data_clone = data.clone();
    let payload = payload.into_inner();
    let resume_idx = payload.resume_from.unwrap_or(0);

    tokio::spawn(async move {
        // Send a starting message immediately from the task, before acquiring camera lock
        // This ensures the frontend gets feedback even if waiting for the lock
        let start_msg = format!("Starting sequence (resuming from {})...", resume_idx);
        println!("{}", start_msg);
        let _ = data_clone.event_sender.send(start_msg);

        let camera_opt = data_clone.camera.lock().await;
        
        let camera = match camera_opt.as_ref() {
            Some(c) => c,
            None => {
                let msg = "Camera not connected, cannot start sequence.".to_string();
                eprintln!("{}", msg);
                let _ = data_clone.event_sender.send(msg);
                return;
            }
        };

        match run_sequence(
            camera,
            &capture_settings,
            &payload.sequence,
            &payload.target,
            &payload.date,
            payload.subfolder.clone(),
            resume_idx,
            &data_clone.event_sender,
            &data_clone.is_running,
            &data_clone.should_pause,
        ).await {
            Ok(_) => {
                // If paused or cancelled, don't say "Sequence complete!" as run_sequence handles its own messages
                let is_still_running = data_clone.is_running.load(Ordering::Relaxed);
                let is_paused = data_clone.should_pause.load(Ordering::Relaxed);
                
                if is_still_running && !is_paused {
                    let msg = "Sequence complete!".to_string();
                    println!("{}", msg);
                    let _ = data_clone.event_sender.send(msg);
                }
            },
            Err(e) => {
                let msg = format!("Plan failed: {}", e);
                eprintln!("{}", msg);
                let _ = data_clone.event_sender.send(msg);
            }
        }
        
        // Reset running state when sequence finishes (unless paused, where we might want to resume?)
        // For now, if paused, we keep is_running true so frontend sees it as active? 
        // No, current logic is simple pause stops the loop in run_sequence but keeps state?
        // Actually run_sequence returns on pause. So loop exits.
        // If we want to resume, we need to handle that. But for now let's just fix the message.
        if !data_clone.should_pause.load(Ordering::Relaxed) {
             data_clone.is_running.store(false, Ordering::Relaxed);
        } else {
             // If paused, we might want to keep is_running=true to show "Paused" state vs "Stopped"?
             // But run_sequence has returned, so the thread is gone.
             // We probably need to treat pause as a stop that can be resumed by re-sending start_sequence with resume_from.
             data_clone.is_running.store(false, Ordering::Relaxed);
        }
    });

    HttpResponse::Ok().body("Sequence started in background")
}

#[post("/abort")]
async fn handle_abort(data: web::Data<AppState>) -> impl Responder {
    println!("Received Abort request");
    let _ = data.event_sender.send("Received Abort request".to_string());
    
    // Set running flag false to stop loops
    data.is_running.store(false, Ordering::Relaxed);

    // Acquire read lock (should be available even if goto is running, as goto now uses read lock too)
    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        println!("Aborting motion...");
        let _ = data.event_sender.send("Aborting motion...".to_string());
        if let Err(e) = client.abort_motion().await {
            eprintln!("Failed to abort motion: {}", e);
            let _ = data.event_sender.send(format!("Failed to abort motion: {}", e));
        } else {
            println!("Abort command sent.");
            let _ = data.event_sender.send("Abort command sent.".to_string());
        }
    }
    
    HttpResponse::Ok().body("Abort signal sent")
}

#[post("/park")]
async fn handle_park(data: web::Data<AppState>) -> impl Responder {
    println!("Received Park request");
    let _ = data.event_sender.send("Received Park request".to_string());

    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client.park().await {
            eprintln!("Failed to park: {}", e);
            let _ = data.event_sender.send(format!("Failed to park: {}", e));
            return HttpResponse::InternalServerError().body(format!("Failed to park: {}", e));
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
async fn handle_unpark(data: web::Data<AppState>) -> impl Responder {
    println!("Received Unpark request");
    let _ = data.event_sender.send("Received Unpark request".to_string());

    let client_opt = data.indi_client.read().await;
    if let Some(client) = client_opt.as_ref() {
        if let Err(e) = client.unpark().await {
            eprintln!("Failed to unpark: {}", e);
            let _ = data.event_sender.send(format!("Failed to unpark: {}", e));
            return HttpResponse::InternalServerError().body(format!("Failed to unpark: {}", e));
        } else {
            println!("Unpark command sent.");
            let _ = data.event_sender.send("Unpark command sent.".to_string());
        }
    } else {
        return HttpResponse::InternalServerError().body("EQMod not connected");
    }

    HttpResponse::Ok().body("Unpark signal sent")
}

#[post("/shutdown")]
async fn handle_shutdown(data: web::Data<AppState>) -> impl Responder {
    println!("Received Shutdown request");
    let _ = data.event_sender.send("Shutting down Raspberry Pi...".to_string());

    // Give a moment for the SSE message to be sent
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    match std::process::Command::new("sudo")
        .args(&["shutdown", "-h", "now"])
        .spawn()
    {
        Ok(_) => HttpResponse::Ok().body("Shutdown command sent"),
        Err(e) => {
            eprintln!("Failed to shutdown: {}", e);
            let _ = data.event_sender.send(format!("Failed to shutdown: {}", e));
            HttpResponse::InternalServerError().body(format!("Failed to shutdown: {}", e))
        }
    }
}

#[post("/reboot")]
async fn handle_reboot(data: web::Data<AppState>) -> impl Responder {
    println!("Received Reboot request");
    let _ = data.event_sender.send("Rebooting Raspberry Pi...".to_string());

    // Give a moment for the SSE message to be sent
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    match std::process::Command::new("sudo")
        .args(&["reboot"])
        .spawn()
    {
        Ok(_) => HttpResponse::Ok().body("Reboot command sent"),
        Err(e) => {
            eprintln!("Failed to reboot: {}", e);
            let _ = data.event_sender.send(format!("Failed to reboot: {}", e));
            HttpResponse::InternalServerError().body(format!("Failed to reboot: {}", e))
        }
    }
}

#[post("/restart_indi")]
async fn handle_restart_indi(data: web::Data<AppState>) -> impl Responder {
    println!("Received Restart INDI request");
    let _ = data.event_sender.send("Received Restart INDI request".to_string());
    
    let mut process_guard = data.indi_server_process.lock().await;

    if let Some(mut child) = process_guard.take() {
        println!("Killing existing INDI server process...");
        let _ = data.event_sender.send("Killing existing INDI server process...".to_string());
        if let Err(e) = child.kill() {
            eprintln!("Failed to kill process: {}", e);
            let _ = data.event_sender.send(format!("Failed to kill process: {}", e));
        }
        let _ = child.wait();
    } else {
        // Fallback if no child stored
        let _ = std::process::Command::new("pkill")
            .arg("indiserver")
            .output();
        let _ = data.event_sender.send("Sent pkill indiserver command as fallback".to_string());
    }

    // Wait a moment for cleanup
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Start new indiserver
    println!("Starting new INDI server...");
    let _ = data.event_sender.send("Starting new INDI server...".to_string());
    match std::process::Command::new("indiserver")
        .arg("indi_eqmod_telescope")
        .spawn()
    {
        Ok(child) => {
            let pid = child.id();
            *process_guard = Some(child);
            println!("INDI server started with PID: {}", pid);
            let _ = data.event_sender.send(format!("INDI server started with PID: {}", pid));
        }
        Err(e) => {
            eprintln!("Failed to start INDI server: {}", e);
            let _ = data.event_sender.send(format!("Failed to start INDI server: {}", e));
            return HttpResponse::InternalServerError()
                .body(format!("Failed to start INDI server: {}", e));
        }
    }

    // Drop the lock so other parts can use it if they ever needed (though currently only this handler uses it)
    drop(process_guard);

    // Wait for it to start
    let _ = data.event_sender.send("Waiting 2s for INDI server initialization...".to_string());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Reconnect EQMod client
    let _ = data.event_sender.send("Reconnecting EQMod client...".to_string());
    let mut client_opt = data.indi_client.write().await;

    // Attempt to drop the old client cleanly first if possible, though process is gone
    if let Some(client) = client_opt.take() {
        // Maybe call disconnect but it will fail since server is gone. Just drop it.
        drop(client);
    }

    // Connect new client
    // We try multiple times to connect because indiserver might take a while to be ready
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
                let msg = format!("Failed to reconnect to EQMod (attempt {}): {}", retry_count + 1, e);
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

#[derive(Serialize)]
struct StatusResponse {
    camera_connected: bool,
    eqmod_connected: bool,
    is_running: bool,
    mount_status: String,
    mount_ra: f64,
    mount_dec: f64,
}

#[get("/ping")]
async fn ping() -> impl Responder {
    HttpResponse::Ok().body("pong")
}

#[get("/status")]
async fn handle_status(data: web::Data<AppState>) -> impl Responder {
    let camera_connected = if let Ok(guard) = data.camera.try_lock() {
        guard.is_some()
    } else {
        // If we can't get the lock, it means the camera is busy (which implies it's connected/in use)
        // This is an assumption, but prevents blocking the status check during long exposures.
        true 
    };
    
    let (eqmod_connected, mount_status, mount_ra, mount_dec) = {
        let client_lock = data.indi_client.read().await;
        if let Some(client) = &*client_lock {
            let (ra, dec) = client.get_coordinates().await;
            (true, client.get_mount_status().await, ra, dec)
        } else {
            (false, "DISCONNECTED".to_string(), 0.0, 0.0)
        }
    };
    let is_running = data.is_running.load(Ordering::Relaxed);

    HttpResponse::Ok().json(StatusResponse {
        camera_connected,
        eqmod_connected,
        is_running,
        mount_status,
        mount_ra,
        mount_dec,
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Starting INDI server...");
    // Start INDI server as a child process
    let indi_process = std::process::Command::new("indiserver")
        .arg("indi_eqmod_telescope")
        .spawn()
        .expect("Failed to start INDI server");

    // Give it a moment to initialize
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    println!("Initializing hardware...");

    let (tx, _rx) = broadcast::channel(100);

    let camera = match CameraController::connect().await {
        Ok(c) => {
            println!("Camera connected successfully.");
            let _ = tx.send("Camera connected successfully.".to_string());
            Some(c)
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to camera: {}. Continuing without camera.",
                e
            );
            None
        }
    };

    let indi_client = match init_eqmod_goto(0.0, 0.0, 0.0, tx.clone()).await {
        Ok(c) => {
            println!("EQMod connected successfully.");
            let _ = tx.send("EQMod connected successfully.".to_string());
            Some(c)
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to EQMod: {}. Continuing without EQMod.",
                e
            );
            None
        }
    };

    let state = web::Data::new(AppState {
        indi_client: RwLock::new(indi_client),
        camera: Mutex::new(camera),
        event_sender: tx,
        is_running: Arc::new(AtomicBool::new(false)),
        location: Mutex::new(Location {
            latitude: 0.0,
            longitude: 0.0,
            elevation: 0.0,
        }),
        indi_server_process: Mutex::new(Some(indi_process)),
        camera_settings: Mutex::new(CameraGlobalSettings::default()),
        should_pause: Arc::new(AtomicBool::new(false)),
    });

    println!("Starting server at http://0.0.0.0:8080");

    HttpServer::new(move || {
        let cors = Cors::permissive();

        App::new()
            .wrap(cors)
            .app_data(state.clone())
            .service(receive_command)
            .service(handle_goto)
            .service(handle_update_camera_settings)
            .service(handle_start_sequence)
            .service(handle_stop)
            .service(handle_pause)
            .service(handle_disconnect)
            .service(handle_connect_camera)
            .service(handle_abort)
            .service(handle_park)
            .service(handle_unpark)
            .service(sse_events)
            .service(handle_status)
            .service(take_preview)
            .service(ping)
            .service(get_location)
            .service(update_location)
            .service(update_time)
            .service(handle_restart_indi)
            .service(handle_shutdown)
            .service(handle_reboot)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
