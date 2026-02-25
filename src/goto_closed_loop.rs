use core::f64;

use crate::capture_solve::{capture_and_solve, CaptureSettings};
use astro_pi_plate_solving::{CoordinateEquatorial, CameraConfig};
use camera_control::CameraController;
use eqmod_communication::{IndiClient, IndiError};
use tokio::sync::broadcast::Sender;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const DEVICE_NAME: &str = "EQMod Mount";
const PORT: u16 = 7624;
const TARGET_TOLLERANCE_ARCSEC: f64 = 500.0; // Tolerance for considering the goto successful
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
    println!("DEBUG: Inside goto_closed_loop");
    // Set target coordinates
    let _ = sender.send("Starting Goto sequence...".to_string());


    let target_ra = target_pos.ra.to_hours();
    let target_dec = target_pos.dec.to_degrees();

    let msg = format!(
        "Slewing to adjusted coordinates: RA={:.4}, Dec={:.4}",
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

    // Check before wait
    if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = sender.send("Goto aborted by user.".to_string());
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

    let _ = sender.send("Waiting for mount to settle...".to_string());
    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the mount to stop moving

    // Check before capture
    if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = sender.send("Goto aborted by user.".to_string());
        return Ok(());
    }

    let _ = sender.send("Capturing and solving plate...".to_string());
    let result_platesolve = capture_and_solve(camera, &target_pos, &setting, cam_config).await?;

    let solved_coordinate = CoordinateEquatorial::from_radians(
        result_platesolve.solution.optical_axis_ra,
        result_platesolve.solution.optical_axis_dec,
    );

    let solved_ra = solved_coordinate.ra.to_hours();
    let solved_dec = solved_coordinate.dec.to_degrees();


    // Sync the mount's internal position with the plate-solved coordinates
    println!("DEBUG: Syncing mount with solved coordinates - RA: {:.4}h, Dec: {:.4}°", solved_ra, solved_dec);
    if let Err(e) = client.sync(solved_ra, solved_dec).await {
        let msg = format!("Warning: sync failed: {}", e);
        eprintln!("{}", msg);
        let _ = sender.send(msg);
    }

    let diff_ra_h = target_ra - solved_ra;
    let diff_dec_deg = target_dec - solved_dec;

  
    let mut distance_arcsec = (diff_ra_h * 15.0).hypot(diff_dec_deg) * 3600.0;

    let msg = format!("Initial position error: {:.2} arcsec.", distance_arcsec);
    println!("{}", msg);
    let _ = sender.send(msg);

    let mut i = 0;

    while distance_arcsec > TARGET_TOLLERANCE_ARCSEC && i < MAX_ITERATIONS {
        if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
             let _ = sender.send("Goto aborted by user.".to_string());
             return Ok(());
        }

        let msg = format!(
            "Correction iteration {}: Error {:.2} arcsec > tolerance {:.2}. Adjusting...",
            i + 1,
            distance_arcsec,
            TARGET_TOLLERANCE_ARCSEC
        );
        println!("{}", msg);
        let _ = sender.send(msg);

        client
            .goto(
                target_ra + (target_ra - solved_ra),
                target_dec + (target_dec - solved_dec),
                Some(is_running),
            )
            .await?;

        let _ = sender.send("Waiting for mount to settle...".to_string());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the mount to stop moving

        if !is_running.load(std::sync::atomic::Ordering::Relaxed) {
             let _ = sender.send("Goto aborted by user.".to_string());
             return Ok(());
        }

        let _ = sender.send("Capturing and solving again...".to_string());
        let result_platesolve = capture_and_solve(camera, &target_pos, &setting, cam_config).await?;

        println!("DEBUG: Plate solve result - RA: {:.4}h, Dec: {:.4}°", 
            result_platesolve.solution.optical_axis_ra.to_degrees() / 15.0, 
            result_platesolve.solution.optical_axis_dec.to_degrees()
        );

        let solved_coordinate = CoordinateEquatorial::from_radians(
            result_platesolve.solution.optical_axis_ra,
            result_platesolve.solution.optical_axis_dec,
        );

        let solved_ra_iter = solved_coordinate.ra.to_hours();
        let solved_dec_iter = solved_coordinate.dec.to_degrees();

        // Sync the mount's internal position with the plate-solved coordinates
        println!("DEBUG: Syncing mount with solved coordinates - RA: {:.4}h, Dec: {:.4}°", solved_ra_iter, solved_dec_iter);
        if let Err(e) = client.sync(solved_ra_iter, solved_dec_iter).await {
            let msg = format!("Warning: sync failed: {}", e);
            eprintln!("{}", msg);
            let _ = sender.send(msg);
        }

        println!("DEBUG: Iteration {} - Solved RA: {:.4}h, Dec: {:.4}°", i + 1, solved_ra_iter, solved_dec_iter);

        let diff_ra_h_iter = target_ra - solved_ra_iter;
        let diff_dec_deg_iter = target_dec - solved_dec_iter;

        distance_arcsec = ((diff_ra_h_iter) * 15.0).hypot(diff_dec_deg_iter) * 3600.0;

        println!("DEBUG: Iteration {} - Position error: {:.2} arcsec", i + 1, distance_arcsec);

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
        "Max iterations reached, close enough.".to_string()
    };
    let _ = sender.send(final_msg);

    Ok(())
}

pub async fn init_eqmod_disconnect(client: &IndiClient) -> Result<(), IndiError> {
    client.disconnect().await
}
