use actix_web::{post, web, HttpResponse, Responder};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct TimePayload {
    pub utc_datetime: String,
}

#[post("/time")]
pub async fn update_time(
    payload: web::Json<TimePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received time update: {}", payload.utc_datetime);

    let _ = data
        .event_sender
        .send(format!("Time updated: {}", payload.utc_datetime));

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

#[post("/shutdown")]
pub async fn handle_shutdown(data: web::Data<AppState>) -> impl Responder {
    println!("Received Shutdown request");
    let _ = data
        .event_sender
        .send("Shutting down Raspberry Pi...".to_string());

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
pub async fn handle_reboot(data: web::Data<AppState>) -> impl Responder {
    println!("Received Reboot request");
    let _ = data
        .event_sender
        .send("Rebooting Raspberry Pi...".to_string());

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    match std::process::Command::new("sudo").args(&["reboot"]).spawn() {
        Ok(_) => HttpResponse::Ok().body("Reboot command sent"),
        Err(e) => {
            eprintln!("Failed to reboot: {}", e);
            let _ = data.event_sender.send(format!("Failed to reboot: {}", e));
            HttpResponse::InternalServerError().body(format!("Failed to reboot: {}", e))
        }
    }
}
