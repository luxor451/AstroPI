// --- CONFIG ---
pub const START_X: f64 = 1200.0;
pub const START_Y: f64 = 1200.0;
pub const CAM_W: u32 = 640;
pub const CAM_H: u32 = 480;
pub const CAM_SCALE: f64 = 4.0; // Camera upscaling factor (0.5 mount pixels = 1 camera pixel)
pub const T: u32 = 4; // Threshold for Fast Gaussian Fit
pub const NB_STARS: usize = 6; // Number of guide stars to use for multi-star guiding
pub const RMS_WINDOW_SIZE: usize = 100;  // RMS tracking - sliding window of last 10 seconds (100 samples at 100ms interval)
pub const MAX_CORRECTION: f64 = 5.0; // Outlier rejection - max correction in pixels per frame


// Control constants for the close loop guider
pub const KP_RA: f64 = 2.0;
pub const KI_RA: f64 = 0.8;
pub const KD_RA: f64 = 1.5;

pub const KP_DEC: f64 = 1.5;
pub const KI_DEC: f64 = 0.6;
pub const KD_DEC: f64 = 1.0;

// Low-pass filter cutoff frequency (Hz)
// Lower values = more filtering (smoother but slower response)
// Higher values = less filtering (faster but noisier response)
pub const FILTER_CUTOFF_FREQ_RA: f64 = 0.05;
pub const FILTER_CUTOFF_FREQ_DEC: f64 = 0.1;

// Integral anti-windup limits (prevents integral term from growing too large)
pub const INTEGRAL_LIMIT_RA: f64 = 15.0;
pub const INTEGRAL_LIMIT_DEC: f64 = 15.0;