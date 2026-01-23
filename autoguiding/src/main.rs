mod r#const;
mod find_star;
mod guider;
mod simulator;
mod traits;

use crate::traits::{Camera, MountDriver};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};

use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tower_http::services::ServeFile;

use crate::r#const::{
    CAM_H, CAM_SCALE, CAM_W, MAX_CORRECTION, MAX_DEVIATION, NB_STARS, RMS_WINDOW_SIZE, START_X,
    START_Y, T,
};

// Star finding algorithm selection
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum StarFindingAlgorithm {
    CG,
    GF,
    #[allow(clippy::upper_case_acronyms)]
    #[default]
    FGF,
}

// Commands from UI to guider
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "command")]
enum GuiderCommand {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "set_algorithm")]
    SetAlgorithm { algorithm: StarFindingAlgorithm },
}

// The data packet we send to the browser
#[derive(Clone, Serialize, Debug)]
struct Telemetry {
    ra_error: f64,
    dec_error: f64,
    ra_rms: f64,
    dec_rms: f64,
    total_rms: f64,
    star_cg_x: Option<f64>,
    star_cg_y: Option<f64>,
    star_gf_x: Option<f64>,
    star_gf_y: Option<f64>,
    star_fgf_x: Option<f64>,
    star_fgf_y: Option<f64>,
    selected_star_x: Option<f64>,
    selected_star_y: Option<f64>,
    guide_stars: Vec<(f64, f64)>,
    image_base64: String,
    mount_x: f64,
    mount_y: f64,
    debug_log: String,
    // Algorithm timing (microseconds)
    cg_time: u128,
    gf_time: u128,
    fgf_time: u128,
    // Delta from initial position
    cg_delta: Option<String>,
    gf_delta: Option<String>,
    fgf_delta: Option<String>,
    // Guider state
    is_guiding: bool,
    // Selected algorithm
    selected_algorithm: StarFindingAlgorithm,
}

// Shared app state
#[derive(Clone)]
struct AppState {
    telemetry_tx: broadcast::Sender<Telemetry>,
    command_tx: mpsc::Sender<GuiderCommand>,
}

// --- WEBSOCKET HANDLER ---
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket: WebSocket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.telemetry_tx.subscribe();
    let command_tx = state.command_tx.clone();

    loop {
        tokio::select! {
            // Send telemetry to client
            result = rx.recv() => {
                match result {
                    Ok(telemetry) => {
                        let json_text = serde_json::to_string(&telemetry).unwrap();
                        if socket.send(Message::Text(json_text)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            // Receive commands from client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<GuiderCommand>(&text) {
                            let _ = command_tx.send(cmd).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

/// Reject outlier displacements using median filtering.
/// Returns the filtered average displacement (RA, DEC) in mount pixels.
fn filter_outlier_displacements(displacements: &[(f64, f64)]) -> (f64, f64) {
    if displacements.len() >= 2 {
        // Calculate median displacement
        let mut ra_vals: Vec<f64> = displacements.iter().map(|(ra, _)| *ra).collect();
        let mut dec_vals: Vec<f64> = displacements.iter().map(|(_, dec)| *dec).collect();
        ra_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        dec_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median_ra = ra_vals[ra_vals.len() / 2];
        let median_dec = dec_vals[dec_vals.len() / 2];

        // Filter outliers (> MAX_DEVIATION pixels from median)
        let mut filtered_ra = 0.0;
        let mut filtered_dec = 0.0;
        let mut count = 0;
        for (ra, dec) in displacements {
            if (ra - median_ra).abs() <= MAX_DEVIATION && (dec - median_dec).abs() <= MAX_DEVIATION
            {
                filtered_ra += ra;
                filtered_dec += dec;
                count += 1;
            }
        }

        if count > 0 {
            (filtered_ra / count as f64, filtered_dec / count as f64)
        } else {
            // All stars were outliers, use median as fallback
            (median_ra, median_dec)
        }
    } else if displacements.len() == 1 {
        displacements[0]
    } else {
        (0.0, 0.0)
    }
}

fn guide_mount(
    guider: &mut guider::Guider,
    w: u32,
    h: u32,
    gray_pixels: &[u8],
    tracked_stars: &mut Vec<guider::StarPosition>,
    tracked_original_indices: &mut Vec<usize>,
    algorithm: StarFindingAlgorithm,
) -> (f64, f64) {
    let search_positions = &guider.guide_stars;

    // Track each guide star using the selected algorithm
    for (idx, star) in search_positions.iter().enumerate() {
        let found_star = match algorithm {
            StarFindingAlgorithm::CG => guider.find_star_CG(w, h, gray_pixels, *star),
            StarFindingAlgorithm::GF => guider.find_star_GF(w, h, gray_pixels, *star),
            StarFindingAlgorithm::FGF => guider.find_star_FGF(w, h, gray_pixels, *star, T),
        };
        if let Some(s) = found_star {
            tracked_stars.push(s);
            tracked_original_indices.push(idx);
        }
    }

    // Calculate per-star displacements (in mount pixels)
    let displacements: Vec<(f64, f64)> = tracked_stars
        .iter()
        .enumerate()
        .map(|(i, current_star)| {
            let initial_star = &search_positions[tracked_original_indices[i]];
            let dx = (current_star.x - initial_star.x) / CAM_SCALE;
            let dy = (current_star.y - initial_star.y) / CAM_SCALE;
            (dx, dy)
        })
        .collect();

    // Filter outliers and get average displacement
    let measured_star_displacement = filter_outlier_displacements(&displacements);

    // Calculate correction based on displacement
    let correction = if !tracked_stars.is_empty() {
        let combined_displacement = vec![guider::StarPosition {
            x: measured_star_displacement.0,
            y: measured_star_displacement.1,
        }];
        guider.calculate_correction_multi_star(&combined_displacement)
    } else {
        println!("⚠ Lost all guide stars!");
        (0.0, 0.0)
    };

    correction
}

// --- SIMULATION LOOP ---
async fn run_simulation_loop(
    tx: broadcast::Sender<Telemetry>,
    mut cmd_rx: mpsc::Receiver<GuiderCommand>,
) {
    // Check if image exists to prevent silent crash
    let path = "sky_map.jpg";
    if std::fs::metadata(path).is_err() {
        println!("ERROR: Could not find '{}' in root folder.", path);
        return;
    }

    let mut mount = simulator::SimMount::new(START_X, START_Y);
    let camera = simulator::SimCamera::new(path, CAM_W, CAM_H);
    let mut guider = guider::Guider::new();

    let mut is_guiding = false;
    let mut initial_mount_pos = mount.get_position();
    let mut rms_samples: VecDeque<(f64, f64)> = VecDeque::with_capacity(RMS_WINDOW_SIZE);
    let mut selected_algorithm = StarFindingAlgorithm::default();

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Check for commands (non-blocking)
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        GuiderCommand::Start => {
                            println!("▶ Starting guiding...");
                            // Capture frame and select guide stars
                            let (mx, my) = mount.get_position();
                            let (w, h, gray_pixels) = camera.capture_frame(mx, my);

                            // First, detect candidate stars
                            guider.select_guide_star(w, h, &gray_pixels, NB_STARS);

                            // Then refine positions using the selected algorithm
                            let refined_stars: Vec<guider::StarPosition> = guider.guide_stars
                                .iter()
                                .filter_map(|star| {
                                    match selected_algorithm {
                                        StarFindingAlgorithm::CG => guider.find_star_CG(w, h, &gray_pixels, *star),
                                        StarFindingAlgorithm::GF => guider.find_star_GF(w, h, &gray_pixels, *star),
                                        StarFindingAlgorithm::FGF => guider.find_star_FGF(w, h, &gray_pixels, *star, T),
                                    }
                                })
                                .collect();

                            guider.guide_stars = refined_stars;
                            initial_mount_pos = mount.get_position();
                            rms_samples.clear();
                            is_guiding = true;
                            println!("✓ Guiding started with {} stars", guider.guide_stars.len());
                        }
                        GuiderCommand::Stop => {
                            println!("⏹ Stopping guiding...");
                            is_guiding = false;
                            guider.guide_stars.clear();
                            rms_samples.clear();
                        }
                        GuiderCommand::SetAlgorithm { algorithm } => {
                            println!("🔧 Algorithm changed to {:?}", algorithm);
                            selected_algorithm = algorithm;

                            // If guiding is active, re-select guide stars with the new algorithm
                            // to avoid offset issues from algorithm differences
                            if is_guiding {
                                println!("🔄 Re-selecting guide stars with new algorithm...");
                                let (mx, my) = mount.get_position();
                                let (w, h, gray_pixels) = camera.capture_frame(mx, my);

                                // First, detect candidate stars
                                guider.select_guide_star(w, h, &gray_pixels, NB_STARS);

                                // Then refine positions using the selected algorithm
                                let refined_stars: Vec<guider::StarPosition> = guider.guide_stars
                                    .iter()
                                    .filter_map(|star| {
                                        match algorithm {
                                            StarFindingAlgorithm::CG => guider.find_star_CG(w, h, &gray_pixels, *star),
                                            StarFindingAlgorithm::GF => guider.find_star_GF(w, h, &gray_pixels, *star),
                                            StarFindingAlgorithm::FGF => guider.find_star_FGF(w, h, &gray_pixels, *star, T),
                                        }
                                    })
                                    .collect();

                                guider.guide_stars = refined_stars;
                                initial_mount_pos = mount.get_position();
                                rms_samples.clear();
                                println!("✓ Guide stars re-selected: {} stars", guider.guide_stars.len());
                            }
                        }
                    }
                }

                // Always update mount physics
                mount.update();

                let (mx, my) = mount.get_position();
                let (w, h, gray_pixels) = camera.capture_frame(mx, my);

                // Only track and correct if guiding is active
                let (correction, tracked_stars, time_multi) = if is_guiding && !guider.guide_stars.is_empty() {
                    let mut tracked_stars: Vec<guider::StarPosition> = Vec::new();
                    let mut tracked_original_indices: Vec<usize> = Vec::new();
                    let start_multi = Instant::now();

                    let correction = guide_mount(
                        &mut guider,
                        w,
                        h,
                        &gray_pixels,
                        &mut tracked_stars,
                        &mut tracked_original_indices,
                        selected_algorithm,
                    );

                    // Apply corrections to mount (with outlier rejection)
                    let clamped_correction = (
                        correction.0.clamp(-MAX_CORRECTION, MAX_CORRECTION),
                        correction.1.clamp(-MAX_CORRECTION, MAX_CORRECTION),
                    );

                    if clamped_correction != correction {
                        println!("⚠ Outlier rejected: ({:.2}, {:.2}) → ({:.2}, {:.2})",
                            correction.0, correction.1, clamped_correction.0, clamped_correction.1);
                    }

                    mount.guide_ra(clamped_correction.0);
                    mount.guide_dec(clamped_correction.1);

                    let time_multi = start_multi.elapsed();
                    (correction, tracked_stars, time_multi.as_micros())
                } else {
                    ((0.0, 0.0), Vec::new(), 0)
                };

                let mount_err = (mx - initial_mount_pos.0, my - initial_mount_pos.1);

                // Run comparison algorithms only if we have tracked stars
                let (star_cg, star_gf, star_fgf, time_cg, time_gf, time_fgf) = if !tracked_stars.is_empty() {
                    let start_cg = Instant::now();
                    let star_cg = guider.find_star_CG(w, h, &gray_pixels, tracked_stars[0]);
                    let time_cg = start_cg.elapsed();

                    let start_gf = Instant::now();
                    let star_gf = guider.find_star_GF(w, h, &gray_pixels, tracked_stars[0]);
                    let time_gf = start_gf.elapsed();

                    let start_fgf = Instant::now();
                    let star_fgf = guider.find_star_FGF(w, h, &gray_pixels, tracked_stars[0], T);
                    let time_fgf = start_fgf.elapsed();

                    (star_cg, star_gf, star_fgf, time_cg.as_micros(), time_gf.as_micros(), time_fgf.as_micros())
                } else {
                    (None, None, None, 0, 0, 0)
                };

                // Build debug log
                let mut log = String::new();
                log.push_str(&format!("STATUS: {}\n\n", if is_guiding { "GUIDING" } else { "IDLE" }));
                log.push_str("MOUNT STATUS\n");
                log.push_str(&format!("  Position: ({:.2}, {:.2})\n", mx, my));
                log.push_str(&format!("  Error:    ΔRA={:+.3}  ΔDEC={:+.3}  |Δ|={:.3} px\n",
                    mount_err.0, mount_err.1, (mount_err.0.powi(2) + mount_err.1.powi(2)).sqrt()));

                if is_guiding {
                    log.push_str("\nGUIDING\n");
                    log.push_str(&format!("  Correction: ΔRA={:+.4}  ΔDEC={:+.4}\n", correction.0, correction.1));
                    log.push_str(&format!("  Time taken: {}μs:\n", time_multi));
                    log.push_str(&format!("\nSTAR TRACKING ({}/{} stars)\n", tracked_stars.len(), guider.guide_stars.len()));

                }

                // Update RMS if guiding
                let error_magnitude = (mount_err.0.powi(2) + mount_err.1.powi(2)).sqrt();
                if is_guiding && !tracked_stars.is_empty() && error_magnitude <= 10.0 {
                    if rms_samples.len() >= RMS_WINDOW_SIZE {
                        rms_samples.pop_front();
                    }
                    rms_samples.push_back((mount_err.0, mount_err.1));
                }

                let (ra_rms, dec_rms) = if !rms_samples.is_empty() {
                    let ra_squared_sum: f64 = rms_samples.iter().map(|(ra, _)| ra.powi(2)).sum();
                    let dec_squared_sum: f64 = rms_samples.iter().map(|(_, dec)| dec.powi(2)).sum();
                    let n = rms_samples.len() as f64;
                    ((ra_squared_sum / n).sqrt(), (dec_squared_sum / n).sqrt())
                } else {
                    (0.0, 0.0)
                };
                let total_rms = (ra_rms.powi(2) + dec_rms.powi(2)).sqrt();

                // Encode image
                let mut jpg_buffer = Vec::new();
                let mut encoder = JpegEncoder::new(&mut jpg_buffer);
                encoder.encode(&gray_pixels, w, h, image::ColorType::L8).unwrap();
                let b64_img = base64::engine::general_purpose::STANDARD.encode(&jpg_buffer);

                // Calculate deltas
                let cg_delta = star_cg.and_then(|cg| {
                    guider.guide_stars.first().map(|init| {
                        format!("({:+.2}, {:+.2})", cg.x - init.x, cg.y - init.y)
                    })
                });
                let gf_delta = star_gf.and_then(|gf| {
                    guider.guide_stars.first().map(|init| {
                        format!("({:+.2}, {:+.2})", gf.x - init.x, gf.y - init.y)
                    })
                });
                let fgf_delta = star_fgf.and_then(|fgf| {
                    guider.guide_stars.first().map(|init| {
                        format!("({:+.2}, {:+.2})", fgf.x - init.x, fgf.y - init.y)
                    })
                });

                let _ = tx.send(Telemetry {
                    ra_error: mount_err.0,
                    dec_error: mount_err.1,
                    ra_rms,
                    dec_rms,
                    total_rms,
                    star_cg_x: star_cg.map(|s| s.x),
                    star_cg_y: star_cg.map(|s| s.y),
                    star_gf_x: star_gf.map(|s| s.x),
                    star_gf_y: star_gf.map(|s| s.y),
                    star_fgf_x: star_fgf.map(|s| s.x),
                    star_fgf_y: star_fgf.map(|s| s.y),
                    selected_star_x: guider.guide_stars.first().map(|s| s.x),
                    selected_star_y: guider.guide_stars.first().map(|s| s.y),
                    guide_stars: guider.guide_stars.iter().map(|s| (s.x, s.y)).collect(),
                    image_base64: b64_img,
                    mount_x: mx,
                    mount_y: my,
                    debug_log: log,
                    cg_time: time_cg,
                    gf_time: time_gf,
                    fgf_time: time_fgf,
                    cg_delta,
                    gf_delta,
                    fgf_delta,
                    is_guiding,
                    selected_algorithm,
                });
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // Create channels
    let (telemetry_tx, _rx) = broadcast::channel::<Telemetry>(16);
    let (command_tx, command_rx) = mpsc::channel::<GuiderCommand>(16);

    let telemetry_tx_clone = telemetry_tx.clone();

    // Spawn the Simulation Loop
    tokio::spawn(async move {
        run_simulation_loop(telemetry_tx_clone, command_rx).await;
    });

    // Setup Web Server with AppState
    let state = AppState {
        telemetry_tx,
        command_tx,
    };

    let app = Router::new()
        .route_service("/", ServeFile::new("index.html"))
        .route_service("/sky_map.jpg", ServeFile::new("sky_map.jpg"))
        .route("/ws", get(ws_handler))
        .with_state(state);

    println!("Server running at http://127.0.0.1:3000");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}
