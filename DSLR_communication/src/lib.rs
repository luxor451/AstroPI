//! Camera Control Library
//!
//! Simple library to control cameras via USB using gphoto2.
//! Designed for astrophotography - assumes camera is in Bulb (B) mode.

use gphoto2::camera::CameraEvent;
use gphoto2::widget::{RadioWidget, ToggleWidget};
use gphoto2::{Camera, Context, Error, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::sleep;

mod test;
/// Camera controller for astrophotography
pub struct CameraController {
    camera: Camera,
    #[allow(dead_code)]
    context: Context,
}

impl CameraController {
    /// Connect to the camera
    ///
    /// Make sure:
    /// - Camera is connected via USB
    /// - Camera is in Manual (M) or Bulb (B) mode
    /// - No other software is using the camera
    pub async fn connect() -> Result<Self> {
        let context = Context::new()?;
        let camera = context.autodetect_camera().await?;
        Ok(Self { camera, context })
    }

    /// Get the camera model name
    pub async fn model(&self) -> String {
        self.camera.abilities().model().to_string()
    }

    /// Take a long exposure photo
    ///
    /// For exposures ≤30s, uses the camera's built-in shutter speed timer
    /// (hardware-precise). For longer exposures, falls back to software-timed
    /// bulb mode.
    ///
    /// # Arguments
    /// * `iso` - ISO value (e.g., 100, 400, 800, 1600, 3200)
    /// * `aperture` - Optional aperture value (e.g., Some(2.8), Some(5.6)). None keeps current setting.
    /// * `exposure_seconds` - Exposure time in seconds
    /// * `save_path` - Directory to save the image
    /// * `cancel_flag` - Optional atomic bool to check for cancellation
    ///
    /// # Returns
    /// Full path to the saved image
    pub async fn take_photo(
        &self,
        iso: u64,
        aperture: Option<f64>,
        exposure_seconds: u64,
        save_path: &Path,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<std::path::PathBuf> {
        // Set ISO
        self.set_iso(iso).await?;

        // Set aperture if provided
        if let Some(ap) = aperture {
            self.set_aperture(ap).await?;
        }

        // For exposures ≤30s, try camera-timed capture (hardware precision).
        // Fall back to software-timed bulb for longer or if shutterspeed setting fails.
        let filename = if exposure_seconds <= 30 {
            match self.camera_timed_capture(exposure_seconds, save_path, cancel_flag).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Camera-timed capture failed ({}), falling back to bulb", e);
                    self.bulb_capture(Duration::from_secs(exposure_seconds), save_path, cancel_flag).await?
                }
            }
        } else {
            self.bulb_capture(Duration::from_secs(exposure_seconds), save_path, cancel_flag).await?
        };

        Ok(save_path.join(filename))
    }

    /// Get available ISO values
    pub async fn get_iso_options(&self) -> Result<Vec<u64>> {
        let widget: RadioWidget = self.camera.config_key("iso").await?;
        Ok(widget
            .choices_iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect())
    }

    /// Get available aperture values (f-stops)
    pub async fn get_aperture_options(&self) -> Result<Vec<f64>> {
        let widget: RadioWidget = self.camera.config_key("aperture").await?;
        Ok(widget
            .choices_iter()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect())
    }

    /// Print full camera config (for debugging)
    pub async fn print_config(&self) -> Result<()> {
        let config = self.camera.config().await?;
        println!("{:#?}", config);
        Ok(())
    }

    // ===== Private methods =====

    async fn set_iso(&self, iso: u64) -> Result<()> {
        let widget: RadioWidget = self.camera.config_key("iso").await?;
        let iso_str = iso.to_string();

        let choices: Vec<String> = widget.choices_iter().collect();
        if !choices.contains(&iso_str) {
            return Err(Error::from(format!(
                "ISO {} not available. Options: {:?}",
                iso,
                self.get_iso_options().await.unwrap_or_default()
            )));
        }

        widget.set_choice(&iso_str)?;
        self.camera.set_config(&widget).await?;
        Ok(())
    }

    async fn set_aperture(&self, aperture: f64) -> Result<()> {
        let widget: RadioWidget = self.camera.config_key("aperture").await?;

        let choices: Vec<String> = widget.choices_iter().collect();
        
        // Try exact match first, then look for close match
        let choice = choices.iter().find(|c| {
            c.parse::<f64>().map(|v| (v - aperture).abs() < 0.01).unwrap_or(false)
        });

        match choice {
            Some(c) => {
                widget.set_choice(c)?;
                self.camera.set_config(&widget).await?;
                Ok(())
            }
            None => Err(Error::from(format!(
                "Aperture f/{} not available. Options: {:?}",
                aperture,
                self.get_aperture_options().await.unwrap_or_default()
            ))),
        }
    }

    /// Capture using the camera's built-in shutter speed timer (hardware-precise).
    /// Sets shutterspeed to the requested value, triggers a normal capture, then
    /// restores the previous shutterspeed setting.
    async fn camera_timed_capture(
        &self,
        exposure_seconds: u64,
        save_path: &Path,
        _cancel_flag: Option<&AtomicBool>,
    ) -> Result<String> {
        let widget: RadioWidget = self.camera.config_key("shutterspeed").await?;
        let prev_choice = widget.choice();

        // Find the closest matching shutter speed from available choices.
        // Camera choices are strings like "1", "2", "5", "10", "15", "20", "30", "bulb", etc.
        let target = exposure_seconds.to_string();
        let choices: Vec<String> = widget.choices_iter().collect();
        let matched = choices.iter().find(|c| *c == &target);

        let choice = match matched {
            Some(c) => c.clone(),
            None => {
                return Err(Error::from(format!(
                    "Shutter speed {}s not available on camera. Options: {:?}",
                    exposure_seconds, choices
                )));
            }
        };

        widget.set_choice(&choice)?;
        self.camera.set_config(&widget).await?;

        // Trigger capture — the camera handles its own hardware timer.
        // capture_image() blocks until the camera finishes the exposure and
        // writes the file, so no software timing loop is needed.
        let result = self.camera.capture_image().await;

        // Restore previous shutter speed
        let widget: RadioWidget = self.camera.config_key("shutterspeed").await?;
        widget.set_choice(&prev_choice)?;
        self.camera.set_config(&widget).await?;

        let file = result?;
        let filename = file.name().to_string();
        let dest = save_path.join(&filename);

        let final_dest = if dest.exists() {
            let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
            let ext = dest.extension().unwrap_or_default().to_string_lossy();
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let new_name = format!("{}_{}.{}", stem, ts, ext);
            save_path.join(&new_name)
        } else {
            dest
        };

        self.camera
            .fs()
            .download_to(&file.folder(), &file.name(), &final_dest)
            .await?;

        Ok(final_dest.file_name().unwrap().to_string_lossy().to_string())
    }

    async fn bulb_capture(&self, duration: Duration, save_path: &Path, cancel_flag: Option<&AtomicBool>) -> Result<String> {
        // Try the bulb toggle method first (works on Nikon and some Canon)
        if let Ok(()) = self.bulb_with_toggle(duration, cancel_flag).await {
            return self.download_image(save_path).await;
        }

        // Try Canon EOS remote release method
        if let Ok(()) = self.bulb_with_eos_release(duration, cancel_flag).await {
            return self.download_image(save_path).await;
        }

        Err(Error::from(
            "Bulb capture failed. Make sure camera is in Bulb (B) mode.",
        ))
    }

    async fn wait_for_duration(&self, duration: Duration, cancel_flag: Option<&AtomicBool>) -> bool {
        let start = std::time::Instant::now();
        let step = Duration::from_millis(100);

        while start.elapsed() < duration {
            if let Some(flag) = cancel_flag {
                if !flag.load(Ordering::Relaxed) {
                    return true; // Cancelled
                }
            }
            // Sleep for the lesser of one step or the remaining time
            let remaining = duration.saturating_sub(start.elapsed());
            sleep(remaining.min(step)).await;
        }
        false // Not cancelled
    }

    async fn bulb_with_toggle(&self, duration: Duration, cancel_flag: Option<&AtomicBool>) -> Result<()> {
        // Get the bulb toggle widget
        let bulb: ToggleWidget = self.camera.config_key("bulb").await?;

        // Open shutter
        bulb.set_toggled(true);
        self.camera.set_config(&bulb).await?;

        // Wait for exposure (cancellable)
        self.wait_for_duration(duration, cancel_flag).await;

        // Close shutter
        bulb.set_toggled(false);
        self.camera.set_config(&bulb).await?;

        Ok(())
    }

    async fn bulb_with_eos_release(&self, duration: Duration, cancel_flag: Option<&AtomicBool>) -> Result<()> {
        // Get the EOS remote release widget
        let release: RadioWidget = self.camera.config_key("eosremoterelease").await?;

        // Press shutter
        release.set_choice("Immediate")?;
        self.camera.set_config(&release).await?;

        release.set_choice("Press Full")?;
        self.camera.set_config(&release).await?;

        // Wait for exposure (cancellable)
        self.wait_for_duration(duration, cancel_flag).await;

        // Release shutter
        release.set_choice("Release Full")?;
        self.camera.set_config(&release).await?;

        release.set_choice("None")?;
        self.camera.set_config(&release).await?;

        Ok(())
    }

    async fn download_image(&self, save_path: &Path) -> Result<String> {
        let timeout = Duration::from_secs(30);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            let event = self.camera.wait_event(Duration::from_secs(2)).await?;

            if let CameraEvent::NewFile(file) = event {
                let filename = file.name().to_string();
                let dest = save_path.join(&filename);

                // If file exists, add timestamp
                let final_dest = if dest.exists() {
                    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
                    let ext = dest.extension().unwrap_or_default().to_string_lossy();
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let new_name = format!("{}_{}.{}", stem, ts, ext);
                    save_path.join(&new_name)
                } else {
                    dest
                };

                self.camera
                    .fs()
                    .download_to(&file.folder(), &file.name(), &final_dest)
                    .await?;

                return Ok(final_dest.file_name().unwrap().to_string_lossy().to_string());
            }
        }

        Err(Error::from("Timeout waiting for image from camera"))
    }
}


