use core::f64;

use crate::capture_solve::{capture_and_solve, CaptureSettings};
use astro_pi_plate_solving::{CameraConfig, CoordinateEquatorial};
use camera_control::CameraController;
use eqmod_communication::{IndiClient, IndiError};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;

const DEVICE_NAME: &str = "EQMod Mount";
const PORT: u16 = 7624;
const TARGET_TOLLERANCE_ARCSEC: f64 = 100.0; // Tolerance for considering the goto successful
const MAX_ITERATIONS: usize = 5; // Max iterations for closed-loop correction

pub async fn init_eqmod_goto(
    latitude: f64,
    longitude: f64,
    elevation: f64,
    sender: Sender<String>,
) -> Result<IndiClient, IndiError> {
    // Connect to EQMOD via INDI
    let indi_client = IndiClient::new("localhost", PORT, DEVICE_NAME, Some(sender)).await?;
    indi_client.connect().await?;
    indi_client
        .init_date_pos(latitude, longitude, elevation)
        .await?;
    Ok(indi_client)
}

pub async fn goto_closed_loop(
    client: &IndiClient,
    camera: Option<&CameraController>,
    setting: CaptureSettings,
    target_pos: CoordinateEquatorial,
    close_loop: bool,
    sender: &Sender<String>,
    is_running: &Arc<AtomicBool>,
    cam_config: &CameraConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    // Set target coordinates
    let _ = sender.send("Starting Goto sequence...".to_string());

    let target_ra = target_pos.ra.to_hours();
    let target_dec = target_pos.dec.to_degrees();

    let msg = format!(
        "Slewing to coordinates: RA={:.4}, Dec={:.4}",
        target_ra, target_dec
    );
    println!("{}", msg);
    let _ = sender.send(msg);

    // Initial check
    if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = sender.send("Goto aborted by user.".to_string());
        return Ok(());
    }

    client.goto(target_ra, target_dec, Some(is_running)).await?;

    if !close_loop {
        let _ = sender.send("Closed loop disabled, goto finished.".to_string());
        return Ok(());
    }

    let camera = match camera {
        Some(c) => c,
        None => {
            println!("Closed loop requested but no camera available.");
            let _ = sender.send("Closed loop requested but no camera available.".to_string());
            return Ok(());
        }
    };

    let mut distance_arcsec = f64::INFINITY; // Start with an infinite error to ensure we enter the loop
    let mut i = 0;

    while distance_arcsec > TARGET_TOLLERANCE_ARCSEC && i < MAX_ITERATIONS {
        if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = sender.send("Goto aborted by user.".to_string());
            return Ok(());
        }

        let msg = format!(
            "Correction iteration {}: Error {:.2} arcsec > tolerance {:.2}. Adjusting...",
            i, distance_arcsec, TARGET_TOLLERANCE_ARCSEC
        );
        println!("{}", msg);
        let _ = sender.send(msg);

        let _ = sender.send("Waiting for mount to settle...".to_string());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the mount to stop moving

        if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = sender.send("Goto aborted by user.".to_string());
            return Ok(());
        }

        let _ = sender.send("Capturing and platesolving...".to_string());

        // Platesolving step
        let result_platesolve =
            capture_and_solve(camera, &target_pos, &setting, cam_config).await?;

        // Skip correction entirely if plate solve returned no solution —
        // never sync to (0, 0) which would corrupt the mount's alignment model.
        if result_platesolve.solution.coeffs_x.is_none() {
            let msg = format!(
                "Iteration {}: plate solve failed — skipping correction, retrying capture.",
                i
            );
            eprintln!("{}", msg);
            let _ = sender.send(msg);
            i += 1;
            continue;
        }

        let solved_coordinate = CoordinateEquatorial::from_radians(
            result_platesolve.solution.optical_axis_ra,
            result_platesolve.solution.optical_axis_dec,
        );

        let solved_ra = solved_coordinate.ra.to_hours();
        let solved_dec = solved_coordinate.dec.to_degrees();

        distance_arcsec = ((target_ra - solved_ra) * 15.0).hypot(target_dec - solved_dec) * 3600.0;

        if let Err(e) = client.sync(solved_ra, solved_dec).await {
            let msg = format!("Warning: sync failed: {}", e);
            eprintln!("{}", msg);
            let _ = sender.send(msg);
        }

        tokio::time::sleep(std::time::Duration::from_millis(250)).await; // Wait for mount to process sync

        client.goto(target_ra, target_dec, Some(is_running)).await?;

        let msg = format!(
            "Iteration {} result: Position error is {:.2} arcsec.",
            i + 1,
            distance_arcsec
        );
        println!("{}", msg);
        let _ = sender.send(msg);

        i += 1;
    }

    let final_msg = if distance_arcsec <= TARGET_TOLLERANCE_ARCSEC {
        "Target acquired within tolerance.".to_string()
    } else {
        "Max iterations reached, goto failed.".to_string()
    };
    let _ = sender.send(final_msg);

    Ok(())
}

pub async fn init_eqmod_disconnect(client: &IndiClient) -> Result<(), IndiError> {
    client.disconnect().await
}
