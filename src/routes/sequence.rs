use actix_web::{post, web, HttpResponse, Responder};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::capture_solve::{
    make_initial_guess, run_sequence, CaptureSettings, MeridianFlipConfig, SequenceItem,
};
use crate::goto_closed_loop::goto_closed_loop;
use crate::state::{AppState, SequenceProgress};

#[derive(Deserialize)]
pub struct SequenceGroup {
    /// Display / folder name for this target.
    pub target: String,
    /// Capture items (light, dark, etc.) for this group.
    pub sequence: Vec<SequenceItem>,
    /// Target RA in "HH:MM:SS" format (for GoTo & meridian flip).
    pub ra: Option<String>,
    /// Target Dec in "DD:MM:SS" format (for GoTo & meridian flip).
    pub dec: Option<String>,
    /// If true, skip GoTo for this group.
    pub skip_goto: Option<bool>,
    /// If true, this is a Park-only group (no captures).
    pub park: Option<bool>,
}

#[derive(Deserialize)]
pub struct StartSequencePayload {
    /// Ordered list of target groups to execute sequentially.
    pub groups: Vec<SequenceGroup>,
    pub date: String,
    pub resume_from: Option<u32>,
    pub subfolder: Option<String>,
    /// Enable automatic meridian-flip detection during the sequence.
    pub meridian_flip: Option<bool>,
    /// How many hours past the meridian to wait before flipping (default: 0.1 ≈ 6 min).
    pub post_meridian_limit_h: Option<f64>,
    /// Park the mount after all groups complete.
    pub park_after: Option<bool>,
    /// Shut down the Raspberry Pi after all groups (and optional park) complete.
    pub shutdown_after: Option<bool>,
    /// Use closed-loop (plate-solve) GoTo.
    pub closed_loop: Option<bool>,
    /// Auto-recenter every N light frames using closed-loop GoTo. 0 or None = disabled.
    pub recenter_every: Option<u32>,
}

#[post("/start_sequence")]
pub async fn handle_start_sequence(
    payload: web::Json<StartSequencePayload>,
    data: web::Data<AppState>,
) -> impl Responder {
    println!(
        "Received Start Sequence request ({} group(s))",
        payload.groups.len()
    );
    let _ = data.event_sender.send(format!(
        "Received Start Sequence request ({} group(s))",
        payload.groups.len()
    ));

    let settings_guard = data.camera_settings.lock().await;
    let iso = settings_guard.iso;
    let platesolving_exposure = settings_guard.platesolving_exposure;
    let cam_config = settings_guard.to_camera_config();
    let camera_model = settings_guard.camera_model.clone();
    drop(settings_guard);

    let custom_suffix = {
        let opts = data.sequence_options.lock().await;
        opts.custom_suffix.clone()
    };

    let capture_settings = CaptureSettings {
        iso,
        aperture: None,
        exposure_seconds: 0.0,
        save_directory: PathBuf::from("imgs/astro_captures"),
    };

    data.is_running.store(true, Ordering::Relaxed);
    data.should_pause.store(false, Ordering::Relaxed);

    let data_clone = data.clone();
    let payload = payload.into_inner();
    let resume_idx = payload.resume_from.unwrap_or(0);
    let use_closed_loop = payload.closed_loop.unwrap_or(false);

    let parse_hms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 {
            return None;
        }
        let h: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        Some(h + m / 60.0 + s / 3600.0)
    };
    let parse_dms = |s: &str| -> Option<f64> {
        let p: Vec<&str> = s.trim().split(':').collect();
        if p.len() != 3 {
            return None;
        }
        let d: f64 = p[0].parse().ok()?;
        let m: f64 = p[1].parse().ok()?;
        let s: f64 = p[2].parse().ok()?;
        let sign = if d < 0.0 { -1.0 } else { 1.0 };
        Some(sign * (d.abs() + m / 60.0 + s / 3600.0))
    };
    let parse_time_tuple = |s: &str| -> Option<(u8, u8, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    };
    let parse_dec_tuple = |s: &str| -> Option<(i64, i64, f64)> {
        let parts: Vec<&str> = s.trim().split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    };

    tokio::spawn(async move {
        {
            let mut ss = data_clone.sequence_state.lock().await;
            ss.status = "running".to_string();
            ss.progress = None;
        }

        let mut progress_rx = data_clone.event_sender.subscribe();
        let data_for_progress = data_clone.clone();
        let progress_tracker = tokio::spawn(async move {
            loop {
                match progress_rx.recv().await {
                    Ok(msg) if msg.starts_with("PROGRESS:") => {
                        let rest = &msg["PROGRESS:".len()..];
                        if let Some(colon_pos) = rest.find(':') {
                            let counts_part = &rest[..colon_pos];
                            let msg_part = &rest[colon_pos + 1..];
                            let parts: Vec<&str> = counts_part.split('/').collect();
                            if parts.len() == 2 {
                                if let (Ok(current), Ok(total)) =
                                    (parts[0].parse::<u32>(), parts[1].parse::<u32>())
                                {
                                    let mut ss = data_for_progress.sequence_state.lock().await;
                                    ss.progress = Some(SequenceProgress {
                                        current,
                                        total,
                                        msg: msg_part.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        });

        let start_msg = format!(
            "Starting sequence ({} group(s), resuming from {})...",
            payload.groups.len(),
            resume_idx
        );
        println!("{}", start_msg);
        let _ = data_clone.event_sender.send(start_msg);

        let camera_opt = data_clone.camera.lock().await;
        let camera = match camera_opt.as_ref() {
            Some(c) => c,
            None => {
                let msg = "Camera not connected, cannot start sequence.".to_string();
                eprintln!("{}", msg);
                let _ = data_clone.event_sender.send(msg);
                progress_tracker.abort();
                let mut ss = data_clone.sequence_state.lock().await;
                ss.status = "idle".to_string();
                return;
            }
        };

        let client_guard = data_clone.indi_client.read().await;
        let indi_ref = client_guard.as_ref();

        let mut all_ok = true;
        let num_groups = payload.groups.len();

        for (group_idx, group) in payload.groups.iter().enumerate() {
            if !data_clone.is_running.load(Ordering::Relaxed) {
                let _ = data_clone
                    .event_sender
                    .send("Sequence stopped between groups.".to_string());
                all_ok = false;
                break;
            }

            // --- Park-only group ---
            if group.park.unwrap_or(false) {
                let _ = data_clone
                    .event_sender
                    .send("Parking mount (sequence group)...".to_string());
                if let Some(client) = indi_ref {
                    if let Err(e) = client.park().await {
                        let msg = format!("Park failed during sequence: {}", e);
                        eprintln!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                        all_ok = false;
                        break;
                    }
                    let _ = data_clone.event_sender.send("Mount parked.".to_string());
                }
                continue;
            }

            // --- GoTo (unless skipped) ---
            if !group.skip_goto.unwrap_or(false) {
                if let (Some(ra_str), Some(dec_str)) = (group.ra.as_deref(), group.dec.as_deref())
                {
                    let _ = data_clone.event_sender.send(format!(
                        "Group {}/{}: Slewing to {}...",
                        group_idx + 1,
                        num_groups,
                        group.target
                    ));

                    let ra_parsed = parse_time_tuple(ra_str);
                    let dec_parsed = parse_dec_tuple(dec_str);

                    match (ra_parsed, dec_parsed) {
                        (Some(ra), Some(dec)) => {
                            let target = make_initial_guess(
                                ra.0 as i64,
                                ra.1 as i64,
                                ra.2,
                                dec.0,
                                dec.1,
                                dec.2,
                            );
                            let platesolve_settings = CaptureSettings {
                                iso,
                                aperture: None,
                                exposure_seconds: platesolving_exposure,
                                save_directory: PathBuf::from("imgs/goto/captures"),
                            };
                            if let Some(indi) = indi_ref {
                                let goto_result = goto_closed_loop(
                                    indi,
                                    Some(camera),
                                    platesolve_settings,
                                    target,
                                    use_closed_loop,
                                    &data_clone.event_sender,
                                    &data_clone.is_running,
                                    &cam_config,
                                )
                                .await;
                                if let Err(e) = goto_result {
                                    let msg =
                                        format!("GoTo failed for {}: {}", group.target, e);
                                    eprintln!("{}", msg);
                                    let _ = data_clone.event_sender.send(msg);
                                    all_ok = false;
                                    break;
                                }
                            } else {
                                let msg = "Mount not connected, skipping GoTo.".to_string();
                                let _ = data_clone.event_sender.send(msg);
                            }
                        }
                        _ => {
                            let msg = format!(
                                "Invalid RA/Dec for group {}, skipping GoTo.",
                                group.target
                            );
                            let _ = data_clone.event_sender.send(msg);
                        }
                    }
                }
            }

            // --- Build per-group meridian-flip config ---
            let flip_config: Option<MeridianFlipConfig> =
                if payload.meridian_flip.unwrap_or(false) {
                    let ra_h = group.ra.as_deref().and_then(|s| parse_hms(s));
                    let dec_deg = group.dec.as_deref().and_then(|s| parse_dms(s));
                    match (ra_h, dec_deg) {
                        (Some(ra), Some(dec)) => {
                            let longitude = {
                                let loc = data_clone.location.lock().await;
                                loc.longitude
                            };
                            Some(MeridianFlipConfig {
                                longitude_deg: longitude,
                                target_ra_h: ra,
                                target_dec_deg: dec,
                                post_meridian_limit_h: payload
                                    .post_meridian_limit_h
                                    .unwrap_or(0.1),
                            })
                        }
                        _ => None,
                    }
                } else {
                    None
                };

            {
                let mut afc = data_clone.active_flip_config.lock().await;
                *afc = flip_config.clone();
            }

            let group_resume = if group_idx == 0 { resume_idx } else { 0 };
            let _ = data_clone.event_sender.send(format!(
                "Group {}/{}: Capturing {} for {}...",
                group_idx + 1,
                num_groups,
                group.sequence.iter().map(|s| s.count).sum::<u32>(),
                group.target
            ));

            // Build recenter target from this group's RA/Dec (only when recenter is enabled)
            let recenter_every = payload.recenter_every.unwrap_or(0);
            let recenter_target: Option<(f64, f64)> = if recenter_every > 0 {
                let ra_h   = group.ra.as_deref().and_then(|s| parse_hms(s));
                let dec_deg = group.dec.as_deref().and_then(|s| parse_dms(s));
                match (ra_h, dec_deg) {
                    (Some(ra), Some(dec)) => Some((ra, dec)),
                    _ => None,
                }
            } else {
                None
            };

            let seq_result = run_sequence(
                camera,
                &capture_settings,
                &group.sequence,
                &group.target,
                &payload.date,
                payload.subfolder.clone(),
                group_resume,
                &data_clone.event_sender,
                &data_clone.is_running,
                &data_clone.should_pause,
                indi_ref,
                flip_config.as_ref(),
                cam_config.focal_length_mm,
                cam_config.pixel_size_micron,
                recenter_every,
                recenter_target,
                if recenter_every > 0 { Some(&cam_config) } else { None },
                platesolving_exposure,
                &camera_model,
                &custom_suffix,
            )
            .await
            .map_err(|e| e.to_string());

            match seq_result {
                Ok(_) => {
                    let is_still_running = data_clone.is_running.load(Ordering::Relaxed);
                    let is_paused = data_clone.should_pause.load(Ordering::Relaxed);

                    if !is_still_running || is_paused {
                        all_ok = false;
                        break;
                    }

                    if group_idx < num_groups - 1 {
                        let msg = format!(
                            "Group {}/{} complete ({}).",
                            group_idx + 1,
                            num_groups,
                            group.target
                        );
                        println!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                    }
                }
                Err(e) => {
                    let msg = format!("Plan failed for {}: {}", group.target, e);
                    eprintln!("{}", msg);
                    let _ = data_clone.event_sender.send(msg);
                    all_ok = false;
                    break;
                }
            }
        }

        let is_paused = data_clone.should_pause.load(Ordering::Relaxed);
        if all_ok && !is_paused {
            let msg = "Sequence complete!".to_string();
            println!("{}", msg);
            let _ = data_clone.event_sender.send(msg);
        }

        {
            let mut ss = data_clone.sequence_state.lock().await;
            if is_paused {
                ss.status = "paused".to_string();
            } else {
                ss.status = "idle".to_string();
                ss.progress = None;
            }
        }

        progress_tracker.abort();
        data_clone.is_running.store(false, Ordering::Relaxed);

        {
            let mut afc = data_clone.active_flip_config.lock().await;
            *afc = None;
        }

        if all_ok && !is_paused {
            if payload.park_after.unwrap_or(false) {
                let _ = data_clone
                    .event_sender
                    .send("Parking mount after sequence...".to_string());
                let client_opt = data_clone.indi_client.read().await;
                if let Some(client) = client_opt.as_ref() {
                    if let Err(e) = client.park().await {
                        let msg = format!("Post-sequence park failed: {}", e);
                        eprintln!("{}", msg);
                        let _ = data_clone.event_sender.send(msg);
                    } else {
                        let _ = data_clone
                            .event_sender
                            .send("Mount parked after sequence.".to_string());
                    }
                }
            }

            if payload.shutdown_after.unwrap_or(false) {
                let _ = data_clone
                    .event_sender
                    .send("Shutting down after sequence...".to_string());
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = std::process::Command::new("sudo")
                    .args(&["shutdown", "-h", "now"])
                    .spawn();
            }
        }
    });

    HttpResponse::Ok().body("Sequence started in background")
}

#[post("/stop")]
pub async fn handle_stop(data: web::Data<AppState>) -> impl Responder {
    println!("Received Stop request");
    let _ = data.event_sender.send("Received Stop request".to_string());
    data.is_running.store(false, Ordering::Relaxed);
    {
        let mut ss = data.sequence_state.lock().await;
        ss.status = "idle".to_string();
        ss.progress = None;
    }
    HttpResponse::Ok().body("Stop signal sent")
}

#[post("/pause")]
pub async fn handle_pause(data: web::Data<AppState>) -> impl Responder {
    println!("Received Pause request");
    let _ = data.event_sender.send("Received Pause request".to_string());
    data.should_pause.store(true, Ordering::Relaxed);
    {
        let mut ss = data.sequence_state.lock().await;
        ss.status = "paused".to_string();
    }
    HttpResponse::Ok().body("Pause signal sent")
}
