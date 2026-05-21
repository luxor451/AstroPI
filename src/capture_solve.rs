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
    Arcdegrees, CameraConfig, CoordinateEquatorial,
    PlateSolvingResult, RaHoursMinutesSeconds,
};

use crate::astrometry_solver::{raw_to_jpeg_fast, solve_with_astrometry};
use crate::goto_closed_loop::goto_closed_loop;
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
    /// Exposure time in seconds (fractional values supported)
    pub exposure_seconds: f64,
    /// Directory to save captured images
    pub save_directory: PathBuf,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            iso: 6400,
            aperture: None,
            exposure_seconds: 5.0,
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
    let image_path = camera
        .take_photo(
            settings.iso,
            settings.aperture,
            settings.exposure_seconds,
            &settings.save_directory,
            None, // No cancellation for single plate solve capture yet (or could pass one)
        )
        .await?;
    let capture_time = capture_start.elapsed();

    println!("Image captured: {}", image_path.display());
    println!("Capture time: {:.2}s", capture_time.as_secs_f64());

    // Plate solve the image
    println!(
        "\nPlate solving from initial guess: RA={}, Dec={}",
        initial_guess.ra, initial_guess.dec
    );

    let solve_start = Instant::now();
    // Offload the blocking Python subprocess to a dedicated OS thread so the
    // tokio runtime stays free to handle other requests while solving.
    // CoordinateEquatorial is not Clone/Copy, so extract primitive values.
    let solution = {
        let img   = image_path.clone();
        let ra_h  = initial_guess.ra.to_hours();
        let dec_d = initial_guess.dec.to_degrees();
        let cfg   = cam_config.clone();
        match tokio::task::spawn_blocking(move || -> Result<PlateSolvingResult, Box<dyn std::error::Error + Send + Sync>> {
            let guess = CoordinateEquatorial::from_radians(
                ra_h  * std::f64::consts::PI / 12.0,
                dec_d * std::f64::consts::PI / 180.0,
            );
            solve_with_astrometry(&img, &guess, &cfg)
                .map_err(|e| format!("{e}").into())
        }).await {
            Ok(Ok(s))  => s,
            Ok(Err(e)) => return Err(format!("{e}").into()),
            Err(e)     => return Err(format!("plate-solve task panicked: {e}").into()),
        }
    };
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
    capture_and_solve(
        camera,
        initial_guess,
        &CaptureSettings::default(),
        &CameraConfig::default(),
    )
    .await
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
    settings: &CaptureSettings,
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
    focal_mm: f64,
    pixel_size_um: f64,
    recenter_every: u32,
    recenter_target: Option<(f64, f64)>,  // (ra_hours, dec_deg)
    recenter_cam_config: Option<&CameraConfig>,
    recenter_platesolve_secs: f64,
    camera_model: &str,
    custom_suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initial message handled by caller to ensure immediate feedback
    // println!("Starting sequence (resuming from {})...", resume_from_idx);
    // let _ = sender.send(format!("Starting sequence (resuming from {})...", resume_from_idx));

    let mut total_count: u32 = 0;
    for item in sequence {
        total_count += item.count;
    }
    let mut current_global_idx = 0;
    // Counts completed light frames in this run (resets on new run, not on resume)
    let mut light_counter: u32 = 0;

    // Handle for the previous frame's background rename/convert task so we
    // can await it before the sequence finishes (or on cancellation).
    let mut prev_post_task: Option<tokio::task::JoinHandle<()>> = None;

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
            settings.save_directory.join(sub).join(target).join(type_name)
        } else {
            settings.save_directory.join(target).join(type_name)
        };

        std::fs::create_dir_all(&current_save_dir)?;

        let exposure: f64 = if matches!(item.item_type, SequenceType::Bias) {
            0.0 // minimal exposure supported by camera lib for bias
        } else {
            item.exposure
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
                    if client
                        .needs_meridian_flip(fc.longitude_deg, fc.post_meridian_limit_h)
                        .await
                    {
                        let pier_before = client.get_pier_side().await;
                        let ha = client
                            .get_hour_angle(fc.longitude_deg)
                            .await
                            .unwrap_or(f64::NAN);
                        let flip_msg = format!(
                            "Meridian flip triggered! Pier={}, HA={:.3}h. Flipping...",
                            pier_before, ha
                        );
                        println!("{}", flip_msg);
                        let _ = sender.send(flip_msg);

                        match client
                            .execute_meridian_flip(
                                fc.target_ra_h,
                                fc.target_dec_deg,
                                Some(is_running.as_ref()),
                            )
                            .await
                        {
                            Ok(new_side) => {
                                let msg =
                                    format!("Meridian flip complete. New pier side: {}", new_side);
                                println!("{}", msg);
                                let _ = sender.send(msg);

                                // Let the mount settle after the flip
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            }
                            Err(e) => {
                                let msg =
                                    format!("Meridian flip failed: {}. Continuing anyway.", e);
                                eprintln!("{}", msg);
                                let _ = sender.send(msg);
                            }
                        }

                        // Re-check stop/pause after the flip slew
                        if !is_running.load(Ordering::Relaxed) {
                            let _ =
                                sender.send("Sequence cancelled during meridian flip.".to_string());
                            return Ok(());
                        }
                    }
                }
            }

            // --- Auto-recenter (Light frames only) ---
            if matches!(item.item_type, SequenceType::Light)
                && recenter_every > 0
                && light_counter > 0
                && light_counter % recenter_every == 0
            {
                if let (Some(client), Some((rc_ra_h, rc_dec_deg)), Some(rc_cfg)) =
                    (indi_client, recenter_target, recenter_cam_config)
                {
                    let msg = format!(
                        "Auto-recenter: {} light frames captured, recentering...",
                        light_counter
                    );
                    println!("{}", msg);
                    let _ = sender.send(msg);

                    let rc_settings = CaptureSettings {
                        iso: settings.iso,
                        aperture: None,
                        exposure_seconds: recenter_platesolve_secs,
                        save_directory: PathBuf::from("imgs/goto/captures"),
                    };

                    let rc_coord = CoordinateEquatorial::from_radians(
                        rc_ra_h * std::f64::consts::PI / 12.0,
                        rc_dec_deg * std::f64::consts::PI / 180.0,
                    );

                    match goto_closed_loop(
                        client,
                        Some(camera),
                        rc_settings,
                        rc_coord,
                        true,
                        sender,
                        is_running,
                        rc_cfg,
                    )
                    .await
                    {
                        Ok(_) => {
                            let _ = sender.send("Auto-recenter complete.".to_string());
                        }
                        Err(e) => {
                            let msg = format!("Auto-recenter failed: {}. Continuing sequence.", e);
                            eprintln!("{}", msg);
                            let _ = sender.send(msg);
                        }
                    }

                    if !is_running.load(Ordering::Relaxed) {
                        let _ = sender.send("Sequence cancelled during recenter.".to_string());
                        return Ok(());
                    }
                }
            }

            let msg = format!(
                "PROGRESS:{}/{}:Capturing {} frame {}/{} ({}s)...",
                current_global_idx,
                total_count,
                type_name,
                i + 1,
                item.count,
                exposure
            );
            println!("{}", msg);
            let _ = sender.send(msg);

            // Snapshot actual mount coordinates just before the shutter opens.
            let (mount_ra_deg, mount_dec_deg) = if let Some(client) = indi_client {
                let (ra_h, dec) = client.get_coordinates().await;
                (Some(ra_h * 15.0), Some(dec))
            } else {
                (
                    flip_config.map(|c| c.target_ra_h * 15.0),
                    flip_config.map(|c| c.target_dec_deg),
                )
            };

            // Note: Aperture is passed as None to keep current or can be wired up if needed
            let path = camera
                .take_photo(
                    settings.iso,
                    settings.aperture,
                    exposure,
                    &current_save_dir,
                    Some(is_running),
                )
                .await?;

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

            // Spawn the rename + JPEG-conversion in the background so that
            // the next exposure can start immediately (saves ~6 s per frame).
            {
                let sender_bg = sender.clone();
                let path_bg = path.clone();
                let save_dir_bg = current_save_dir.clone();
                let target_bg = target.to_string();
                let date_bg = date_str.to_string();
                let idx = current_global_idx;
                let total = total_count;
                let ra_bg           = mount_ra_deg;
                let dec_bg          = mount_dec_deg;
                let focal_bg        = focal_mm;
                let pixel_bg        = pixel_size_um;
                let exp_bg          = exposure;
                let iso_bg          = settings.iso;
                let camera_model_bg = camera_model.to_string();
                let custom_suffix_bg = custom_suffix.to_string();
                let item_type_bg    = match item.item_type {
                    SequenceType::Light => "Light",
                    SequenceType::Dark  => "Dark",
                    SequenceType::Flat  => "Flat",
                    SequenceType::Bias  => "Bias",
                };

                prev_post_task = Some(tokio::task::spawn_blocking(move || {
                    // Rename file to include target, date, type, camera model and custom suffix
                    if let Some(ext) = path_bg.extension() {
                        let ext_str = ext.to_string_lossy();
                        let model_part = if camera_model_bg.is_empty() {
                            String::new()
                        } else {
                            format!("_{}", camera_model_bg)
                        };
                        let suffix_part = if custom_suffix_bg.is_empty() {
                            String::new()
                        } else {
                            format!("_{}", custom_suffix_bg)
                        };
                        let new_filename = format!(
                            "{}_{}_{}{}{}_{:04}.{}",
                            target_bg, date_bg, item_type_bg,
                            model_part, suffix_part, idx, ext_str
                        );
                        let new_path = save_dir_bg.join(&new_filename);
                        if let Err(e) = std::fs::rename(&path_bg, &new_path) {
                            eprintln!("Failed to rename file: {}", e);
                            let _ =
                                sender_bg.send(format!("Warning: Failed to rename file: {}", e));
                        } else {
                            println!("Saved to {}", new_path.display());

                            // Convert captured image to JPEG for live preview (fast rawpy path)
                            let seq_preview_path = PathBuf::from("imgs/sequence_latest.jpg");
                            match raw_to_jpeg_fast(&new_path, &seq_preview_path) {
                                Ok(_) => {
                                    let _ = sender_bg.send("SEQ_IMAGE_READY".to_string());
                                }
                                Err(e) => {
                                    eprintln!("Failed to convert sequence image for preview: {}", e);
                                }
                            }

                            // Auto-convert RAW → 16-bit Bayer FITS with mount metadata for Siril
                            let fits_path = new_path.with_extension("fits");
                            if !fits_path.exists() {
                                let ra_str  = ra_bg .map(|v| format!("{v:.6}")).unwrap_or_default();
                                let dec_str = dec_bg.map(|v| format!("{v:.6}")).unwrap_or_default();
                                let out = std::process::Command::new("python3")
                                    .arg("scripts/raw_tools.py")
                                    .args([
                                        "tiff",   // command still named "tiff", now outputs FITS
                                        new_path.to_str().unwrap_or(""),
                                        fits_path.to_str().unwrap_or(""),
                                        &target_bg,
                                        &ra_str,
                                        &dec_str,
                                        &format!("{focal_bg:.3}"),
                                        &format!("{pixel_bg:.3}"),
                                        &format!("{exp_bg:.3}"),
                                        &format!("{iso_bg}"),
                                    ])
                                    .output();
                                match out {
                                    Ok(o) if o.status.success() => {
                                        let stem = fits_path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy();
                                        let _ = sender_bg.send(format!("FITS saved: {stem}"));
                                        // Delete the RAW — only the FITS is kept.
                                        if let Err(e) = std::fs::remove_file(&new_path) {
                                            eprintln!("Failed to delete RAW {}: {e}", new_path.display());
                                        }
                                    }
                                    Ok(o) => eprintln!(
                                        "FITS conversion failed: {}",
                                        String::from_utf8_lossy(&o.stderr)
                                    ),
                                    Err(e) => eprintln!("raw_tools.py spawn error: {e}"),
                                }
                            }
                        }
                    }

                    let msg_done = format!("Captured frame {}/{}", idx, total);
                    println!("{}", msg_done);
                    let _ = sender_bg.send(msg_done);
                }));
            }

            // Track completed light frames for auto-recenter
            if matches!(item.item_type, SequenceType::Light) {
                light_counter += 1;
            }

            // Check pause AFTER capture (so we can pause between frames)
            if should_pause.load(Ordering::Relaxed) {
                let msg = "Sequence paused manually.".to_string();
                println!("{}", msg);
                let _ = sender.send(msg);
                return Ok(());
            }
        }
    }

    // Wait for the last background rename/convert task to finish before
    // announcing completion.
    if let Some(task) = prev_post_task.take() {
        let _ = task.await;
    }

    let msg = format!("Sequence complete! Captured {} frames total.", total_count);
    println!("{}", msg);
    let _ = sender.send(msg);

    Ok(())
}
