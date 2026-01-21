mod traits;
mod simulator;
mod guider;
mod find_star;


use axum::{
    extract::{State, ws::{Message, WebSocket, WebSocketUpgrade}},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::time::Instant;
use tokio::sync::broadcast;
use crate::traits::{MountDriver, Camera};
use image::codecs::jpeg::JpegEncoder;
use serde::Serialize;
use tower_http::services::ServeFile; // Ensure "fs" feature is in Cargo.toml
use base64::Engine as _;

// --- CONFIG ---
const CAM_W: u32 = 640;
const CAM_H: u32 = 480;
const T : u32 = 4; // Threshold for Fast Gaussian Fit - tighter for more stable centroids
const NB_STARS: usize = 10; // Number of guide stars to use for multi-star guiding

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
    guide_stars: Vec<(f64, f64)>,  // All 5 guide stars for UI display
    image_base64: String,
    mount_x: f64,
    mount_y: f64,
}

#[tokio::main]
async fn main() {
    // 1. Create a Broadcast Channel
    let (tx, _rx) = broadcast::channel::<Telemetry>(16);
    let tx_clone = tx.clone();

    // 2. Spawn the Simulation Loop
    tokio::spawn(async move {
        run_simulation_loop(tx_clone).await;
    });

    // 3. Setup Web Server
    // FIX: Use 'route_service' for the file, and 'get' for the websocket
    let app = Router::new()
        .route_service("/", ServeFile::new("index.html")) // <--- FIXED LINE
        .route_service("/sky_map.jpg", ServeFile::new("sky_map.jpg")) // Serve sky map
        .route("/ws", get(ws_handler))
        .with_state(tx);

    println!("Server running at http://127.0.0.1:3000");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- WEBSOCKET HANDLER ---
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(tx): State<broadcast::Sender<Telemetry>>, 
) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let mut rx = tx.subscribe();
        while let Ok(telemetry) = rx.recv().await {
            let json_text = serde_json::to_string(&telemetry).unwrap();
            if socket.send(Message::Text(json_text)).await.is_err() {
                break;
            }
        }
    })
}

// --- SIMULATION LOOP ---
async fn run_simulation_loop(tx: broadcast::Sender<Telemetry>) {
    // Check if image exists to prevent silent crash
    let path = "sky_map.jpg";
    if std::fs::metadata(path).is_err() {
        println!("ERROR: Could not find '{}' in root folder.", path);
        return;
    }

    let mut mount = simulator::SimMount::new(1500.0, 1500.0);
    let camera = simulator::SimCamera::new(path, CAM_W, CAM_H);
    let mut guider = guider::Guider::new();
    
    // Capture initial frame BEFORE any mount movement and select guide stars
    let (mx, my) = mount.get_position();
    let (w, h, gray_pixels) = camera.capture_frame(mx, my);
    guider.select_guide_star(w, h, &gray_pixels, NB_STARS);
    
    // Store initial mount position as reference (after selection, before any movement)
    let initial_mount_pos = mount.get_position();
    println!("📍 Reference mount position: ({:.2}, {:.2})", initial_mount_pos.0, initial_mount_pos.1);
    
    // RMS tracking
    let mut ra_squared_sum = 0.0;
    let mut dec_squared_sum = 0.0;
    let mut rms_sample_count = 0;

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

    loop {
        interval.tick().await;
        
        // 1. Update mount physics
        mount.update();

        // 2. Get current mount position and capture new frame
        let (mx, my) = mount.get_position();
        let (w, h, gray_pixels) = camera.capture_frame(mx, my);

        // 3. Calculate mount error from reference position
        let mount_err = (mx - initial_mount_pos.0, my - initial_mount_pos.1);
        
        // Expected star displacement in frame should be opposite to mount error
        // If mount moves right (+x), stars appear to move left (-x) in frame
        let expected_star_displacement = (-mount_err.0, -mount_err.1);
        
        // 4. Track all guide stars using FGF (multi-star guiding)
        let mut tracked_stars: Vec<guider::StarPosition> = Vec::new();
        let mut tracked_original_indices: Vec<usize> = Vec::new(); // Track which ORIGINAL stars were successfully tracked
        let start_multi = Instant::now();
        
        // Track from initial positions to measure displacement accurately
        let search_positions = &guider.guide_stars;
        let search_indices: Vec<usize> = (0..guider.guide_stars.len()).collect();
        
        // Try to track each star from its initial position
        for (idx, pos) in search_positions.iter().enumerate() {
            if let Some(s) = guider.find_star_FGF(w, h, &gray_pixels, *pos, T) {
                tracked_stars.push(s);
                tracked_original_indices.push(search_indices[idx]);
            } else {
                println!("⚠ Lost guide star!");
            }
        }

        // Calculate displacement for each tracked star
        let all_star_displacements: Vec<guider::StarPosition> = tracked_stars.iter()
            .enumerate()
            .map(|(i, current_star)| {
                let original_idx = tracked_original_indices[i];
                let initial_star = &guider.guide_stars[original_idx];
                guider::StarPosition {
                    x: current_star.x - initial_star.x,
                    y: current_star.y - initial_star.y,
                }
            })
            .collect();
        
        // RA: Use ALL stars (no filtering - RA was working fine)
        let ra_displacement = if !all_star_displacements.is_empty() {
            all_star_displacements.iter().map(|s| s.x).sum::<f64>() / all_star_displacements.len() as f64
        } else {
            0.0
        };
        
        // DEC: Use MEDIAN to be robust against outliers
        let dec_displacement = if !all_star_displacements.is_empty() {
            let mut y_vals: Vec<f64> = all_star_displacements.iter().map(|s| s.y).collect();
            y_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            y_vals[y_vals.len() / 2]  // Use median instead of mean for robustness
        } else {
            0.0
        };
        
        // Count outliers for display
        let dec_outliers = if all_star_displacements.len() >= 3 {
            let mut y_vals: Vec<f64> = all_star_displacements.iter().map(|s| s.y).collect();
            y_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median_y = y_vals[y_vals.len() / 2];
            let mut y_deviations: Vec<f64> = y_vals.iter().map(|y| (y - median_y).abs()).collect();
            y_deviations.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mad_y = y_deviations[y_deviations.len() / 2].max(0.1);
            all_star_displacements.iter().filter(|s| (s.y - median_y).abs() > 3.0 * mad_y).count()
        } else {
            0
        };
        
        if dec_outliers > 0 {
            println!("  ⚠ {} DEC outlier star(s) excluded via median", dec_outliers);
        }
        
        // Calculate averages for display (using all stars for consistency)
        let (tracked_stars_average, initial_tracked_stars_average) = if !tracked_stars.is_empty() {
            let sum_x: f64 = tracked_stars.iter().map(|s| s.x).sum();
            let sum_y: f64 = tracked_stars.iter().map(|s| s.y).sum();
            let tracked_avg = guider::StarPosition {
                x: sum_x / tracked_stars.len() as f64,
                y: sum_y / tracked_stars.len() as f64,
            };
            
            let init_sum_x: f64 = tracked_original_indices.iter().map(|&i| guider.guide_stars[i].x).sum();
            let init_sum_y: f64 = tracked_original_indices.iter().map(|&i| guider.guide_stars[i].y).sum();
            let initial_avg = guider::StarPosition {
                x: init_sum_x / tracked_original_indices.len() as f64,
                y: init_sum_y / tracked_original_indices.len() as f64,
            };
            
            (tracked_avg, initial_avg)
        } else {
            (guider::StarPosition { x: mx, y: my }, guider::StarPosition { x: 0.0, y: 0.0 })
        };

        // Measured star displacement: RA uses mean, DEC uses median
        let measured_star_displacement = (ra_displacement, dec_displacement);
        
        // Guider tracking error: difference between measured and expected star displacement
        let tracking_error = (
            measured_star_displacement.0 - expected_star_displacement.0,
            measured_star_displacement.1 - expected_star_displacement.1,
        );


        // Create displacement for PID: RA=mean of all, DEC=median
        let correction = if !all_star_displacements.is_empty() {
            let combined_displacement = vec![guider::StarPosition {
                x: ra_displacement,
                y: dec_displacement,
            }];
            guider.calculate_correction_multi_star(&combined_displacement)
        } else {
            println!("⚠ Lost all guide stars!");
            (0.0, 0.0)
        };


        
        // 4. Apply corrections to mount (using multi-star averaged correction)

        

        mount.guide_ra(correction.0);
        mount.guide_dec(correction.1);

        
        let time_multi = start_multi.elapsed();
        
        
        // Also run CG and GF on primary star for comparison
        let start_cg = Instant::now();
        let star_cg = guider.find_star_CG(w, h, &gray_pixels, tracked_stars[0]);
        let time_cg = start_cg.elapsed();
        
        let start_gf = Instant::now();
        let star_gf = guider.find_star_GF(w, h, &gray_pixels, tracked_stars[0]);
        let time_gf = start_gf.elapsed();
        
        // FGF on primary star for UI display
        let start_fgf = Instant::now();
        let star_fgf = guider.find_star_FGF(w, h, &gray_pixels, tracked_stars[0], T);
        let time_fgf = start_fgf.elapsed();
    

        
        // Print mount status and guider performance
        println!("═══════════════════════════════════════════════════════════════════════════");
        println!("MOUNT STATUS:");
        println!("  Actual position:      ({:7.2}, {:7.2})", mx, my);
        println!("  Mount error from ref: Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px", 
            mount_err.0, mount_err.1, (mount_err.0.powi(2) + mount_err.1.powi(2)).sqrt());
        
        println!("\nSTAR DISPLACEMENT IN CAMERA FRAME:");
        println!("  Expected (=-mount_err): Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
            expected_star_displacement.0, expected_star_displacement.1, 
            (expected_star_displacement.0.powi(2) + expected_star_displacement.1.powi(2)).sqrt());
        println!("  Measured by camera:     Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
            measured_star_displacement.0, measured_star_displacement.1, 
            (measured_star_displacement.0.powi(2) + measured_star_displacement.1.powi(2)).sqrt());
        println!("Applying correction: ΔRA={:.4}, ΔDEC={:.4}", correction.0, correction.1);
        println!("\nGUIDER TRACKING ACCURACY:");
        println!("  Tracking error:       Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
            tracking_error.0, tracking_error.1, 
            (tracking_error.0.powi(2) + tracking_error.1.powi(2)).sqrt());
        println!("  (Difference between measured and expected star displacement)");
        
        println!("\nSTAR TRACKING ALGORITHMS ({}/{} stars tracked):", 
            tracked_stars.len(), guider.guide_stars.len());
        
        // Multi-star tracking results
        if !tracked_stars.is_empty() {
            println!("  Multi-star [{:4}µs]:", time_multi.as_micros());
            println!("    Tracked avg position:  ({:7.2}, {:7.2})", tracked_stars_average.x, tracked_stars_average.y);
            println!("    Initial avg position:  ({:7.2}, {:7.2}) (average of tracked stars only)", 
                initial_tracked_stars_average.x, initial_tracked_stars_average.y);
        } else {
            println!("  Multi-star [{:4}µs]: NO STARS TRACKED", time_multi.as_micros());
        }
        
        // Print CG results
        if let Some(cg) = star_cg {
            let cg_vs_initial = (cg.x - guider.guide_stars[0].x, cg.y - guider.guide_stars[0].y);
            println!("  CG  [{:4}µs]:", time_cg.as_micros());
            println!("    Position: ({:7.2}, {:7.2})", cg.x, cg.y);
            println!("    Displacement from initial: Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
                cg_vs_initial.0, cg_vs_initial.1, (cg_vs_initial.0.powi(2) + cg_vs_initial.1.powi(2)).sqrt());
        } else {
            println!("  CG  [{:4}µs]: FAILED", time_cg.as_micros());
        }
        
        // Print GF results
        if let Some(gf) = star_gf {
            let gf_vs_initial = (gf.x - guider.guide_stars[0].x, gf.y - guider.guide_stars[0].y);
            println!("  GF  [{:4}µs]:", time_gf.as_micros());
            println!("    Position: ({:7.2}, {:7.2})", gf.x, gf.y);
            println!("    Displacement from initial: Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
                gf_vs_initial.0, gf_vs_initial.1, (gf_vs_initial.0.powi(2) + gf_vs_initial.1.powi(2)).sqrt());
        } else {
            println!("  GF  [{:4}µs]: FAILED", time_gf.as_micros());
        }
        
        // Print FGF results
        if let Some(fgf) = star_fgf {
            let fgf_vs_initial = (fgf.x - guider.guide_stars[0].x, fgf.y - guider.guide_stars[0].y);
            println!("  FGF [{:4}µs]:", time_fgf.as_micros());
            println!("    Position: ({:7.2}, {:7.2})", fgf.x, fgf.y);
            println!("    Displacement from initial: Δx={:7.3}  Δy={:7.3}  |Δ|={:7.3} px",
                fgf_vs_initial.0, fgf_vs_initial.1, (fgf_vs_initial.0.powi(2) + fgf_vs_initial.1.powi(2)).sqrt());
        } else {
            println!("  FGF [{:4}µs]: FAILED", time_fgf.as_micros());
        }
        
        println!();
        
        // Update RMS calculation using measured star displacement
        // Only include samples where tracking is successful and displacement is reasonable
        let displacement_magnitude = (measured_star_displacement.0.powi(2) + measured_star_displacement.1.powi(2)).sqrt();
        if !tracked_stars.is_empty() && displacement_magnitude <= 10.0 {
            ra_squared_sum += measured_star_displacement.0.powi(2);
            dec_squared_sum += measured_star_displacement.1.powi(2);
            rms_sample_count += 1;
        }
        
        let ra_rms = if rms_sample_count > 0 {
            (ra_squared_sum / rms_sample_count as f64).sqrt()
        } else {
            0.0
        };
        let dec_rms = if rms_sample_count > 0 {
            (dec_squared_sum / rms_sample_count as f64).sqrt()
        } else {
            0.0
        };
        let total_rms = (ra_rms.powi(2) + dec_rms.powi(2)).sqrt();

        // 5. Encode image for transmission
        let mut jpg_buffer = Vec::new();
        let mut encoder = JpegEncoder::new(&mut jpg_buffer);
        encoder.encode(&gray_pixels, w, h, image::ColorType::L8).unwrap();
        let b64_img = base64::engine::general_purpose::STANDARD.encode(&jpg_buffer);

        // 6. Broadcast telemetry (all three algorithm results for UI, FGF for tracking)
        // Send displacement from initial position to UI (what user wants to see)
        let _ = tx.send(Telemetry {
            ra_error: mount_err.0,
            dec_error: mount_err.1,
            ra_rms,
            dec_rms,
            total_rms,
            star_cg_x: star_cg.map(|s| s.x),             // CG result for display
            star_cg_y: star_cg.map(|s| s.y),
            star_gf_x: star_gf.map(|s| s.x),             // GF result for display
            star_gf_y: star_gf.map(|s| s.y),
            star_fgf_x: star_fgf.map(|s| s.x),           // FGF result for display and tracking
            star_fgf_y: star_fgf.map(|s| s.y),
            selected_star_x: guider.guide_stars.first().map(|s| s.x),  // FGF result (stored in guider)
            selected_star_y: guider.guide_stars.first().map(|s| s.y),  // FGF result (stored in guider)
            guide_stars: guider.guide_stars.iter().map(|s| (s.x, s.y)).collect(),  // All tracked guide stars
            image_base64: b64_img,
            mount_x: mx,
            mount_y: my,
        });
    }
}