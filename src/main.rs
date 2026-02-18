mod capture_solve;
mod read_csv;
mod tui;
mod goto_closed_loop;
use actix_cors::Cors;
use actix_web::{post, web, App, HttpResponse, HttpServer, Responder};
use serde::Deserialize;
use tokio::sync::Mutex;
use std::path::PathBuf;

use goto_closed_loop::{init_eqmod_goto, goto_closed_loop, init_eqmod_disconnect, GotoState};
use crate::capture_solve::{planify_shoot, make_initial_guess, CaptureSettings};
use camera_control::CameraController;

const LATITUDE: f64 = 42.960213;   // degrees North
const LONGITUDE: f64 = 1.609226;   // degrees East
const ELEVATION: f64 = 600.0;     // meters

struct AppState {
    indi_client: Mutex<eqmod_communication::IndiClient>,
    camera: Mutex<CameraController>,
    goto_state: Mutex<GotoState>,
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
struct CommandCheck {
    action: String,
}

#[post("/command")]
async fn receive_command(info: web::Json<CommandCheck>) -> impl Responder {
    println!("Received command: {}", info.action);
    if info.action == "launch_function" {
        return HttpResponse::Ok().body("Function launched successfully");
    }
    HttpResponse::BadRequest().body("Unknown command")
}

#[post("/goto")]
async fn handle_goto(
    payload: web::Json<GoToPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received GoTo request: RA={}, DEC={}", payload.ra, payload.dec);

    let parse_time = |s: &str| -> Option<(u8, u8, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 { return None; }
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    };

    let parse_dec = |s: &str| -> Option<(i64, i64, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 { return None; }
        
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

    let target = make_initial_guess(ra.0 as i64, ra.1 as i64, ra.2, dec.0, dec.1, dec.2);
    
    let platesolve_settings = CaptureSettings {
        iso: 3200,
        aperture: None,
        exposure_seconds: 10,
        save_directory: PathBuf::from("imgs/goto/captures"),
    };

    let mut client = data.indi_client.lock().await;
    let camera = data.camera.lock().await;
    let mut goto_state = data.goto_state.lock().await;

    // TODO modifiy this (close_loop = false) to true when the function is ready

    match goto_closed_loop(&mut *client, &*camera, platesolve_settings, &mut *goto_state, target, false).await {
        Ok(_) => HttpResponse::Ok().body("GoTo completed successfully"),
        Err(e) => {
            eprintln!("GoTo failed: {}", e);
            HttpResponse::InternalServerError().body(format!("GoTo failed: {}", e))
        },
    }
}

#[post("/planify")]
async fn handle_planify(
    payload: web::Json<PlanifyPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Planify request");

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

    let camera = data.camera.lock().await;
    
    match planify_shoot(&*camera, &capture_settings, lights, darks, biases) {
        Ok(_) => HttpResponse::Ok().body("Plan completed successfully"),
        Err(e) => {
            eprintln!("Plan failed: {}", e);
            HttpResponse::InternalServerError().body(format!("Plan failed: {}", e))
        },
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Initializing hardware...");

    let camera = match CameraController::connect() {
        Ok(c) => {
            println!("Camera connected successfully.");
            c
        },
        Err(e) => {
            eprintln!("Failed to connect to camera: {}", e);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
        }
    };
    
    let indi_client = match init_eqmod_goto(LATITUDE, LONGITUDE, ELEVATION).await {
        Ok(c) => {
            println!("EQMod connected successfully.");
            c
        },
        Err(e) => {
             eprintln!("Failed to connect to EQMod: {}", e);
             return Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
        }
    };

    let state = web::Data::new(AppState {
        indi_client: Mutex::new(indi_client),
        camera: Mutex::new(camera),
        goto_state: Mutex::new(GotoState::default()),
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
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
