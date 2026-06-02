mod astrometry_solver;
mod capture_solve;
mod goto_closed_loop;
mod routes;
mod state;

use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use camera_control::CameraController;
use goto_closed_loop::init_eqmod_goto;
use state::{AppState, BackendSequenceState, CameraGlobalSettings, Location, SequenceOptions};

use routes::camera::{
    get_sequence_preview, get_snap_preview, handle_connect_camera, handle_disconnect_camera,
    handle_update_camera_settings, start_livefeed, take_preview,
};
use routes::gallery::{
    gallery_convert_fits, gallery_delete, gallery_delete_folder, gallery_download, gallery_files,
    gallery_fits_header, gallery_platesolve, gallery_platesolve_snap, gallery_preview,
    gallery_thumbnail,
};
use routes::mount::{
    handle_abort, handle_disconnect, handle_goto, handle_manual_move, handle_meridian_flip,
    handle_park, handle_restart_indi, handle_unpark,
};
use routes::polar_align::{polar_capture_solve, polar_rotate_ra};
use routes::sequence::{handle_pause, handle_start_sequence, handle_stop};
use routes::settings::{get_location, get_settings, update_location, update_settings};
use routes::status::{handle_status, ping, receive_command, sse_events};
use routes::system::{handle_reboot, handle_shutdown, update_time};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Starting INDI server...");
    let indi_process = std::process::Command::new("indiserver")
        .arg("indi_eqmod_telescope")
        .spawn()
        .expect("Failed to start INDI server");

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
        closed_loop: Mutex::new(false),
        sequence_options: Mutex::new(SequenceOptions::default()),
        sequence_state: Mutex::new(BackendSequenceState::default()),
        should_pause: Arc::new(AtomicBool::new(false)),
        active_flip_config: Mutex::new(None),
    });

    println!("Starting server at http://0.0.0.0:8080");

    HttpServer::new(move || {
        let cors = Cors::permissive();

        App::new()
            .wrap(cors)
            .app_data(state.clone())
            // Camera
            .service(take_preview)
            .service(start_livefeed)
            .service(handle_update_camera_settings)
            .service(handle_connect_camera)
            .service(handle_disconnect_camera)
            .service(get_sequence_preview)
            .service(get_snap_preview)
            // Mount
            .service(handle_goto)
            .service(handle_manual_move)
            .service(handle_abort)
            .service(handle_park)
            .service(handle_unpark)
            .service(handle_meridian_flip)
            .service(handle_disconnect)
            .service(handle_restart_indi)
            // Sequence
            .service(handle_start_sequence)
            .service(handle_stop)
            .service(handle_pause)
            // Settings & location
            .service(get_location)
            .service(update_location)
            .service(get_settings)
            .service(update_settings)
            // System
            .service(update_time)
            .service(handle_shutdown)
            .service(handle_reboot)
            // Polar alignment
            .service(polar_capture_solve)
            .service(polar_rotate_ra)
            // Gallery
            .service(gallery_files)
            .service(gallery_thumbnail)
            .service(gallery_preview)
            .service(gallery_convert_fits)
            .service(gallery_fits_header)
            .service(gallery_download)
            .service(gallery_platesolve)
            .service(gallery_platesolve_snap)
            .service(gallery_delete)
            .service(gallery_delete_folder)
            // Status & events
            .service(handle_status)
            .service(sse_events)
            .service(ping)
            .service(receive_command)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
