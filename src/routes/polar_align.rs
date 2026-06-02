use actix_web::{post, web, HttpResponse, Responder};
use astro_pi_plate_solving::CoordinateEquatorial;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::capture_solve::{capture_and_solve, CaptureSettings};
use crate::state::AppState;

#[derive(Serialize)]
struct PolarSolveResponse {
    success: bool,
    ra_deg: Option<f64>,
    dec_deg: Option<f64>,
    message: String,
}

/// POST /polar_align/capture_solve
///
/// Captures a single image using the configured plate-solving exposure/ISO and
/// plate-solves it at full sensor resolution (same code path as goto).
/// Returns the solved RA/Dec centre of the image.
#[post("/polar_align/capture_solve")]
pub async fn polar_capture_solve(data: web::Data<AppState>) -> impl Responder {
    let (iso, exposure, cam_config) = {
        let s = data.camera_settings.lock().await;
        (s.iso, s.platesolving_exposure, s.to_camera_config())
    };

    let (mount_ra_h, mount_dec_deg) = {
        let c = data.indi_client.read().await;
        if let Some(client) = c.as_ref() {
            client.get_coordinates().await
        } else {
            (0.0, 0.0)
        }
    };

    let camera_guard = data.camera.lock().await;
    let camera = match camera_guard.as_ref() {
        Some(c) => c,
        None => {
            return HttpResponse::Ok().json(PolarSolveResponse {
                success: false,
                ra_deg: None,
                dec_deg: None,
                message: "Camera not connected".into(),
            });
        }
    };

    let initial_guess = CoordinateEquatorial::from_radians(
        mount_ra_h * std::f64::consts::PI / 12.0,
        mount_dec_deg * std::f64::consts::PI / 180.0,
    );
    let settings = CaptureSettings {
        iso,
        aperture: None,
        exposure_seconds: exposure,
        save_directory: PathBuf::from("/media/mmcblk1p1/polar_tmp"),
    };

    let _ = data.event_sender.send(format!(
        "Polar align: capturing {:.1}s ISO {} (hint RA={:.2} Dec={:.2})...",
        exposure,
        iso,
        mount_ra_h * 15.0,
        mount_dec_deg,
    ));

    match capture_and_solve(camera, &initial_guess, &settings, &cam_config).await {
        Ok(result) => {
            std::fs::remove_file(&result.image_path).ok();
            if result.solution.coeffs_x.is_some() {
                let ra = result.solution.solution_ra_deg;
                let dec = result.solution.solution_dec_deg;
                let _ = data
                    .event_sender
                    .send(format!("Polar align solved: RA={:.4} Dec={:.4}", ra, dec));
                HttpResponse::Ok().json(PolarSolveResponse {
                    success: true,
                    ra_deg: Some(ra),
                    dec_deg: Some(dec),
                    message: format!("RA {:.4}  Dec {:.4}", ra, dec),
                })
            } else {
                let _ = data
                    .event_sender
                    .send("Polar align: no solution found".into());
                HttpResponse::Ok().json(PolarSolveResponse {
                    success: false,
                    ra_deg: None,
                    dec_deg: None,
                    message: "No solution found — ensure stars are visible near the pole".into(),
                })
            }
        }
        Err(e) => {
            let _ = data
                .event_sender
                .send(format!("Polar align capture failed: {}", e));
            HttpResponse::Ok().json(PolarSolveResponse {
                success: false,
                ra_deg: None,
                dec_deg: None,
                message: e.to_string(),
            })
        }
    }
}

#[derive(Deserialize)]
struct RotateRaPayload {
    ra: String,
    dec: String,
}

/// POST /polar_align/rotate — slew to the given RA/Dec while locking the
/// current pier side so EQMod cannot do a pier flip mid-rotation.
#[post("/polar_align/rotate")]
pub async fn polar_rotate_ra(
    data: web::Data<AppState>,
    payload: web::Json<RotateRaPayload>,
) -> impl Responder {
    let client = {
        let guard = data.indi_client.read().await;
        match guard.as_ref() {
            Some(c) => c.clone(),
            None => return HttpResponse::InternalServerError().body("EQMod not connected"),
        }
    };

    let ra_h = {
        let parts: Vec<&str> = payload.ra.trim().split(':').collect();
        if parts.len() != 3 {
            return HttpResponse::BadRequest().body("Invalid RA format. Expected HH:MM:SS");
        }
        let h: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        h + m / 60.0 + s / 3600.0
    };

    let dec_deg = {
        let parts: Vec<&str> = payload.dec.trim().split(':').collect();
        if parts.len() != 3 {
            return HttpResponse::BadRequest().body("Invalid Dec format. Expected DD:MM:SS");
        }
        let d: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        let sign = if d < 0.0 { -1.0 } else { 1.0 };
        sign * (d.abs() + m / 60.0 + s / 3600.0)
    };

    data.is_running.store(true, Ordering::Relaxed);
    let _ = data.event_sender.send(format!(
        "Polar align: rotating to RA={:.4}h Dec={:.4}° (pier side locked)...",
        ra_h, dec_deg
    ));

    let result = client
        .goto_preserve_pier_side(ra_h, dec_deg, Some(&data.is_running))
        .await;

    data.is_running.store(false, Ordering::Relaxed);

    match result {
        Ok(_) => HttpResponse::Ok().body("ok"),
        Err(e) => {
            let msg = format!("Polar align rotate failed: {}", e);
            eprintln!("{}", msg);
            let _ = data.event_sender.send(msg.clone());
            HttpResponse::InternalServerError().body(msg)
        }
    }
}
