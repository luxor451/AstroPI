use actix_web::{get, post, web, HttpResponse, Responder};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use camera_control::CameraController;

use crate::astrometry_solver::find_raw_tools;
use crate::state::AppState;

/// Convert a RAW file (CR3/DNG/NEF/…) to JPEG using rawpy's embedded-thumbnail
/// fast path — skips dnglab entirely.  ~50–800 ms vs 10–30 s for dnglab.
fn raw_to_jpeg_fast(raw_path: &Path, jpeg_path: &Path) -> Result<(), String> {
    let script = find_raw_tools();
    let out = std::process::Command::new("python3")
        .args([
            script.to_str().unwrap_or("scripts/raw_tools.py"),
            "snap_jpeg",
            raw_path .to_str().unwrap_or(""),
            jpeg_path.to_str().unwrap_or(""),
        ])
        .output()
        .map_err(|e| format!("Failed to spawn raw_tools.py snap_jpeg: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("snap_jpeg failed: {stderr}"));
    }
    Ok(())
}

const PREVIEW_MAX_PX: u32 = 1920;
const PREVIEW_JPEG_QUALITY: u8 = 82;

/// Resize a JPEG file in-place to fit within `max_side`×`max_side`, re-encoding at `quality`.
/// Silently no-ops on any error — it is a best-effort preview optimisation.
fn resize_jpeg_inplace(path: &Path, max_side: u32, quality: u8) {
    let img = match image::open(path) {
        Ok(i) => i,
        Err(_) => return,
    };
    if img.width() <= max_side && img.height() <= max_side {
        return;
    }
    let resized = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    if let Ok(mut file) = std::fs::File::create(path) {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, quality);
        let _ = resized.write_with_encoder(encoder);
    }
}

#[derive(Deserialize)]
pub struct TakepreviewPayload {
    pub exposure: String,
    pub aperture: String,
    pub iso: String,
}

#[derive(Deserialize)]
pub struct StartLivefeedPayload {
    /// Exposure time in seconds. Defaults to `camera_settings.platesolving_exposure`.
    pub exposure: Option<f64>,
    /// ISO value. Defaults to `camera_settings.iso`.
    pub iso: Option<u64>,
}

#[derive(Deserialize)]
pub struct CameraSettingsPayload {
    pub iso: String,
    pub platesolving_exposure: String,
    pub pixel_size_micron: Option<String>,
    pub focal_length_mm: Option<String>,
    pub bitdepth: Option<String>,
}

#[post("/take_preview")]
pub async fn take_preview(
    data: web::Data<AppState>,
    payload: web::Json<TakepreviewPayload>,
) -> impl Responder {
    println!("Received preview command");

    let iso = payload.iso.parse::<u64>().unwrap_or(800);

    let camera_opt = data.camera.lock().await;
    let camera = match camera_opt.as_ref() {
        Some(c) => c,
        None => return HttpResponse::InternalServerError().body("Camera not connected"),
    };

    let exposure_seconds = payload.exposure.parse::<f64>().unwrap_or(5.0);
    let aperture_str = payload.aperture.trim().replace("f/", "");
    let aperture = if aperture_str.is_empty() {
        None
    } else {
        aperture_str.parse::<f64>().ok()
    };

    println!(
        "Preview params: ISO={}, exposure={}s, aperture={:?}",
        iso, exposure_seconds, aperture
    );
    let _ = data.event_sender.send(format!(
        "Preview params: ISO={}, exposure={}s, aperture={:?}",
        iso, exposure_seconds, aperture
    ));

    let preview_dir = PathBuf::from("imgs/previews");
    if !preview_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&preview_dir) {
            eprintln!("Failed to create preview directory: {}", e);
            return HttpResponse::InternalServerError()
                .body(format!("Failed to create directory: {}", e));
        }
    }

    match camera
        .take_photo(iso, aperture, exposure_seconds, &preview_dir, None)
        .await
    {
        Ok(path) => {
            let jpg_path = path.with_extension("jpg");
            let result_response = if let Err(e) = raw_to_jpeg_fast(&path, &jpg_path) {
                eprintln!("Failed to convert preview: {}", e);
                let _ = data
                    .event_sender
                    .send(format!("Failed to convert preview: {}", e));
                HttpResponse::InternalServerError().body(format!("Conversion failed: {}", e))
            } else {
                println!("Preview saved at: {}", jpg_path.display());
                let _ = data
                    .event_sender
                    .send(format!("Preview saved at: {}", jpg_path.display()));

                // Resize to preview resolution before transfer (much smaller payload)
                resize_jpeg_inplace(&jpg_path, PREVIEW_MAX_PX, PREVIEW_JPEG_QUALITY);
                // Persist as snap_latest.jpg for plate-solving and serving
                let _ = std::fs::copy(&jpg_path, "imgs/snap_latest.jpg");
                std::fs::remove_file(&jpg_path).ok();
                // Return a lightweight ready signal; the image is served at GET /snap_preview
                HttpResponse::Ok().body("preview_ready")
            };

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

#[post("/start_livefeed")]
pub async fn start_livefeed(
    data: web::Data<AppState>,
    payload: web::Json<StartLivefeedPayload>,
) -> impl Responder {
    if data.is_running.load(Ordering::Relaxed) {
        return HttpResponse::Conflict().body("Already running (sequence or livefeed active)");
    }

    let cam_settings = data.camera_settings.lock().await.clone();
    let exposure = payload.exposure.unwrap_or(cam_settings.platesolving_exposure);
    let iso = payload.iso.unwrap_or(cam_settings.iso);

    {
        let guard = data.camera.lock().await;
        if guard.is_none() {
            return HttpResponse::BadRequest().body("Camera not connected");
        }
    }

    data.is_running.store(true, Ordering::Relaxed);
    let _ = data
        .event_sender
        .send(format!("Livefeed started ({:.1}s exposure)...", exposure));

    let data_clone = data.clone();
    tokio::spawn(async move {
        let livefeed_dir = PathBuf::from("imgs/livefeed");
        std::fs::create_dir_all(&livefeed_dir).ok();

        loop {
            if !data_clone.is_running.load(Ordering::Relaxed) {
                break;
            }

            let result = {
                let guard = data_clone.camera.lock().await;
                match guard.as_ref() {
                    None => Err("Camera disconnected".to_string()),
                    Some(camera) => camera
                        .take_photo(
                            iso,
                            None,
                            exposure,
                            &livefeed_dir,
                            Some(&data_clone.is_running),
                        )
                        .await
                        .map_err(|e| e.to_string()),
                }
            };

            match result {
                Ok(path) => {
                    let preview = PathBuf::from("imgs/sequence_latest.jpg");
                    if raw_to_jpeg_fast(&path, &preview).is_ok() {
                        resize_jpeg_inplace(&preview, PREVIEW_MAX_PX, PREVIEW_JPEG_QUALITY);
                        let _ = data_clone.event_sender.send("SEQ_IMAGE_READY".to_string());
                    }
                    std::fs::remove_file(&path).ok();
                }
                Err(e) => {
                    if !e.to_lowercase().contains("cancel")
                        && !e.to_lowercase().contains("abort")
                    {
                        eprintln!("Livefeed error: {}", e);
                        let _ = data_clone
                            .event_sender
                            .send(format!("Livefeed error: {}", e));
                    }
                    break;
                }
            }
        }

        data_clone.is_running.store(false, Ordering::Relaxed);
        let _ = data_clone
            .event_sender
            .send("Livefeed stopped.".to_string());
    });

    HttpResponse::Ok().body("Livefeed started")
}

#[post("/update_camera_settings")]
pub async fn handle_update_camera_settings(
    payload: web::Json<CameraSettingsPayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    let mut settings = data.camera_settings.lock().await;

    settings.iso = payload.iso.parse().unwrap_or(settings.iso);
    settings.platesolving_exposure = payload
        .platesolving_exposure
        .parse()
        .unwrap_or(settings.platesolving_exposure);

    if let Some(ref v) = payload.pixel_size_micron {
        settings.pixel_size_micron = v.parse().unwrap_or(settings.pixel_size_micron);
    }
    if let Some(ref v) = payload.focal_length_mm {
        settings.focal_length_mm = v.parse().unwrap_or(settings.focal_length_mm);
    }
    if let Some(ref v) = payload.bitdepth {
        settings.bitdepth = v.parse().unwrap_or(settings.bitdepth);
    }

    println!(
        "Updated settings: ISO={}, Plate Expose={}, Pixel Size={}μm, Focal Length={}mm, Bitdepth={}",
        settings.iso, settings.platesolving_exposure,
        settings.pixel_size_micron, settings.focal_length_mm, settings.bitdepth
    );
    let _ = data.event_sender.send(format!(
        "Camera settings updated: ISO={}, Plate Expose={}, Pixel Size={}μm, Focal Length={}mm, Bitdepth={}",
        settings.iso, settings.platesolving_exposure,
        settings.pixel_size_micron, settings.focal_length_mm, settings.bitdepth
    ));

    HttpResponse::Ok().body("Settings updated successfully")
}

#[post("/connect_camera")]
pub async fn handle_connect_camera(data: web::Data<AppState>) -> impl Responder {
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
            let model = c.model().await.replace(' ', "_");
            {
                let mut settings = data.camera_settings.lock().await;
                settings.camera_model = model.clone();
            }
            *camera_opt = Some(c);
            println!("Camera connected successfully (model: {}).", model);
            let _ = data
                .event_sender
                .send(format!("Camera connected successfully (model: {}).", model));
            HttpResponse::Ok().body("Camera connected successfully")
        }
        Err(e) => {
            eprintln!("Failed to connect to camera: {}", e);
            let _ = data
                .event_sender
                .send(format!("Failed to connect to camera: {}", e));
            HttpResponse::InternalServerError()
                .body(format!("Failed to connect to camera: {}", e))
        }
    }
}

#[post("/disconnect_camera")]
pub async fn handle_disconnect_camera(data: web::Data<AppState>) -> impl Responder {
    let mut cam = data.camera.lock().await;
    *cam = None;
    let _ = data.event_sender.send("Camera disconnected.".to_string());
    println!("Camera disconnected.");
    HttpResponse::Ok().body("Camera disconnected")
}

#[get("/sequence_preview")]
pub async fn get_sequence_preview() -> impl Responder {
    let path = PathBuf::from("imgs/sequence_latest.jpg");
    match std::fs::read(&path) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type("image/jpeg")
            .insert_header(("Cache-Control", "no-store"))
            .body(bytes),
        Err(_) => HttpResponse::NotFound().body("No sequence preview available"),
    }
}

#[get("/snap_preview")]
pub async fn get_snap_preview() -> impl Responder {
    let path = PathBuf::from("imgs/snap_latest.jpg");
    match std::fs::read(&path) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type("image/jpeg")
            .insert_header(("Cache-Control", "no-store"))
            .body(bytes),
        Err(_) => HttpResponse::NotFound().body("No snap preview available"),
    }
}
