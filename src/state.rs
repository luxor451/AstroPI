use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use astro_pi_plate_solving::CameraConfig;
use crate::capture_solve::MeridianFlipConfig;

#[derive(Serialize, Deserialize, Clone)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CameraGlobalSettings {
    pub iso: u64,
    pub platesolving_exposure: f64,
    pub pixel_size_micron: f64,
    pub focal_length_mm: f64,
    pub bitdepth: u16,
}

impl Default for CameraGlobalSettings {
    fn default() -> Self {
        Self {
            iso: 1600,
            platesolving_exposure: 2.0,
            pixel_size_micron: 6.0,
            focal_length_mm: 714.0,
            bitdepth: 14,
        }
    }
}

impl CameraGlobalSettings {
    pub fn to_camera_config(&self) -> CameraConfig {
        CameraConfig {
            pixel_size_micron: self.pixel_size_micron,
            focal_length_mm: self.focal_length_mm,
            bitdepth: self.bitdepth,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SequenceOptions {
    pub park_after: bool,
    pub shutdown_after: bool,
    pub meridian_flip: bool,
    pub post_meridian_limit_h: String,
    pub subfolder: String,
    /// Recenter every N light frames using closed-loop GoTo. 0 = disabled.
    pub recenter_every: u32,
}

impl Default for SequenceOptions {
    fn default() -> Self {
        Self {
            park_after: false,
            shutdown_after: false,
            meridian_flip: false,
            post_meridian_limit_h: "0.1".to_string(),
            subfolder: String::new(),
            recenter_every: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SequenceItemBackend {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub exposure: String,
    pub count: String,
    #[serde(rename = "targetName")]
    pub target_name: String,
    #[serde(rename = "targetRaDeg")]
    pub target_ra_deg: f64,
    #[serde(rename = "targetDecDeg")]
    pub target_dec_deg: f64,
    #[serde(rename = "skipGoto")]
    pub skip_goto: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SequenceProgress {
    pub current: u32,
    pub total: u32,
    pub msg: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BackendSequenceState {
    pub plan: Vec<SequenceItemBackend>,
    pub status: String,
    pub progress: Option<SequenceProgress>,
}

impl Default for BackendSequenceState {
    fn default() -> Self {
        Self {
            plan: vec![],
            status: "idle".to_string(),
            progress: None,
        }
    }
}

/// Partial update struct – only the fields the frontend sends.
#[derive(Deserialize)]
pub struct SequenceStateUpdate {
    pub plan: Option<Vec<SequenceItemBackend>>,
    pub status: Option<String>,
    pub progress: Option<SequenceProgress>,
}

pub struct AppState {
    pub indi_client: RwLock<Option<eqmod_communication::IndiClient>>,
    pub camera: Mutex<Option<camera_control::CameraController>>,
    pub event_sender: broadcast::Sender<String>,
    pub is_running: Arc<AtomicBool>,
    pub location: Mutex<Location>,
    pub indi_server_process: Mutex<Option<std::process::Child>>,
    pub camera_settings: Mutex<CameraGlobalSettings>,
    pub closed_loop: Mutex<bool>,
    pub sequence_options: Mutex<SequenceOptions>,
    pub sequence_state: Mutex<BackendSequenceState>,
    pub should_pause: Arc<AtomicBool>,
    /// Active meridian-flip configuration (set when a sequence with flip is running).
    pub active_flip_config: Mutex<Option<MeridianFlipConfig>>,
}
