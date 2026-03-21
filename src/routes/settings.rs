use actix_web::{get, post, web, HttpResponse, Responder};
use serde::{Deserialize, Serialize};

use crate::state::{
    AppState, BackendSequenceState, CameraGlobalSettings, Location, SequenceOptions,
    SequenceStateUpdate,
};

#[derive(Serialize, Deserialize)]
pub struct LocationPayload {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
}

#[derive(Serialize)]
struct SettingsResponse {
    camera_settings: CameraGlobalSettings,
    closed_loop: bool,
    observer_location: Location,
    sequence_options: SequenceOptions,
    sequence_state: BackendSequenceState,
}

#[derive(Deserialize)]
pub struct SettingsUpdate {
    pub camera_settings: Option<CameraGlobalSettings>,
    pub closed_loop: Option<bool>,
    pub observer_location: Option<Location>,
    pub sequence_options: Option<SequenceOptions>,
    pub sequence_state: Option<SequenceStateUpdate>,
}

#[get("/location")]
pub async fn get_location(data: web::Data<AppState>) -> impl Responder {
    let loc = data.location.lock().await;
    HttpResponse::Ok().json(LocationPayload {
        latitude: loc.latitude,
        longitude: loc.longitude,
        elevation: loc.elevation,
    })
}

#[post("/location")]
pub async fn update_location(
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

    let _ = data.event_sender.send(format!(
        "Location updated: {}, {}, {}",
        payload.latitude, payload.longitude, payload.elevation
    ));

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

#[get("/settings")]
pub async fn get_settings(data: web::Data<AppState>) -> impl Responder {
    let cam = data.camera_settings.lock().await.clone();
    let cl = *data.closed_loop.lock().await;
    let loc = data.location.lock().await.clone();
    let seq = data.sequence_options.lock().await.clone();
    let ss = data.sequence_state.lock().await.clone();

    HttpResponse::Ok().json(SettingsResponse {
        camera_settings: cam,
        closed_loop: cl,
        observer_location: loc,
        sequence_options: seq,
        sequence_state: ss,
    })
}

#[post("/settings")]
pub async fn update_settings(
    payload: web::Json<SettingsUpdate>,
    data: web::Data<AppState>,
) -> impl Responder {
    if let Some(ref cs) = payload.camera_settings {
        let mut settings = data.camera_settings.lock().await;
        *settings = cs.clone();
    }
    if let Some(cl) = payload.closed_loop {
        let mut closed = data.closed_loop.lock().await;
        *closed = cl;
    }
    if let Some(ref loc) = payload.observer_location {
        let mut location = data.location.lock().await;
        location.latitude = loc.latitude;
        location.longitude = loc.longitude;
        location.elevation = loc.elevation;

        let client_opt = data.indi_client.read().await;
        if let Some(client) = client_opt.as_ref() {
            if let Err(e) = client
                .set_location(loc.latitude, loc.longitude, loc.elevation)
                .await
            {
                eprintln!("Failed to update location on EQMod: {}", e);
            }
        }
    }
    if let Some(ref so) = payload.sequence_options {
        let mut opts = data.sequence_options.lock().await;
        *opts = so.clone();
    }
    if let Some(ref su) = payload.sequence_state {
        let mut ss = data.sequence_state.lock().await;
        if let Some(ref plan) = su.plan {
            ss.plan = plan.clone();
        }
        if let Some(ref status) = su.status {
            ss.status = status.clone();
        }
        if let Some(ref progress) = su.progress {
            ss.progress = Some(progress.clone());
        }
    }

    let _ = data.event_sender.send("Settings updated.".to_string());
    HttpResponse::Ok().body("Settings updated")
}
