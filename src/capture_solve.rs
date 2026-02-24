//! Capture and Plate Solve module
//!
//! Provides functionality to capture an image from the camera and plate solve
//! it to determine the actual telescope pointing position.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast::Sender;
// Removed chrono import plan as discussed, using string injection from payload

use astro_pi_plate_solving::{
    solve_plate_with_options, Arcdegrees, CoordinateEquatorial, PlateSolvingResult, RaHoursMinutesSeconds,
    CameraConfig, cr3_to_png,
};
use camera_control::CameraController;
use eqmod_communication::IndiClient;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub enum SequenceType {
    Light,
    Dark,
    Bias,
    Flat,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SequenceItem {
    #[serde(rename = "type")]
    pub item_type: SequenceType,
    pub exposure: f64,
    pub count: u32,
}

#[allow(dead_code)]
/// Result of a capture and solve operation
pub struct CaptureAndSolveResult {
    /// Path to the captured image
    pub image_path: PathBuf,
    /// The plate solving result
    pub solution: PlateSolvingResult,
    /// Time taken for capture (seconds)
    pub capture_time_secs: f64,
    /// Time taken for plate solving (seconds)
    pub solve_time_secs: f64,
}
#[allow(dead_code)]
/// Default capture settings for plate solving
pub struct CaptureSettings {
    /// ISO value (default: 1600)
    pub iso: u64,
    /// Aperture f-stop (None = use current setting)
    pub aperture: Option<f64>,
    /// Exposure time in seconds (default: 5)
    pub exposure_seconds: u64,
    /// Directory to save captured images
    pub save_directory: PathBuf,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            iso: 6400,
            aperture: None,
            exposure_seconds: 5,
            save_directory: PathBuf::from("tmp/astro_captures"),
        }
    }
}
#[allow(dead_code)]
/// Capture an image and plate solve it to determine telescope position
///
/// # Arguments
/// * `camera` - Connected camera controller
/// * `initial_guess` - Initial guess for the telescope position (RA/Dec)
/// * `settings` - Capture settings (ISO, exposure, etc.)
///
/// # Returns
/// * `Ok(CaptureAndSolveResult)` - Contains the solved position and timing info
/// * `Err` - If capture or plate solving fails
///
/// # Example
/// ```no_run
/// use astro_pi_plate_solving::{CoordinateEquatorial, RaHoursMinutesSeconds, Arcdegrees};
/// use dslr_communication::CameraController;
/// use AstroPI::capture_solve::{capture_and_solve, CaptureSettings};
///
/// let camera = CameraController::connect().unwrap();
/// let initial_guess = CoordinateEquatorial::new(
///     RaHoursMinutesSeconds::new(14, 3, 26.0),  // M101 RA
///     Arcdegrees::new(54, 20, 57.0),             // M101 Dec
/// );
/// let settings = CaptureSettings::default();
///
/// let result = capture_and_solve(&camera, &initial_guess, &settings).unwrap();
/// println!("Actual position: RA={}, Dec={}", result.solution.optical_axis_ra, result.solution.optical_axis_dec);
/// ```
pub async fn capture_and_solve(
    camera: &CameraController,
    initial_guess: &CoordinateEquatorial,
    settings: &CaptureSettings,
    cam_config: &CameraConfig,
) -> Result<CaptureAndSolveResult, Box<dyn std::error::Error>> {
    // Ensure save directory exists
    std::fs::create_dir_all(&settings.save_directory)?;

    println!(
        "Capturing image ({}s exposure, ISO {})...",
        settings.exposure_seconds, settings.iso
    );

    // Capture the image
    let capture_start = Instant::now();
    let image_path = camera.take_photo(
        settings.iso,
        settings.aperture,
        settings.exposure_seconds,
        &settings.save_directory,
        None, // No cancellation for single plate solve capture yet (or could pass one)
    ).await?;
    let capture_time = capture_start.elapsed();

    println!("Image captured: {}", image_path.display());
    println!("Capture time: {:.2}s", capture_time.as_secs_f64());

    // Plate solve the image
    println!(
        "\nPlate solving from initial guess: RA={}, Dec={}",
        initial_guess.ra, initial_guess.dec
    );

    let solve_start = Instant::now();
    let solution = solve_plate_with_options(&image_path, initial_guess, 20000, cam_config)?;
    let solve_time = solve_start.elapsed();

    if solution.coeffs_x.is_some() {
        println!("\nSolution found!");
        println!("  Actual RA:  {}", solution.optical_axis_ra);
        println!("  Actual Dec: {}", solution.optical_axis_dec);
        println!("  Position Angle: {:.2}°", solution.rotation_deg);
        println!(
            "  Scale: {:.4} arcsec/pixel",
            solution.scale_arcsec_per_pixel
        );
        println!("  Matched quads: {}", solution.matched_quads_count);
        println!("  Solve time: {:.2}s", solve_time.as_secs_f64());
    } else {
        println!("\nPlate solving failed - no solution found");
    }

    Ok(CaptureAndSolveResult {
        image_path,
        solution,
        capture_time_secs: capture_time.as_secs_f64(),
        solve_time_secs: solve_time.as_secs_f64(),
    })
}
#[allow(dead_code)]
/// Capture and solve with default settings
pub async fn capture_and_solve_quick(
    camera: &CameraController,
    initial_guess: &CoordinateEquatorial,
) -> Result<CaptureAndSolveResult, Box<dyn std::error::Error>> {
    capture_and_solve(camera, initial_guess, &CaptureSettings::default(), &CameraConfig::default()).await
}

/// Create initial guess from RA (hours, minutes, seconds) and Dec (degrees, arcmin, arcsec)
pub fn make_initial_guess(
    ra_h: i64,
    ra_m: i64,
    ra_s: f64,
    dec_d: i64,
    dec_m: i64,
    dec_s: f64,
) -> CoordinateEquatorial {
    CoordinateEquatorial::new(
        RaHoursMinutesSeconds::new(ra_h, ra_m, ra_s),
        Arcdegrees::new(dec_d, dec_m, dec_s),
    )
}

/// Configuration for automatic meridian-flip detection during sequences.
///
/// When provided to [`run_sequence`], the loop will check before each Light
/// frame whether the mount has crossed the meridian and, if so, re-issue a
/// GOTO to let EQMod pick the opposite pier side.
#[derive(Clone, Debug)]
pub struct MeridianFlipConfig {
    /// Observatory longitude in degrees (East-positive).
    pub longitude_deg: f64,
    /// Target RA in hours (0…24).
    pub target_ra_h: f64,
    /// Target Dec in degrees (-90…90).
    pub target_dec_deg: f64,
    /// How many hours *past* the meridian the mount is allowed before
    /// triggering a flip (e.g. 0.1 h ≈ 6 min). Prevents flipping back and
    /// forth right at the meridian.
    pub post_meridian_limit_h: f64,
}

pub async fn run_sequence(
    camera: &CameraController,
    settings: &CaptureSettings, // Base settings (ISO, Save dir root)
    sequence: &[SequenceItem],
    target: &str,
    date_str: &str,
    subfolder: Option<String>,
    resume_from_idx: u32,
    sender: &Sender<String>,
    is_running: &Arc<AtomicBool>,
    should_pause: &Arc<AtomicBool>,
    indi_client: Option<&IndiClient>,
    flip_config: Option<&MeridianFlipConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initial message handled by caller to ensure immediate feedback
    // println!("Starting sequence (resuming from {})...", resume_from_idx);
    // let _ = sender.send(format!("Starting sequence (resuming from {})...", resume_from_idx));

    let mut total_count: u32 = 0;
    for item in sequence {
        total_count += item.count;
    }
    let mut current_global_idx = 0;

    for item in sequence {
        // Check if stopped
        if !is_running.load(Ordering::Relaxed) {
             let msg = "Plan cancelled manually.".to_string();
             println!("{}", msg);
             let _ = sender.send(msg);
             return Ok(());
        }

        // Check if paused (before starting new item type)
        if should_pause.load(Ordering::Relaxed) {
             let msg = "Plan paused manually.".to_string();
             println!("{}", msg);
             let _ = sender.send(msg);
             return Ok(());
        }

        let type_name = match item.item_type {
            SequenceType::Light => "lights",
            SequenceType::Dark => "darks",
            SequenceType::Bias => "biases",
            SequenceType::Flat => "flats",
        };

        let current_save_dir = if let Some(ref sub) = subfolder {
            settings.save_directory.join(sub).join(type_name)
        } else {
            settings.save_directory.join(type_name)
        };

        std::fs::create_dir_all(&current_save_dir)?;

        let exposure = if matches!(item.item_type, SequenceType::Bias) {
            0 // minimal exposure supported by camera lib for bias
        } else {
            item.exposure as u64
        };
        
        for i in 0..item.count {
            current_global_idx += 1;

            if current_global_idx <= resume_from_idx {
                continue;
            }

             if !is_running.load(Ordering::Relaxed) {
                let msg = "Sequence cancelled manually.".to_string();
                println!("{}", msg);
                let _ = sender.send(msg);
                return Ok(());
            }

            // Check pause before starting capture
            if should_pause.load(Ordering::Relaxed) {
                let msg = "Sequence paused manually.".to_string();
                println!("{}", msg);
                let _ = sender.send(msg);
                return Ok(());
            }

            // --- Meridian flip check (Light frames only) ---
            if matches!(item.item_type, SequenceType::Light) {
                if let (Some(client), Some(fc)) = (indi_client, flip_config) {
                    if client.needs_meridian_flip(fc.longitude_deg, fc.post_meridian_limit_h).await {
                        let pier_before = client.get_pier_side().await;
                        let ha = client.get_hour_angle(fc.longitude_deg).await.unwrap_or(f64::NAN);
                        let flip_msg = format!(
                            "Meridian flip triggered! Pier={}, HA={:.3}h. Flipping...",
                            pier_before, ha
                        );
                        println!("{}", flip_msg);
                        let _ = sender.send(flip_msg);

                        match client.execute_meridian_flip(
                            fc.target_ra_h,
                            fc.target_dec_deg,
                            Some(is_running.as_ref()),
                        ).await {
                            Ok(new_side) => {
                                let msg = format!("Meridian flip complete. New pier side: {}", new_side);
                                println!("{}", msg);
                                let _ = sender.send(msg);

                                // Let the mount settle after the flip
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            }
                            Err(e) => {
                                let msg = format!("Meridian flip failed: {}. Continuing anyway.", e);
                                eprintln!("{}", msg);
                                let _ = sender.send(msg);
                            }
                        }

                        // Re-check stop/pause after the flip slew
                        if !is_running.load(Ordering::Relaxed) {
                            let _ = sender.send("Sequence cancelled during meridian flip.".to_string());
                            return Ok(());
                        }
                    }
                }
            }

            let msg = format!("PROGRESS:{}/{}:Capturing {} frame {}/{} ({}s)...", current_global_idx, total_count, type_name, i + 1, item.count, exposure);
            println!("{}", msg);
            let _ = sender.send(msg);
            
            // Note: Aperture is passed as None to keep current or can be wired up if needed
            let path = camera.take_photo(
                settings.iso,
                settings.aperture,
                exposure,
                &current_save_dir,
                Some(is_running),
            ).await?;

            // Check if cancelled during exposure
            if !is_running.load(Ordering::Relaxed) {
                let msg = "Sequence cancelled manually. Deleting last image.".to_string();
                println!("{}", msg);
                let _ = sender.send(msg);
                if path.exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        eprintln!("Failed to delete cancelled image {}: {}", path.display(), e);
                    } else {
                        println!("Deleted cancelled image: {}", path.display());
                    }
                }
                return Ok(());
            }

            // Rename file to include target and date
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy();
                let new_filename = format!("{}_{}_{:04}.{}", target, date_str, current_global_idx, ext_str);
                let new_path = current_save_dir.join(&new_filename);
                if let Err(e) = std::fs::rename(&path, &new_path) {
                    eprintln!("Failed to rename file: {}", e);
                    let _ = sender.send(format!("Warning: Failed to rename file: {}", e));
                } else {
                    println!("Saved to {}", new_path.display());

                    // Convert captured image to JPEG for live preview on the main page
                    let seq_preview_path = PathBuf::from("imgs/sequence_latest.jpg");
                    match cr3_to_png(&new_path, &seq_preview_path) {
                        Ok(_) => {
                            let _ = sender.send("SEQ_IMAGE_READY".to_string());
                        }
                        Err(e) => {
                            eprintln!("Failed to convert sequence image for preview: {}", e);
                        }
                    }
                }
            }
            
            let msg_done = format!("Captured frame {}/{}", current_global_idx, total_count);
            println!("{}", msg_done);
            let _ = sender.send(msg_done);

            // Check pause AFTER capture (so we can pause between frames)
            if should_pause.load(Ordering::Relaxed) {
                let msg = "Sequence paused manually.".to_string();
                println!("{}", msg);
                let _ = sender.send(msg);
                return Ok(());
            }
        }
    }


    let msg = format!(
        "Sequence complete! Captured {} frames total.",
        total_count
    );
    println!("{}", msg);
    let _ = sender.send(msg);

    Ok(())
}
