//! Camera Control Library
//!
//! Simple library to control cameras via USB using gphoto2.
//! Designed for astrophotography - assumes camera is in Bulb (B) mode.

use gphoto2::camera::CameraEvent;
use gphoto2::widget::{RadioWidget, ToggleWidget};
use gphoto2::{Camera, Context, Error, Result};
use std::path::Path;
use std::thread;
use std::time::Duration;


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
    pub fn connect() -> Result<Self> {
        let context = Context::new()?;
        let camera = context.autodetect_camera().wait()?;
        Ok(Self { camera, context })
    }

    /// Get the camera model name
    pub fn model(&self) -> String {
        self.camera.abilities().model().to_string()
    }

    /// Take a long exposure photo (bulb mode)
    ///
    /// # Arguments
    /// * `iso` - ISO value (e.g., 100, 400, 800, 1600, 3200)
    /// * `aperture` - Optional aperture value (e.g., Some(2.8), Some(5.6)). None keeps current setting.
    /// * `exposure_seconds` - Exposure time in seconds
    /// * `save_path` - Directory to save the image
    ///
    /// # Returns
    /// Full path to the saved image
    ///
    /// # Example
    /// ```no_run
    /// use camera_control::CameraController;
    /// use std::path::Path;
    ///
    /// let camera = CameraController::connect().unwrap();
    /// // With aperture control
    /// let path = camera.take_photo(800, Some(2.8), 60, Path::new(".")).unwrap();
    /// // Without aperture control (use current setting)
    /// let path = camera.take_photo(1600, None, 30, Path::new(".")).unwrap();
    /// println!("Saved: {}", path.display());
    /// ```
    pub fn take_photo(
        &self,
        iso: u64,
        aperture: Option<f64>,
        exposure_seconds: u64,
        save_path: &Path,
    ) -> Result<std::path::PathBuf> {
        // Set ISO
        self.set_iso(iso)?;

        // Set aperture if provided
        if let Some(ap) = aperture {
            self.set_aperture(ap)?;
        }

        // Take the bulb exposure
        let filename = self.bulb_capture(Duration::from_secs(exposure_seconds), save_path)?;

        Ok(save_path.join(filename))
    }

    /// Get available ISO values
    pub fn get_iso_options(&self) -> Result<Vec<u64>> {
        let widget: RadioWidget = self.camera.config_key("iso").wait()?;
        Ok(widget
            .choices_iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect())
    }

    /// Get available aperture values (f-stops)
    pub fn get_aperture_options(&self) -> Result<Vec<f64>> {
        let widget: RadioWidget = self.camera.config_key("aperture").wait()?;
        Ok(widget
            .choices_iter()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect())
    }

    /// Print full camera config (for debugging)
    pub fn print_config(&self) -> Result<()> {
        let config = self.camera.config().wait()?;
        println!("{:#?}", config);
        Ok(())
    }

    // ===== Private methods =====

    fn set_iso(&self, iso: u64) -> Result<()> {
        let widget: RadioWidget = self.camera.config_key("iso").wait()?;
        let iso_str = iso.to_string();

        let choices: Vec<String> = widget.choices_iter().collect();
        if !choices.contains(&iso_str) {
            return Err(Error::from(format!(
                "ISO {} not available. Options: {:?}",
                iso,
                self.get_iso_options().unwrap_or_default()
            )));
        }

        widget.set_choice(&iso_str)?;
        self.camera.set_config(&widget).wait()?;
        Ok(())
    }

    fn set_aperture(&self, aperture: f64) -> Result<()> {
        let widget: RadioWidget = self.camera.config_key("aperture").wait()?;

        let choices: Vec<String> = widget.choices_iter().collect();
        
        // Try exact match first, then look for close match
        let choice = choices.iter().find(|c| {
            c.parse::<f64>().map(|v| (v - aperture).abs() < 0.01).unwrap_or(false)
        });

        match choice {
            Some(c) => {
                widget.set_choice(c)?;
                self.camera.set_config(&widget).wait()?;
                Ok(())
            }
            None => Err(Error::from(format!(
                "Aperture f/{} not available. Options: {:?}",
                aperture,
                self.get_aperture_options().unwrap_or_default()
            ))),
        }
    }

    fn bulb_capture(&self, duration: Duration, save_path: &Path) -> Result<String> {
        // Try the bulb toggle method first (works on Nikon and some Canon)
        if let Ok(()) = self.bulb_with_toggle(duration) {
            return self.download_image(save_path);
        }

        // Try Canon EOS remote release method
        if let Ok(()) = self.bulb_with_eos_release(duration) {
            return self.download_image(save_path);
        }

        Err(Error::from(
            "Bulb capture failed. Make sure camera is in Bulb (B) mode.",
        ))
    }

    fn bulb_with_toggle(&self, duration: Duration) -> Result<()> {
        // Get the bulb toggle widget
        let bulb: ToggleWidget = self.camera.config_key("bulb").wait()?;

        // Open shutter
        bulb.set_toggled(true);
        self.camera.set_config(&bulb).wait()?;

        // Wait for exposure
        thread::sleep(duration);

        // Close shutter
        bulb.set_toggled(false);
        self.camera.set_config(&bulb).wait()?;

        Ok(())
    }

    fn bulb_with_eos_release(&self, duration: Duration) -> Result<()> {
        // Get the EOS remote release widget
        let release: RadioWidget = self.camera.config_key("eosremoterelease").wait()?;

        // Press shutter
        release.set_choice("Immediate")?;
        self.camera.set_config(&release).wait()?;

        release.set_choice("Press Full")?;
        self.camera.set_config(&release).wait()?;

        // Wait for exposure
        thread::sleep(duration);

        // Release shutter
        release.set_choice("Release Full")?;
        self.camera.set_config(&release).wait()?;

        release.set_choice("None")?;
        self.camera.set_config(&release).wait()?;

        Ok(())
    }

    fn download_image(&self, save_path: &Path) -> Result<String> {
        let timeout = Duration::from_secs(30);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            let event = self.camera.wait_event(Duration::from_secs(2)).wait()?;

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
                    .wait()?;

                return Ok(final_dest.file_name().unwrap().to_string_lossy().to_string());
            }
        }

        Err(Error::from("Timeout waiting for image from camera"))
    }
}


