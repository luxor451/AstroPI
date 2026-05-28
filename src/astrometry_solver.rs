//! Plate solving via the `astrometry` Python package (astrometry.net).
//!
//! Delegates star extraction and solving to `scripts/raw_tools.py solve`,
//! which calls `astrometry.Solver` under the hood. This avoids shipping
//! the full astrometry.net C code in Rust and lets us reuse the existing
//! Python/rawpy star-extraction pipeline.

use std::path::{Path, PathBuf};
use std::time::Instant;

use astro_pi_plate_solving::{CameraConfig, CoordinateEquatorial, PlateSolvingResult};

/// Locate `scripts/raw_tools.py` robustly.
///
/// Checks (in order):
///  1. `$ASTROPI_SCRIPTS/raw_tools.py`  — explicit env-var override
///  2. `<binary_dir>/scripts/raw_tools.py` — production: binary next to scripts/
///  3. `<binary_dir>/../scripts/raw_tools.py` — dev: binary inside target/…/
///  4. `scripts/raw_tools.py` relative to CWD — fallback (matches the rest of
///     the codebase which also uses CWD-relative paths for imgs/, fits/, etc.)
/// Convert a RAW file (CR3 / DNG / NEF …) to JPEG using rawpy's embedded-thumbnail
/// fast path — bypasses dnglab entirely.  ~50–800 ms vs 10–30 s with dnglab.
/// Returns the pixel-scale factor: (sensor_width / jpeg_width).
/// 1.0 means the JPEG is full sensor resolution; 4.16 means a 1616px thumbnail
/// from a 6720px sensor.  Callers that don't need it can use `map(|_| ())`.
pub fn raw_to_jpeg_fast(
    raw_path: &std::path::Path,
    jpeg_path: &std::path::Path,
) -> Result<f64, String> {
    let script = find_raw_tools();
    let out = std::process::Command::new("python3")
        .args([
            script.to_str().unwrap_or("scripts/raw_tools.py"),
            "snap_jpeg",
            raw_path.to_str().unwrap_or(""),
            jpeg_path.to_str().unwrap_or(""),
        ])
        .output()
        .map_err(|e| format!("Failed to spawn snap_jpeg: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("snap_jpeg failed: {stderr}"));
    }
    // Read the sidecar Python wrote, then remove it (we return the value directly).
    let sidecar = format!("{}.scale_factor", jpeg_path.display());
    let factor: f64 = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| {
            eprintln!("[raw_to_jpeg_fast] scale_factor sidecar not found at {sidecar}");
            1.0
        });
    let _ = std::fs::remove_file(&sidecar); // clean up; caller stores the value
    Ok(factor)
}

pub fn find_raw_tools() -> PathBuf {
    // 1. Explicit override via environment variable
    if let Ok(dir) = std::env::var("ASTROPI_SCRIPTS") {
        let p = PathBuf::from(dir).join("raw_tools.py");
        if p.exists() {
            return p;
        }
    }

    // 2 & 3. Search relative to the running binary
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(3) {
            let p = ancestor.join("scripts").join("raw_tools.py");
            if p.exists() {
                return p;
            }
        }
    }

    // 4. CWD fallback
    PathBuf::from("scripts").join("raw_tools.py")
}

/// Solve a RAW image using the astrometry.net Python package.
///
/// # Arguments
/// * `image_path`    - Path to the RAW image file (CR3 / DNG / FITS).
/// * `initial_guess` - Approximate RA/Dec as a starting hint.
/// * `cam_config`    - Camera parameters (pixel size, focal length) used to
///                     derive the expected pixel scale range.
///
/// # Returns
/// A `PlateSolvingResult`. Check `result.coeffs_x.is_some()` for success.
/// On success, `optical_axis_ra` and `optical_axis_dec` are in **radians**.
pub fn solve_with_astrometry(
    image_path: &Path,
    initial_guess: &CoordinateEquatorial,
    cam_config: &CameraConfig,
) -> Result<PlateSolvingResult, Box<dyn std::error::Error>> {
    let t0 = Instant::now();

    let ra_deg = initial_guess.ra.to_hours() * 15.0; // hours → degrees
    let dec_deg = initial_guess.dec.to_degrees();

    // Build a ±60 % pixel-scale search window around the expected scale.
    let expected_scale = cam_config.expected_pixel_scale(); // arcsec / pixel
    let scale_low = (expected_scale * 0.40).max(0.1);
    let scale_high = expected_scale * 1.60;

    // Search within 15° of the mount position (generous for a blind solve
    // from a known approximate position).
    let radius_deg = 15.0_f64;

    let image_str = image_path
        .to_str()
        .ok_or("image path contains non-UTF8 characters")?;

    println!(
        "[astrometry] Solving {image_str}  RA={ra_deg:.3}° Dec={dec_deg:.3}°  \
         scale={scale_low:.2}–{scale_high:.2} arcsec/pix  radius={radius_deg}°"
    );

    let script = find_raw_tools();
    let script_str = script.to_str().unwrap_or("scripts/raw_tools.py");
    println!("[astrometry] using script: {script_str}");

    let output = std::process::Command::new("python3")
        .args([
            script_str,
            "solve",
            image_str,
            &format!("{ra_deg:.6}"),
            &format!("{dec_deg:.6}"),
            &format!("{scale_low:.4}"),
            &format!("{scale_high:.4}"),
            &format!("{radius_deg:.1}"),
        ])
        .output()
        .map_err(|e| format!("Failed to spawn raw_tools.py: {e}"))?;

    let elapsed = t0.elapsed().as_secs_f64();

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Print but don't treat as fatal – astrometry.net writes progress to stderr.
        eprintln!("[astrometry] stderr: {stderr}");
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "raw_tools.py exited with status {}: {stderr}",
            output.status
        )
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();

    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| format!("Failed to parse solver JSON output `{stdout}`: {e}"))?;

    if !parsed["success"].as_bool().unwrap_or(false) {
        let error = parsed["error"].as_str().unwrap_or("unknown error");
        println!("[astrometry] No solution: {error}  ({elapsed:.1}s)");
        // Return a failed (no match) result.
        return Ok(PlateSolvingResult {
            matched_quads_count: 0,
            coeffs_x: None,
            coeffs_y: None,
            transform: None,
            optical_axis_ra: 0.0,
            optical_axis_dec: 0.0,
            spiral_iterations: 0,
            solution_ra_deg: ra_deg,
            solution_dec_deg: dec_deg,
            rotation_deg: 0.0,
            scale_arcsec_per_pixel: 0.0,
            is_mirrored: false,
        });
    }

    let ra_solved_deg = parsed["ra_deg"]
        .as_f64()
        .ok_or("missing ra_deg in solver output")?;
    let dec_solved_deg = parsed["dec_deg"]
        .as_f64()
        .ok_or("missing dec_deg in solver output")?;
    let scale = parsed["scale_arcsec_per_pixel"]
        .as_f64()
        .unwrap_or(expected_scale);
    let logodds = parsed["logodds"].as_f64().unwrap_or(0.0);

    let ra_rad = ra_solved_deg.to_radians();
    let dec_rad = dec_solved_deg.to_radians();

    println!(
        "[astrometry] Solution: RA={ra_solved_deg:.4}° Dec={dec_solved_deg:.4}°  \
         scale={scale:.3} arcsec/pix  logodds={logodds:.1}  ({elapsed:.1}s)"
    );

    // Use a non-None coeffs_x to signal success to callers.
    Ok(PlateSolvingResult {
        matched_quads_count: logodds as usize,
        coeffs_x: Some((1.0, 0.0, 0.0)),
        coeffs_y: Some((0.0, 1.0, 0.0)),
        transform: None,
        optical_axis_ra: ra_rad,
        optical_axis_dec: dec_rad,
        spiral_iterations: 0,
        solution_ra_deg: ra_solved_deg,
        solution_dec_deg: dec_solved_deg,
        rotation_deg: 0.0,
        scale_arcsec_per_pixel: scale,
        is_mirrored: false,
    })
}
