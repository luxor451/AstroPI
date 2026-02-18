mod capture_solve;
mod goto_closed_loop;
use actix_cors::Cors;
use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::BroadcastStream;

use crate::capture_solve::{make_initial_guess, planify_shoot, CaptureSettings};
use astro_pi_plate_solving::cr3_to_png;
use camera_control::CameraController;
use goto_closed_loop::{goto_closed_loop, init_eqmod_disconnect, init_eqmod_goto, GotoState};

// TODO : move these constants to a config file or environment variables

const LATITUDE: f64 = 42.960213; // degrees North
const LONGITUDE: f64 = 1.609226; // degrees East
const ELEVATION: f64 = 600.0; // meters

struct AppState {
    indi_client: Mutex<Option<eqmod_communication::IndiClient>>,
    camera: Mutex<Option<CameraController>>,
    goto_state: Mutex<GotoState>,
    event_sender: broadcast::Sender<String>,
    is_running: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct GoToPayload {
    ra: String,
    dec: String,
}

#[derive(Deserialize)]
struct PlanifyPayload {
    lights: String,
    darks: String,
    biases: String,
    iso: String,
    exposure: String,
    aperture: String,
}

#[derive(Deserialize)]
struct TakepreviewPayload {
    iso: String,
    exposure: String,
    aperture: String,
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
    let camera_opt = data.camera.lock().await;
    let camera = match camera_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("Camera not connected"),
    };

    let iso = payload.iso.parse::<u64>().unwrap_or(1600);
    let exposure_seconds = payload.exposure.parse::<u64>().unwrap_or(5);
    let aperture_str = payload.aperture.trim().replace("f/", "");
    let aperture = if aperture_str.is_empty() {
        None
    } else {
        aperture_str.parse::<f64>().ok()
    };

    let preview_dir = PathBuf::from("imgs/previews");
    if !preview_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&preview_dir) {
            eprintln!("Failed to create preview directory: {}", e);
            return HttpResponse::InternalServerError()
                .body(format!("Failed to create directory: {}", e));
        }
    }

    match camera.take_photo(iso, aperture, exposure_seconds, &preview_dir) {
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

    match CameraController::connect() {
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

    let mut client_opt = data.indi_client.lock().await;
    if let Some(mut client) = client_opt.take() {
        if let Err(e) = init_eqmod_disconnect(&mut client).await {
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

    let mut client_opt = data.indi_client.lock().await;
    let client = match client_opt.as_mut() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("EQMod not connected"),
    };

    let camera_opt = data.camera.lock().await;
    let camera = match camera_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("Camera not connected"),
    };

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

    // Cast ra parts to i64 as required by make_initial_guess
    let target = make_initial_guess(ra.0 as i64, ra.1 as i64, ra.2, dec.0, dec.1, dec.2);

    // TODO : add real capture settings for platesolve instead of hardcoded ones

    let platesolve_settings = CaptureSettings {
        iso: 3200,
        aperture: None,
        exposure_seconds: 10,
        save_directory: PathBuf::from("imgs/goto/captures"),
    };

    let mut goto_state = data.goto_state.lock().await;

    // TODO : add true instead of false for closed loop after testing

    match goto_closed_loop(
        &mut *client,
        &*camera,
        platesolve_settings,
        &mut *goto_state,
        target,
        false,
        &data.event_sender,
    )
    .await
    {
        Ok(_) => HttpResponse::Ok().body("GoTo completed successfully"),
        Err(e) => {
            eprintln!("GoTo failed: {}", e);
            let _ = data.event_sender.send(format!("GoTo failed: {}", e));
            HttpResponse::InternalServerError().body(format!("GoTo failed: {}", e))
        }
    }
}

#[post("/planify")]
async fn handle_planify(
    payload: web::Json<PlanifyPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Planify request");
    let _ = data
        .event_sender
        .send("Received Planify request".to_string());

    let camera_opt = data.camera.lock().await;
    let camera = match camera_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("Camera not connected"),
    };

    let lights = payload.lights.parse::<u32>().unwrap_or(0);
    let darks = payload.darks.parse::<u32>().unwrap_or(0);
    let biases = payload.biases.parse::<u32>().unwrap_or(0);
    let iso = payload.iso.parse::<u64>().unwrap_or(1600);
    let exposure = payload.exposure.parse::<u64>().unwrap_or(5);

    let aperture_str = payload.aperture.trim().replace("f/", "");
    let aperture = if aperture_str.is_empty() {
        None
    } else {
        aperture_str.parse::<f64>().ok()
    };

    let capture_settings = CaptureSettings {
        iso,
        aperture,
        exposure_seconds: exposure,
        save_directory: PathBuf::from("imgs/astro_captures"),
    };

    // Set running state to true before starting
    data.is_running.store(true, Ordering::Relaxed);

    match planify_shoot(
        &*camera,
        &capture_settings,
        lights,
        darks,
        biases,
        &data.event_sender,
        &data.is_running,
    ) {
        Ok(_) => HttpResponse::Ok().body("Plan completed successfully"),
        Err(e) => {
            eprintln!("Plan failed: {}", e);
            let _ = data.event_sender.send(format!("Plan failed: {}", e));
            HttpResponse::InternalServerError().body(format!("Plan failed: {}", e))
        }
    }
}

#[derive(Serialize)]
struct StatusResponse {
    camera_connected: bool,
    eqmod_connected: bool,
}

#[get("/ping")]
async fn ping() -> impl Responder {
    HttpResponse::Ok().body("pong")
}

#[get("/status")]
async fn handle_status(data: web::Data<AppState>) -> impl Responder {
    let camera_connected = data.camera.lock().await.is_some();
    let eqmod_connected = data.indi_client.lock().await.is_some();

    HttpResponse::Ok().json(StatusResponse {
        camera_connected,
        eqmod_connected,
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Initializing hardware...");

    let (tx, _rx) = broadcast::channel(100);

    let camera = match CameraController::connect() {
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

    let indi_client = match init_eqmod_goto(LATITUDE, LONGITUDE, ELEVATION, tx.clone()).await {
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
        indi_client: Mutex::new(indi_client),
        camera: Mutex::new(camera),
        goto_state: Mutex::new(GotoState::default()),
        event_sender: tx,
        is_running: Arc::new(AtomicBool::new(false)),
    });

    println!("Starting server at http://0.0.0.0:8080");

    HttpServer::new(move || {
        let cors = Cors::permissive();

        App::new()
            .wrap(cors)
            .app_data(state.clone())
            .service(receive_command)
            .service(handle_goto)
            .service(handle_planify)
            .service(handle_stop)
            .service(handle_disconnect)
            .service(handle_connect_camera)
            .service(sse_events)
            .service(handle_status)
            .service(take_preview)
            .service(ping)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
