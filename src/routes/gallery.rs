use actix_web::{get, post, web, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

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
            if !is_image(&ext) {
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
