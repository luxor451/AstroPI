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

use crate::capture_solve::{make_initial_guess, CaptureSettings, MeridianFlipConfig, run_sequence, SequenceItem};
use astro_pi_plate_solving::{cr3_to_png, CameraConfig};
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
    pub pixel_size_micron: f64,
    pub focal_length_mm: f64,
    pub bitdepth: u16,
}

impl Default for CameraGlobalSettings {
    fn default() -> Self {
        Self {
            iso: 1600,
            platesolving_exposure: 2.0,
            pixel_size_micron: 6.0,
            focal_length_mm: 714.0,
            bitdepth: 14,
        }
    }
}

impl CameraGlobalSettings {
    /// Build a `CameraConfig` from the current global settings.
    pub fn to_camera_config(&self) -> CameraConfig {
        CameraConfig {
            pixel_size_micron: self.pixel_size_micron,
            focal_length_mm: self.focal_length_mm,
            bitdepth: self.bitdepth,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct SequenceOptions {
    park_after: bool,
    shutdown_after: bool,
    meridian_flip: bool,
    post_meridian_limit_h: String,
    subfolder: String,
}

impl Default for SequenceOptions {
    fn default() -> Self {
        Self {
            park_after: false,
            shutdown_after: false,
            meridian_flip: false,
            post_meridian_limit_h: "0.1".to_string(),
            subfolder: String::new(),
        }
    }
}

// ── Sequence state persistence (mirrors frontend types) ──────────────

#[derive(Serialize, Deserialize, Clone)]
struct SequenceItemBackend {
    id: String,
    #[serde(rename = "type")]
    item_type: String,
    exposure: String,
    count: String,
    #[serde(rename = "targetName")]
    target_name: String,
    #[serde(rename = "targetRaDeg")]
    target_ra_deg: f64,
    #[serde(rename = "targetDecDeg")]
    target_dec_deg: f64,
    #[serde(rename = "skipGoto")]
    skip_goto: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct SequenceProgress {
    current: u32,
    total: u32,
    msg: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct BackendSequenceState {
    plan: Vec<SequenceItemBackend>,
    status: String,
    progress: Option<SequenceProgress>,
}

impl Default for BackendSequenceState {
    fn default() -> Self {
        Self {
            plan: vec![],
            status: "idle".to_string(),
            progress: None,
        }
    }
}

/// Partial update struct – only the fields the frontend sends.
#[derive(Deserialize)]
struct SequenceStateUpdate {
    plan: Option<Vec<SequenceItemBackend>>,
    status: Option<String>,
    progress: Option<SequenceProgress>,
}

struct AppState {
    indi_client: RwLock<Option<eqmod_communication::IndiClient>>,
    camera: Mutex<Option<CameraController>>,
    event_sender: broadcast::Sender<String>,
    is_running: Arc<AtomicBool>,
    location: Mutex<Location>,
    indi_server_process: Mutex<Option<std::process::Child>>,
    camera_settings: Mutex<CameraGlobalSettings>,
    closed_loop: Mutex<bool>,
    sequence_options: Mutex<SequenceOptions>,
    sequence_state: Mutex<BackendSequenceState>,
    should_pause: Arc<AtomicBool>,
    /// Active meridian-flip configuration (set when a sequence with flip is running).
    active_flip_config: Mutex<Option<MeridianFlipConfig>>,
}

#[derive(Deserialize)]
struct GoToPayload {
    ra: String,
    dec: String,
    closed_loop: Option<bool>,
}

#[derive(Deserialize)]
struct SequenceGroup {
    /// Display / folder name for this target.
    target: String,
    /// Capture items (light, dark, etc.) for this group.
    sequence: Vec<SequenceItem>,
    /// Target RA in "HH:MM:SS" format (for GoTo & meridian flip).
    ra: Option<String>,
    /// Target Dec in "DD:MM:SS" format (for GoTo & meridian flip).
    dec: Option<String>,
    /// If true, skip GoTo for this group.
    skip_goto: Option<bool>,
    /// If true, this is a Park-only group (no captures).
    park: Option<bool>,
}

#[derive(Deserialize)]
struct StartSequencePayload {
    /// Ordered list of target groups to execute sequentially.
    groups: Vec<SequenceGroup>,
    date: String,
    resume_from: Option<u32>,
    subfolder: Option<String>,
    /// Enable automatic meridian-flip detection during the sequence.
    meridian_flip: Option<bool>,
    /// How many hours past the meridian to wait before flipping (default: 0.1 ≈ 6 min).
    post_meridian_limit_h: Option<f64>,
    /// Park the mount after all groups complete.
    park_after: Option<bool>,
    /// Shut down the Raspberry Pi after all groups (and optional park) complete.
    shutdown_after: Option<bool>,
    /// Use closed-loop (plate-solve) GoTo.
    closed_loop: Option<bool>,
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
    pixel_size_micron: Option<String>,
    focal_length_mm: Option<String>,
    bitdepth: Option<String>,
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
    // Eagerly mark sequence as idle so reconnecting clients see the stop
    {
        let mut ss = data.sequence_state.lock().await;
        ss.status = "idle".to_string();
        ss.progress = None;
    }
    HttpResponse::Ok().body("Stop signal sent")
}

#[post("/pause")]
async fn handle_pause(data: web::Data<AppState>) -> impl Responder {
    println!("Received Pause request");
    let _ = data.event_sender.send("Received Pause request".to_string());
    data.should_pause.store(true, Ordering::Relaxed);
    // Eagerly mark sequence as paused
    {
        let mut ss = data.sequence_state.lock().await;
        ss.status = "paused".to_string();
    }
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

    let cam_config = settings.to_camera_config();
    drop(settings);

    // Cast ra parts to i64 as required by make_initial_guess
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

    settings.iso = payload.iso.parse().unwrap_or(settings.iso);
    settings.platesolving_exposure = payload.platesolving_exposure.parse().unwrap_or(settings.platesolving_exposure);

    if let Some(ref v) = payload.pixel_size_micron {
        settings.pixel_size_micron = v.parse().unwrap_or(settings.pixel_size_micron);
    }
    if let Some(ref v) = payload.focal_length_mm {
        settings.focal_length_mm = v.parse().unwrap_or(settings.focal_length_mm);
    }
    if let Some(ref v) = payload.bitdepth {
        settings.bitdepth = v.parse().unwrap_or(settings.bitdepth);
    }

    println!("Updated settings: ISO={}, Plate Expose={}, Pixel Size={}μm, Focal Length={}mm, Bitdepth={}",
             settings.iso, settings.platesolving_exposure,
             settings.pixel_size_micron, settings.focal_length_mm, settings.bitdepth);
    let _ = data.event_sender.send(format!(
        "Camera settings updated: ISO={}, Plate Expose={}, Pixel Size={}μm, Focal Length={}mm, Bitdepth={}",
        settings.iso, settings.platesolving_exposure,
        settings.pixel_size_micron, settings.focal_length_mm, settings.bitdepth
    ));

    HttpResponse::Ok().body("Settings updated successfully")
}

#[post("/start_sequence")]
async fn handle_start_sequence(
    payload: web::Json<StartSequencePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Start Sequence request ({} group(s))", payload.groups.len());
    let _ = data
        .event_sender
        .send(format!("Received Start Sequence request ({} group(s))", payload.groups.len()));

    let settings_guard = data.camera_settings.lock().await;
    let iso = settings_guard.iso;
    let platesolving_exposure = settings_guard.platesolving_exposure;
    let cam_config = settings_guard.to_camera_config();
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
    let use_closed_loop = payload.closed_loop.unwrap_or(false);

    // Helper closures for RA/Dec parsing (used per-group)
    let parse_hms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 { return None; }
        let h: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        Some(h + m / 60.0 + s / 3600.0)
    };
    let parse_dms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 { return None; }
        let d: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        let sign = if d < 0.0 { -1.0 } else { 1.0 };
        Some(sign * (d.abs() + m / 60.0 + s / 3600.0))
    };
    let parse_time_tuple = |s: &str| -> Option<(u8, u8, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 { return None; }
        Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
    };
    let parse_dec_tuple = |s: &str| -> Option<(i64, i64, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 { return None; }
        Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
    };

    tokio::spawn(async move {
        // Mark sequence as running in persisted state
        {
            let mut ss = data_clone.sequence_state.lock().await;
            ss.status = "running".to_string();
            ss.progress = None;
        }

        // Subscribe to broadcast channel to track PROGRESS messages
        let mut progress_rx = data_clone.event_sender.subscribe();
        let data_for_progress = data_clone.clone();
        let progress_tracker = tokio::spawn(async move {
            loop {
                match progress_rx.recv().await {
                    Ok(msg) if msg.starts_with("PROGRESS:") => {
                        let rest = &msg["PROGRESS:".len()..];
                        if let Some(colon_pos) = rest.find(':') {
                            let counts_part = &rest[..colon_pos];
                            let msg_part = &rest[colon_pos + 1..];
                            let parts: Vec<&str> = counts_part.split('/').collect();
                            if parts.len() == 2 {
                                if let (Ok(current), Ok(total)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                                    let mut ss = data_for_progress.sequence_state.lock().await;
                                    ss.progress = Some(SequenceProgress { current, total, msg: msg_part.to_string() });
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        });

        let start_msg = format!("Starting sequence ({} group(s), resuming from {})...", payload.groups.len(), resume_idx);
        println!("{}", start_msg);
        let _ = data_clone.event_sender.send(start_msg);

        let camera_opt = data_clone.camera.lock().await;
        let camera = match camera_opt.as_ref() {
            Some(c) => c,
            None => {
                let msg = "Camera not connected, cannot start sequence.".to_string();
                eprintln!("{}", msg);
                let _ = data_clone.event_sender.send(msg);
                progress_tracker.abort();
                let mut ss = data_clone.sequence_state.lock().await;
                ss.status = "idle".to_string();
                return;
            }
        };

        let client_guard = data_clone.indi_client.read().await;
        let indi_ref = client_guard.as_ref();

        let mut all_ok = true;
        let num_groups = payload.groups.len();

        for (group_idx, group) in payload.groups.iter().enumerate() {
            // Check if stopped/paused between groups
            if !data_clone.is_running.load(Ordering::Relaxed) {
                let _ = data_clone.event_sender.send("Sequence stopped between groups.".to_string());
                all_ok = false;
                break;
            }

            // --- Park-only group ---
            if group.park.unwrap_or(false) {
                let _ = data_clone.event_sender.send("Parking mount (sequence group)...".to_string());
                if let Some(client) = indi_ref {
                    if let Err(e) = client.park().await {
                        let msg = format!("Park failed during sequence: {}", e);
                        eprintln!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                        all_ok = false;
                        break;
                    }
                    let _ = data_clone.event_sender.send("Mount parked.".to_string());
                }
                continue;
            }

            // --- GoTo (unless skipped) ---
            if !group.skip_goto.unwrap_or(false) {
                if let (Some(ra_str), Some(dec_str)) = (group.ra.as_deref(), group.dec.as_deref()) {
                    let _ = data_clone.event_sender.send(
                        format!("Group {}/{}: Slewing to {}...", group_idx + 1, num_groups, group.target)
                    );

                    let ra_parsed = parse_time_tuple(ra_str);
                    let dec_parsed = parse_dec_tuple(dec_str);

                    match (ra_parsed, dec_parsed) {
                        (Some(ra), Some(dec)) => {
                            let target = make_initial_guess(
                                ra.0 as i64, ra.1 as i64, ra.2,
                                dec.0, dec.1, dec.2,
                            );
                            let platesolve_settings = CaptureSettings {
                                iso,
                                aperture: None,
                                exposure_seconds: platesolving_exposure as u64,
                                save_directory: PathBuf::from("imgs/goto/captures"),
                            };
                            if let Some(indi) = indi_ref {
                                let goto_result = goto_closed_loop(
                                    indi,
                                    Some(camera),
                                    platesolve_settings,
                                    target,
                                    use_closed_loop,
                                    &data_clone.event_sender,
                                    &data_clone.is_running,
                                    &cam_config,
                                ).await;
                                if let Err(e) = goto_result {
                                    let msg = format!("GoTo failed for {}: {}", group.target, e);
                                    eprintln!("{}", msg);
                                    let _ = data_clone.event_sender.send(msg);
                                    all_ok = false;
                                    break;
                                }
                            } else {
                                let msg = "Mount not connected, skipping GoTo.".to_string();
                                let _ = data_clone.event_sender.send(msg);
                            }
                        }
                        _ => {
                            let msg = format!("Invalid RA/Dec for group {}, skipping GoTo.", group.target);
                            let _ = data_clone.event_sender.send(msg);
                        }
                    }
                }
            }

            // --- Build per-group meridian-flip config ---
            let flip_config: Option<MeridianFlipConfig> = if payload.meridian_flip.unwrap_or(false) {
                let ra_h = group.ra.as_deref().and_then(|s| parse_hms(s));
                let dec_deg = group.dec.as_deref().and_then(|s| parse_dms(s));
                match (ra_h, dec_deg) {
                    (Some(ra), Some(dec)) => {
                        let longitude = {
                            let loc = data_clone.location.lock().await;
                            loc.longitude
                        };
                        Some(MeridianFlipConfig {
                            longitude_deg: longitude,
                            target_ra_h: ra,
                            target_dec_deg: dec,
                            post_meridian_limit_h: payload.post_meridian_limit_h.unwrap_or(0.1),
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };

            // Store the active flip config so /status can read it.
            {
                let mut afc = data_clone.active_flip_config.lock().await;
                *afc = flip_config.clone();
            }

            // --- Run captures for this group ---
            let group_resume = if group_idx == 0 { resume_idx } else { 0 };
            let _ = data_clone.event_sender.send(
                format!("Group {}/{}: Capturing {} for {}...", group_idx + 1, num_groups,
                    group.sequence.iter().map(|s| s.count).sum::<u32>(),
                    group.target)
            );

            let seq_result = run_sequence(
                camera,
                &capture_settings,
                &group.sequence,
                &group.target,
                &payload.date,
                payload.subfolder.clone(),
                group_resume,
                &data_clone.event_sender,
                &data_clone.is_running,
                &data_clone.should_pause,
                indi_ref,
                flip_config.as_ref(),
            ).await.map_err(|e| e.to_string());

            match seq_result {
                Ok(_) => {
                    let is_still_running = data_clone.is_running.load(Ordering::Relaxed);
                    let is_paused = data_clone.should_pause.load(Ordering::Relaxed);

                    if !is_still_running || is_paused {
                        // Stopped or paused mid-group
                        all_ok = false;
                        break;
                    }

                    if group_idx < num_groups - 1 {
                        let msg = format!("Group {}/{} complete ({}).", group_idx + 1, num_groups, group.target);
                        println!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                    }
                }
                Err(e) => {
                    let msg = format!("Plan failed for {}: {}", group.target, e);
                    eprintln!("{}", msg);
                    let _ = data_clone.event_sender.send(msg);
                    all_ok = false;
                    break;
                }
            }
        }

        // --- Final status ---
        let is_paused = data_clone.should_pause.load(Ordering::Relaxed);
        if all_ok && !is_paused {
            let msg = "Sequence complete!".to_string();
            println!("{}", msg);
            let _ = data_clone.event_sender.send(msg);
        }

        // Update persisted state
        {
            let mut ss = data_clone.sequence_state.lock().await;
            if is_paused {
                ss.status = "paused".to_string();
            } else {
                ss.status = "idle".to_string();
                ss.progress = None;
            }
        }

        // Stop the progress tracking task
        progress_tracker.abort();

        // Reset running state
        data_clone.is_running.store(false, Ordering::Relaxed);

        // Clear active flip config
        {
            let mut afc = data_clone.active_flip_config.lock().await;
            *afc = None;
        }

        // --- Post-sequence actions (park / shutdown) ---
        if all_ok && !is_paused {
            if payload.park_after.unwrap_or(false) {
                let _ = data_clone.event_sender.send("Parking mount after sequence...".to_string());
                let client_opt = data_clone.indi_client.read().await;
                if let Some(client) = client_opt.as_ref() {
                    if let Err(e) = client.park().await {
                        let msg = format!("Post-sequence park failed: {}", e);
                        eprintln!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                    } else {
                        let _ = data_clone.event_sender.send("Mount parked after sequence.".to_string());
                    }
                }
            }

            if payload.shutdown_after.unwrap_or(false) {
                let _ = data_clone.event_sender.send("Shutting down after sequence...".to_string());
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = std::process::Command::new("sudo")
                    .args(&["shutdown", "-h", "now"])
                    .spawn();
            }
        }
    });

    HttpResponse::Ok().body("Sequence started in background")
}

#[derive(Deserialize)]
struct ManualMovePayload {
    /// One of "north", "south", "east", "west"
    direction: String,
    /// "start" to begin moving, "stop" to cease
    action: String,
    /// Slew-rate index 0-9 (optional, only applied on "start")
    rate: Option<u8>,
}

#[post("/manual_move")]
async fn handle_manual_move(
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

    // Optionally set slew rate before starting
    if start {
        if let Some(rate) = payload.rate {
            if let Err(e) = client.set_slew_rate(rate.min(9)).await {
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

#[derive(Deserialize)]
struct MeridianFlipPayload {
    /// Target RA in "HH:MM:SS" format.
    ra: String,
    /// Target Dec in "DD:MM:SS" format.
    dec: String,
}

#[post("/meridian_flip")]
async fn handle_meridian_flip(
    payload: web::Json<MeridianFlipPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!("Received Force Meridian Flip request");
    let _ = data.event_sender.send("Received Force Meridian Flip request".to_string());

    let parse_hms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 { return None; }
        let h: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        Some(h + m / 60.0 + s / 3600.0)
    };
    let parse_dms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 { return None; }
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
        "Force meridian flip to RA={:.4}h, Dec={:.4}°", ra_h, dec_deg
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
    pier_side: String,
    hour_angle: Option<f64>,
    /// Estimated minutes until the meridian flip triggers (null when no flip is configured).
    time_to_flip_minutes: Option<f64>,
}

// ── Settings endpoints ────────────────────────────────────────────────

#[derive(Serialize)]
struct SettingsResponse {
    camera_settings: CameraGlobalSettings,
    closed_loop: bool,
    observer_location: Location,
    sequence_options: SequenceOptions,
    sequence_state: BackendSequenceState,
}

#[derive(Deserialize)]
struct SettingsUpdate {
    camera_settings: Option<CameraGlobalSettings>,
    closed_loop: Option<bool>,
    observer_location: Option<Location>,
    sequence_options: Option<SequenceOptions>,
    sequence_state: Option<SequenceStateUpdate>,
}

#[get("/settings")]
async fn get_settings(data: web::Data<AppState>) -> impl Responder {
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
async fn update_settings(
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

        // Push to EQMod if connected
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

#[get("/sequence_preview")]
async fn get_sequence_preview() -> impl Responder {
    let path = PathBuf::from("imgs/sequence_latest.jpg");
    match std::fs::read(&path) {
        Ok(bytes) => HttpResponse::Ok().content_type("image/jpeg").body(bytes),
        Err(_) => HttpResponse::NotFound().body("No sequence preview available"),
    }
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
    
    let (eqmod_connected, mount_status, mount_ra, mount_dec, pier_side, hour_angle, time_to_flip_minutes) = {
        let client_lock = data.indi_client.read().await;
        if let Some(client) = &*client_lock {
            let (ra, dec) = client.get_coordinates().await;
            let ps = client.get_pier_side().await;
            let longitude = {
                let loc = data.location.lock().await;
                loc.longitude
            };
            let ha = client.get_hour_angle(longitude).await;

            // Compute time-to-flip from the active flip config (if any).
            let ttf = {
                let afc = data.active_flip_config.lock().await;
                if let Some(fc) = afc.as_ref() {
                    client.time_to_meridian_flip(fc.longitude_deg, fc.post_meridian_limit_h)
                        .await
                        .map(|h| h * 60.0) // convert hours → minutes
                } else {
                    None
                }
            };
            (true, client.get_mount_status().await, ra, dec, ps.to_string(), ha, ttf)
        } else {
            (false, "DISCONNECTED".to_string(), 0.0, 0.0, "UNKNOWN".to_string(), None, None)
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
            .service(receive_command)
            .service(handle_goto)
            .service(handle_update_camera_settings)
            .service(handle_start_sequence)
            .service(handle_stop)
            .service(handle_pause)
            .service(handle_disconnect)
            .service(handle_connect_camera)
            .service(handle_abort)
            .service(handle_manual_move)
            .service(handle_park)
            .service(handle_unpark)
            .service(handle_meridian_flip)
            .service(sse_events)
            .service(handle_status)
            .service(take_preview)
            .service(get_sequence_preview)
            .service(ping)
            .service(get_location)
            .service(update_location)
            .service(update_time)
            .service(handle_restart_indi)
            .service(handle_shutdown)
            .service(handle_reboot)
            .service(get_settings)
            .service(update_settings)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
