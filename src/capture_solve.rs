//! Capture and Plate Solve module
//!
//! Provides functionality to capture an image from the camera and plate solve
//! it to determine the actual telescope pointing position.

use std::path::PathBuf;
use std::time::Instant;

use astro_pi_plate_solving::{
    solve_plate, Arcdegrees, CoordinateEquatorial, PlateSolvingResult, RaHoursMinutesSeconds,
};
use camera_control::CameraController;

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
            exposure_seconds: 1,
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
pub fn capture_and_solve(
    camera: &CameraController,
    initial_guess: &CoordinateEquatorial,
    settings: &CaptureSettings,
) -> Result<CaptureAndSolveResult, Box<dyn std::error::Error>> {
    // Ensure save directory exists
    std::fs::create_dir_all(&settings.save_directory)?;

    println!("Capturing image ({}s exposure, ISO {})...", settings.exposure_seconds, settings.iso);
    
    // Capture the image
    let capture_start = Instant::now();
    let image_path = camera.take_photo(
        settings.iso,
        settings.aperture,
        settings.exposure_seconds,
        &settings.save_directory,
    )?;
    let capture_time = capture_start.elapsed();
    
    println!("Image captured: {}", image_path.display());
    println!("Capture time: {:.2}s", capture_time.as_secs_f64());

    // Plate solve the image
    println!("\nPlate solving from initial guess: RA={}, Dec={}",
             initial_guess.ra, initial_guess.dec);
    
    let solve_start = Instant::now();
    let solution = solve_plate(&image_path, initial_guess)?;
    let solve_time = solve_start.elapsed();

    if solution.coeffs_x.is_some() {
        println!("\nSolution found!");
        println!("  Actual RA:  {}", solution.optical_axis_ra);
        println!("  Actual Dec: {}", solution.optical_axis_dec);
        println!("  Position Angle: {:.2}°", solution.rotation_deg);
        println!("  Scale: {:.4} arcsec/pixel", solution.scale_arcsec_per_pixel);
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
pub fn capture_and_solve_quick(
    camera: &CameraController,
    initial_guess: &CoordinateEquatorial,
) -> Result<CaptureAndSolveResult, Box<dyn std::error::Error>> {
    capture_and_solve(camera, initial_guess, &CaptureSettings::default())
}
#[allow(dead_code)]
/// Create initial guess from RA (hours, minutes, seconds) and Dec (degrees, arcmin, arcsec)
pub fn make_initial_guess(
    ra_h: i64, ra_m: i64, ra_s: f64,
    dec_d: i64, dec_m: i64, dec_s: f64,
) -> CoordinateEquatorial {
    CoordinateEquatorial::new(
        RaHoursMinutesSeconds::new(ra_h, ra_m, ra_s),
        Arcdegrees::new(dec_d, dec_m, dec_s),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_initial_guess() {
        let guess = make_initial_guess(14, 3, 26.0, 54, 20, 57.0);
        assert!((guess.ra.to_degrees() - 210.8583).abs() < 0.01);
        assert!((guess.dec.to_degrees() - 54.349).abs() < 0.01);
    }
}
