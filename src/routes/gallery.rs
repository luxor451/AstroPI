use actix_web::{get, post, web, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use crate::state::AppState;

// ── helpers ──────────────────────────────────────────────────────────────────

const CAPTURES_ROOT: &str = "imgs/astro_captures";
const THUMB_CACHE:   &str = "imgs/.thumbnails";
const FITS_DIR:      &str = "imgs/fits";
const RAW_TOOLS:     &str = "scripts/raw_tools.py";

fn raw_ext(ext: &str) -> bool {
    matches!(ext, "cr3" | "cr2" | "nef" | "arw" | "dng" | "raf" | "orf")
}

fn is_image(ext: &str) -> bool {
    raw_ext(ext) || matches!(ext, "fits" | "fit" | "jpg" | "jpeg" | "png" | "tiff" | "tif")
}

/// Run `python3 scripts/raw_tools.py <args>` and return success/stderr.
fn run_raw_tool(args: &[&str]) -> Result<String, String> {
    let out = Command::new("python3")
        .arg(RAW_TOOLS)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to spawn python3: {e}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Ensure parent directories exist.
fn ensure_dir(p: &Path) -> std::io::Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
    } else {
        Ok(())
    }
}

// ── data types ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct GalleryFile {
    pub name:        String,
    /// Path relative to CAPTURES_ROOT (or FITS_DIR for .fits files).
    pub rel_path:    String,
    pub size_bytes:  u64,
    pub modified_ms: u64,
    pub kind:        String,   // "raw" | "fits" | "jpeg"
    pub fits_exists: bool,
}

#[derive(Serialize)]
pub struct GalleryFolder {
    pub name:    String,
    pub rel_path: String,
    pub files:   Vec<GalleryFile>,
    pub folders: Vec<GalleryFolder>,
}

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: String,
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub path:    String,
    /// Low percentile for stretch (default 0.1)
    pub lo:      Option<f64>,
    /// High percentile for stretch (default 99.9)
    pub hi:      Option<f64>,
}

#[derive(Deserialize)]
pub struct ConvertPayload {
    pub path: String,
}

#[derive(Serialize)]
pub struct ConvertResponse {
    pub fits_path: String,
    pub message:   String,
}

// ── file listing ─────────────────────────────────────────────────────────────

fn build_folder(abs_path: &Path, base: &Path) -> GalleryFolder {
    let name = abs_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let rel_path = abs_path
        .strip_prefix(base)
        .unwrap_or(abs_path)
        .to_string_lossy()
        .replace('\\', "/");

    let mut files   = Vec::new();
    let mut folders = Vec::new();

    let Ok(entries) = std::fs::read_dir(abs_path) else {
        return GalleryFolder { name, rel_path, files, folders };
    };

    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().into_owned();

        // skip hidden / thumbnail cache
        if fname.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            folders.push(build_folder(&path, base));
        } else if path.is_file() {
            let ext = path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if !is_image(&ext) || raw_ext(&ext) {
                continue;
            }
            let meta = std::fs::metadata(&path).ok();
            let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified_ms = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let kind = if raw_ext(&ext) {
                "raw"
            } else if matches!(ext.as_str(), "fits" | "fit") {
                "fits"
            } else if matches!(ext.as_str(), "tiff" | "tif") {
                "tiff"
            } else {
                "jpeg"
            };

            // check if a .fits counterpart exists
            let fits_exists = if raw_ext(&ext) {
                let fits_name = path.file_stem().unwrap_or_default().to_string_lossy();
                PathBuf::from(FITS_DIR).join(format!("{fits_name}.fits")).exists()
            } else {
                false
            };

            let file_rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            files.push(GalleryFile {
                name: fname,
                rel_path: file_rel,
                size_bytes,
                modified_ms,
                kind: kind.to_owned(),
                fits_exists,
            });
        }
    }

    GalleryFolder { name, rel_path, files, folders }
}

// ── routes ───────────────────────────────────────────────────────────────────

/// GET /gallery/files — returns the full folder tree under imgs/astro_captures/
#[get("/gallery/files")]
pub async fn gallery_files() -> impl Responder {
    let root = PathBuf::from(CAPTURES_ROOT);
    if !root.exists() {
        return HttpResponse::Ok().json(GalleryFolder {
            name:     "astro_captures".into(),
            rel_path: "".into(),
            files:    vec![],
            folders:  vec![],
        });
    }
    let tree = build_folder(&root, &root);
    HttpResponse::Ok().json(tree)
}

/// GET /gallery/thumbnail?path=… — JPEG thumbnail (cached in imgs/.thumbnails/)
#[get("/gallery/thumbnail")]
pub async fn gallery_thumbnail(query: web::Query<PathQuery>) -> impl Responder {
    // Sanitise path: must stay within CAPTURES_ROOT
    let rel = query.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(CAPTURES_ROOT).join(&rel);

    if !abs.exists() {
        return HttpResponse::NotFound().body("File not found");
    }

    // Cache key: flat filename under .thumbnails/
    let cache_name = rel.replace(['/', '\\'], "_") + ".jpg";
    let cache_path = PathBuf::from(THUMB_CACHE).join(&cache_name);

    if !cache_path.exists() {
        if let Err(e) = ensure_dir(&cache_path) {
            return HttpResponse::InternalServerError().body(e.to_string());
        }
        if let Err(e) = run_raw_tool(&[
            "thumbnail",
            abs.to_str().unwrap_or(""),
            cache_path.to_str().unwrap_or(""),
        ]) {
            return HttpResponse::InternalServerError().body(format!("Thumbnail error: {e}"));
        }
    }

    match std::fs::read(&cache_path) {
        Ok(data) => HttpResponse::Ok()
            .content_type("image/jpeg")
            .body(data),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// GET /gallery/preview?path=…&lo=0.1&hi=99.9 — full-res stretched JPEG
#[get("/gallery/preview")]
pub async fn gallery_preview(query: web::Query<PreviewQuery>) -> impl Responder {
    let rel = query.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(CAPTURES_ROOT).join(&rel);

    // Also allow fetching from FITS_DIR
    let abs = if abs.exists() {
        abs
    } else {
        let alt = PathBuf::from(FITS_DIR).join(&rel);
        if alt.exists() { alt } else {
            return HttpResponse::NotFound().body("File not found");
        }
    };

    let lo = query.lo.unwrap_or(0.1).to_string();
    let hi = query.hi.unwrap_or(99.9).to_string();

    // Write to a temp file
    let tmp = std::env::temp_dir().join(format!(
        "astropi_preview_{}.jpg",
        abs.file_stem().unwrap_or_default().to_string_lossy()
    ));

    if let Err(e) = run_raw_tool(&[
        "preview",
        abs.to_str().unwrap_or(""),
        tmp.to_str().unwrap_or(""),
        &lo,
        &hi,
    ]) {
        return HttpResponse::InternalServerError().body(format!("Preview error: {e}"));
    }

    match std::fs::read(&tmp) {
        Ok(data) => HttpResponse::Ok()
            .content_type("image/jpeg")
            .body(data),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// POST /gallery/convert_fits  body: {"path":"lights/M1_0001.cr3"}
#[post("/gallery/convert_fits")]
pub async fn gallery_convert_fits(body: web::Json<ConvertPayload>) -> impl Responder {
    let rel = body.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(CAPTURES_ROOT).join(&rel);

    if !abs.exists() {
        return HttpResponse::NotFound().body("Source file not found");
    }

    let stem = abs
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let fits_path = PathBuf::from(FITS_DIR).join(format!("{stem}.fits"));

    if let Err(e) = std::fs::create_dir_all(FITS_DIR) {
        return HttpResponse::InternalServerError()
            .body(format!("Cannot create fits dir: {e}"));
    }

    if fits_path.exists() {
        return HttpResponse::Ok().json(ConvertResponse {
            fits_path: format!("{stem}.fits"),
            message:   "Already converted".into(),
        });
    }

    match run_raw_tool(&[
        "fits",
        abs.to_str().unwrap_or(""),
        fits_path.to_str().unwrap_or(""),
    ]) {
        Ok(_) => HttpResponse::Ok().json(ConvertResponse {
            fits_path: format!("{stem}.fits"),
            message:   "Converted successfully".into(),
        }),
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Conversion failed: {e}")),
    }
}

/// GET /gallery/fits_header?path=… — returns FITS header as JSON
#[get("/gallery/fits_header")]
pub async fn gallery_fits_header(query: web::Query<PathQuery>) -> impl Responder {
    let rel = query.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(FITS_DIR).join(&rel);

    if !abs.exists() {
        return HttpResponse::NotFound().body("FITS file not found");
    }

    match run_raw_tool(&["fitshdr", abs.to_str().unwrap_or("")]) {
        Ok(json_str) => HttpResponse::Ok()
            .content_type("application/json")
            .body(json_str),
        Err(e) => HttpResponse::InternalServerError().body(e),
    }
}

/// GET /gallery/download?path=… — stream a file (FITS or original RAW)
#[get("/gallery/download")]
pub async fn gallery_download(query: web::Query<PathQuery>) -> impl Responder {
    let rel = query.path.trim_start_matches('/').replace("..", "");
    let abs = if rel.ends_with(".fits") || rel.ends_with(".fit") {
        PathBuf::from(FITS_DIR).join(&rel)
    } else {
        PathBuf::from(CAPTURES_ROOT).join(&rel)
    };

    if !abs.exists() {
        return HttpResponse::NotFound().body("File not found");
    }

    let fname = abs
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let content_type = if fname.ends_with(".fits") || fname.ends_with(".fit") {
        "application/fits"
    } else {
        "application/octet-stream"
    };

    match std::fs::read(&abs) {
        Ok(data) => HttpResponse::Ok()
            .content_type(content_type)
            .append_header(("Content-Disposition", format!("attachment; filename=\"{fname}\"")))
            .body(data),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

// ── delete ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DeletePayload {
    pub path: String,
}

/// POST /gallery/delete  body: {"path":"lights/M31_0001.tif"}
#[post("/gallery/delete")]
pub async fn gallery_delete(body: web::Json<DeletePayload>) -> impl Responder {
    let rel = body.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(CAPTURES_ROOT).join(&rel);

    if !abs.exists() {
        return HttpResponse::NotFound().body("File not found");
    }

    // Remove thumbnail cache entry so it doesn't show stale data.
    let cache_name = rel.replace(['/', '\\'], "_") + ".jpg";
    let _ = std::fs::remove_file(PathBuf::from(THUMB_CACHE).join(cache_name));

    match std::fs::remove_file(&abs) {
        Ok(_)  => HttpResponse::Ok().body("Deleted"),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

// ── plate-solve ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PlateSolvePayload {
    pub path: String,
}

#[derive(Serialize)]
pub struct PlateSolveResponse {
    pub success:                bool,
    pub ra_deg:                 Option<f64>,
    pub dec_deg:                Option<f64>,
    pub scale_arcsec_per_pixel: Option<f64>,
    pub message:                String,
}

/// Parse RA / DEC / FOCAL / XPIXSZ out of the JSON produced by `cmd_tiffhdr`.
fn parse_tiffhdr_meta(json_str: &str) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    let v: serde_json::Value = serde_json::from_str(json_str).unwrap_or_default();
    let ra  = v["RA"]    .as_str().and_then(|s| s.trim().parse().ok());
    let dec = v["DEC"]   .as_str().and_then(|s| s.trim().parse().ok());
    let px  = v["XPIXSZ"].as_str().and_then(|s| s.trim().parse().ok());
    let fo  = v["FOCAL"] .as_str().and_then(|s| s.trim().parse().ok());
    (ra, dec, px, fo)
}

/// POST /gallery/platesolve  body: {"path":"lights/M31_0001.tif"}
///
/// Plate-solves the image using the embedded FITS metadata as a hint.
/// Returns the WCS centre in RA/Dec degrees and the pixel scale.
/// The solve runs in a blocking thread — expect 30–120 s on a Raspberry Pi.
#[post("/gallery/platesolve")]
pub async fn gallery_platesolve(
    body: web::Json<PlateSolvePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    let rel = body.path.trim_start_matches('/').replace("..", "");
    let abs = PathBuf::from(CAPTURES_ROOT).join(&rel);

    if !abs.exists() {
        return HttpResponse::Ok().json(PlateSolveResponse {
            success: false, ra_deg: None, dec_deg: None,
            scale_arcsec_per_pixel: None, message: "File not found".into(),
        });
    }

    // Camera settings — used as fallback when TIFF has no embedded metadata.
    let (pixel_size_um, focal_mm) = {
        let s = data.camera_settings.lock().await;
        (s.pixel_size_micron, s.focal_length_mm)
    };

    // Mount coordinates — used as position hint when TIFF has no RA/Dec.
    let (mount_ra_deg, mount_dec_deg) = {
        let c = data.indi_client.read().await;
        if let Some(client) = c.as_ref() {
            let (ra_h, dec) = client.get_coordinates().await;
            (ra_h * 15.0, dec)
        } else {
            (0.0, 0.0)
        }
    };

    let abs_str     = abs.to_str().unwrap_or("").to_string();
    let event_tx    = data.event_sender.clone();

    let solve_out = tokio::task::spawn_blocking(move || {
        // Read FITS metadata embedded in the TIFF's ImageDescription.
        let (ra_hint, dec_hint, px_hint, fo_hint) =
            match run_raw_tool(&["tiffhdr", &abs_str]) {
                Ok(ref js) => parse_tiffhdr_meta(js),
                Err(_)     => (None, None, None, None),
            };

        let ra_deg  = ra_hint .unwrap_or(mount_ra_deg);
        let dec_deg = dec_hint.unwrap_or(mount_dec_deg);
        let px      = px_hint .unwrap_or(pixel_size_um);
        let fo      = fo_hint .unwrap_or(focal_mm).max(1.0);

        let scale       = (206.265 * px) / fo;
        let scale_lo    = format!("{:.4}", scale * 0.7);
        let scale_hi    = format!("{:.4}", scale * 1.3);
        let ra_s        = format!("{}", ra_deg);
        let dec_s       = format!("{}", dec_deg);

        let _ = event_tx.send(format!(
            "Plate solving {} (hint RA={:.2}° Dec={:.2}°, ~{:.2}\"/px)...",
            abs_str, ra_deg, dec_deg, scale,
        ));

        run_raw_tool(&["solve", &abs_str, &ra_s, &dec_s, &scale_lo, &scale_hi])
    })
    .await;

    // Helper to build a failure response — always JSON so the frontend can res.json() safely.
    let fail = |msg: String| {
        let _ = data.event_sender.send(format!("Plate solve failed: {}", msg));
        HttpResponse::Ok().json(PlateSolveResponse {
            success: false, ra_deg: None, dec_deg: None,
            scale_arcsec_per_pixel: None, message: msg,
        })
    };

    match solve_out {
        Ok(Ok(ref json_str)) if !json_str.trim().is_empty() => {
            match serde_json::from_str::<serde_json::Value>(json_str) {
                Ok(v) => {
                    let success = v["success"].as_bool().unwrap_or(false);
                    let ra_deg  = v["ra_deg"] .as_f64();
                    let dec_deg = v["dec_deg"].as_f64();
                    let scale   = v["scale_arcsec_per_pixel"].as_f64();
                    let message = if success {
                        let m = format!(
                            "Solved: RA {:.4}°  Dec {:.4}°  {:.3}\"/px",
                            ra_deg.unwrap_or(0.0), dec_deg.unwrap_or(0.0), scale.unwrap_or(0.0),
                        );
                        let _ = data.event_sender.send(m.clone());
                        m
                    } else {
                        v["error"].as_str().unwrap_or("No solution found").to_string()
                    };
                    HttpResponse::Ok().json(PlateSolveResponse {
                        success, ra_deg, dec_deg, scale_arcsec_per_pixel: scale, message,
                    })
                }
                Err(e) => fail(format!("Bad solver output: {e} — raw: {}", json_str.chars().take(200).collect::<String>())),
            }
        }
        Ok(Ok(_empty)) => fail("Solver produced no output (check Python / astrometry install)".into()),
        Ok(Err(e))     => fail(e),
        Err(e)         => fail(format!("Spawning task failed: {e}")),
    }
}
