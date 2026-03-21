use actix_web::{get, post, web, HttpResponse, Responder};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct CommandCheck {
    pub action: String,
}

#[derive(Serialize)]
struct StatusResponse {
    camera_connected: bool,
    eqmod_connected: bool,
    is_running: bool,
    mount_status: String,
    mount_ra: f64,
    mount_dec: f64,
    pier_side: String,
    hour_angle: Option<f64>,
    /// Estimated minutes until the meridian flip triggers (null when no flip is configured).
    time_to_flip_minutes: Option<f64>,
}

#[get("/ping")]
pub async fn ping() -> impl Responder {
    HttpResponse::Ok().body("pong")
}

#[get("/events")]
pub async fn sse_events(data: web::Data<AppState>) -> impl Responder {
    let rx = data.event_sender.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(msg) => Some(Ok::<_, actix_web::Error>(web::Bytes::from(format!(
                "data: {}\n\n",
                msg
            )))),
            Err(_e) => None,
        }
    });

    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .streaming(stream)
}

#[get("/status")]
pub async fn handle_status(data: web::Data<AppState>) -> impl Responder {
    let camera_connected = if let Ok(guard) = data.camera.try_lock() {
        guard.is_some()
    } else {
        // Camera is busy (in use), assume connected
        true
    };

    let (
        eqmod_connected,
        mount_status,
        mount_ra,
        mount_dec,
        pier_side,
        hour_angle,
        time_to_flip_minutes,
    ) = {
        let client_lock = data.indi_client.read().await;
        if let Some(client) = &*client_lock {
            let (ra, dec) = client.get_coordinates().await;
            let ps = client.get_pier_side().await;
            let longitude = {
                let loc = data.location.lock().await;
                loc.longitude
            };
            let ha = client.get_hour_angle(longitude).await;

            let ttf = {
                let afc = data.active_flip_config.lock().await;
                if let Some(fc) = afc.as_ref() {
                    client
                        .time_to_meridian_flip(fc.longitude_deg, fc.post_meridian_limit_h)
                        .await
                        .map(|h| h * 60.0)
                } else {
                    None
                }
            };
            (
                true,
                client.get_mount_status().await,
                ra,
                dec,
                ps.to_string(),
                ha,
                ttf,
            )
        } else {
            (
                false,
                "DISCONNECTED".to_string(),
                0.0,
                0.0,
                "UNKNOWN".to_string(),
                None,
                None,
            )
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
        pier_side,
        hour_angle,
        time_to_flip_minutes,
    })
}

#[post("/command")]
pub async fn receive_command(info: web::Json<CommandCheck>) -> impl Responder {
    println!("Received command: {}", info.action);
    if info.action == "launch_function" {
        return HttpResponse::Ok().body("Function launched successfully");
    }
    HttpResponse::BadRequest().body("Unknown command")
}
