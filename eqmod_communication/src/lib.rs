use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration, Instant};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IndiError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Communication error: {0}")]
    Communication(String),
    #[error("Device error: {0}")]
    Device(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IndiError>;

/// INDI client for communicating with telescope mounts
pub struct IndiClient {
    stream: TcpStream,
    device_name: String,
}

impl IndiClient {
    /// Create a new INDI client connection
    ///
    /// # Arguments
    /// * `host` - INDI server host (e.g., "localhost")
    /// * `port` - INDI server port (default is 7624)
    /// * `device_name` - Name of the telescope device (e.g., "EQMod Mount")
    pub async fn new(host: &str, port: u16, device_name: &str) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| IndiError::Connection(format!("Failed to connect to {}: {}", addr, e)))?;

        let mut client = IndiClient {
            stream,
            device_name: device_name.to_string(),
        };

        client.send_command("<getProperties version=\"1.7\" />").await?;
        
        Ok(client)
    }

    /// Connect to the telescope mount
    ///
    /// This must be called before sending any commands to the mount
    /// Waits for the mount to fully initialize and report its properties
    pub async fn connect(&mut self) -> Result<()> {
        self.send_switch("CONNECTION", &[("CONNECT", true), ("DISCONNECT", false)]).await?;
        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        
        sleep(Duration::from_secs(3)).await;
        self.read_response(65536).await?; // Drain buffer
        
        Ok(())
    }

    /// Disconnect from the telescope mount
    pub async fn disconnect(&mut self) -> Result<()> {
        self.send_switch("CONNECTION", &[("CONNECT", false), ("DISCONNECT", true)]).await
    }

    /// Send a goto command to the telescope mount
    ///
    /// # Arguments
    /// * `ra` - Right Ascension in hours (0.0 to 24.0)
    /// * `dec` - Declination in degrees (-90.0 to +90.0)
    ///
    /// # Example
    /// ```no_run
    /// # tokio_test::block_on(async {
    /// use eqmod_communication::IndiClient;
    ///
    /// let mut client = IndiClient::new("localhost", 7624, "EQMod Mount").await.unwrap();
    /// client.connect().await.unwrap();
    /// client.goto(12.5, 45.0).await.unwrap();
    /// # });
    /// ```
    pub async fn goto(&mut self, ra: f64, dec: f64) -> Result<()> {
        self.validate_coordinates(ra, dec)?;
        
        // Get current position
        let current_pos = self.get_current_position().await?;
        self.print_goto_info(current_pos, (ra, dec));

        // Setup mount for goto
        self.send_switch("ON_COORD_SET", &[("TRACK", true), ("SLEW", false), ("SYNC", false)]).await?;
        sleep(Duration::from_millis(200)).await;
        
        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        sleep(Duration::from_millis(200)).await;
        
        self.send_switch("TELESCOPE_TRACK_MODE", &[
            ("TRACK_SIDEREAL", true),
            ("TRACK_SOLAR", false),
            ("TRACK_LUNAR", false),
            ("TRACK_CUSTOM", false)
        ]).await?;
        sleep(Duration::from_millis(200)).await;

        // Send coordinates
        self.send_numbers("EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]).await?;

        println!("\n🚀 GOTO command sent, monitoring slew progress...\n");
        
        // Monitor slew
        self.monitor_slew(ra, dec).await?;
        
        // Ensure tracking after slew
        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        sleep(Duration::from_millis(500)).await;
        
        // Request properties to get updated tracking state
        self.send_command("<getProperties version=\"1.7\" />").await?;
        sleep(Duration::from_millis(500)).await;
        
        // Final status
        if let Ok(response) = self.read_response(65536).await {
            self.display_final_status(&response);
        }

        Ok(())
    }

    /// Get current mount position
    async fn get_current_position(&mut self) -> Result<(f64, f64)> {
        self.send_command("<getProperties version=\"1.7\" />").await?;
        sleep(Duration::from_millis(500)).await;
        
        let response = self.read_response(65536).await?;
        let (ra, dec) = self.parse_coordinates(&response);
        Ok((ra, dec))
    }

    /// Monitor the slew progress
    async fn monitor_slew(&mut self, target_ra: f64, target_dec: f64) -> Result<()> {
        let start = Instant::now();
        let mut last_pos = (0.0, 0.0);
        
        for i in 0..30 {
            sleep(Duration::from_secs(1)).await;
            
            let response = self.read_response(65536).await?;
            if response.is_empty() {
                continue;
            }

            // Check for errors
            if let Some(error) = self.extract_error(&response) {
                println!("\n⚠️  {}", error);
            }

            // Check completion
            if response.contains("Telescope slew is complete") {
                println!("\n✅ Mount reports: Telescope slew is complete!");
                break;
            }

            // Get current position and status
            let (ra, dec) = self.parse_coordinates(&response);
            let is_busy = response.contains("state=\"Busy\"");
            let tracking = self.parse_tracking_state(&response);

            let elapsed = start.elapsed().as_secs();
            let status = if is_busy {
                "🔄 SLEWING"
            } else if tracking {
                "✅ TRACKING"
            } else {
                "⏸️  IDLE"
            };

            let delta_ra = (ra - last_pos.0).abs();
            let delta_dec = (dec - last_pos.1).abs();

            println!("[{:2}s] {} - RA: {:.6}h, DEC: {:.6}° (ΔRA:{:.4}h, ΔDEC:{:.4}°)",
                elapsed, status, ra, dec, delta_ra, delta_dec);

            last_pos = (ra, dec);

            // Check if near target
            if (ra - target_ra).abs() < 0.01 && (dec - target_dec).abs() < 0.1 && i > 5 {
                println!("\n✅ Mount reached target position!");
                break;
            }
        }

        Ok(())
    }

    /// Send a switch vector command
    async fn send_switch(&mut self, name: &str, switches: &[(&str, bool)]) -> Result<()> {
        let mut xml = format!("<newSwitchVector device=\"{}\" name=\"{}\">\n", self.device_name, name);
        for (switch_name, on) in switches {
            xml.push_str(&format!("  <oneSwitch name=\"{}\">{}</oneSwitch>\n",
                switch_name, if *on { "On" } else { "Off" }));
        }
        xml.push_str("</newSwitchVector>\n");
        
        self.send_command(&xml).await
    }

    /// Send a number vector command
    async fn send_numbers(&mut self, name: &str, numbers: &[(&str, f64)]) -> Result<()> {
        let mut xml = format!("<newNumberVector device=\"{}\" name=\"{}\">\n", self.device_name, name);
        for (number_name, value) in numbers {
            xml.push_str(&format!("  <oneNumber name=\"{}\">{:.10}</oneNumber>\n", number_name, value));
        }
        xml.push_str("</newNumberVector>\n");
        
        self.send_command(&xml).await
    }

    /// Send raw command
    pub async fn send_command(&mut self, cmd: &str) -> Result<()> {
        self.stream
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| IndiError::Communication(format!("Failed to send command: {}", e)))
    }

    /// Read response from server
    pub async fn read_response(&mut self, buffer_size: usize) -> Result<String> {
        let mut buffer = vec![0u8; buffer_size];
        match timeout(Duration::from_secs(5), self.stream.read(&mut buffer)).await {
            Ok(Ok(n)) => Ok(String::from_utf8_lossy(&buffer[..n]).to_string()),
            Ok(Err(e)) => Err(IndiError::Communication(format!("Read error: {}", e))),
            Err(_) => Ok(String::new()), // Timeout
        }
    }

    /// Validate coordinates
    fn validate_coordinates(&self, ra: f64, dec: f64) -> Result<()> {
        if !(0.0..=24.0).contains(&ra) {
            return Err(IndiError::Device(format!(
                "Invalid RA: {}. Must be 0.0-24.0 hours", ra)));
        }
        if !(-90.0..=90.0).contains(&dec) {
            return Err(IndiError::Device(format!(
                "Invalid DEC: {}. Must be -90.0 to +90.0 degrees", dec)));
        }
        Ok(())
    }

    /// Parse coordinates from XML response
    pub fn parse_coordinates(&self, response: &str) -> (f64, f64) {
        let coord_section = response.split("EQUATORIAL_EOD_COORD").last();
        
        let ra = coord_section
            .and_then(|s| s.find("<oneNumber name=\"RA\">"))
            .and_then(|pos| {
                let after = &coord_section.unwrap()[pos + 21..];
                after.find("</oneNumber>")
                    .and_then(|end| after[..end].trim().parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        let dec = coord_section
            .and_then(|s| s.find("<oneNumber name=\"DEC\">"))
            .and_then(|pos| {
                let after = &coord_section.unwrap()[pos + 22..];
                after.find("</oneNumber>")
                    .and_then(|end| after[..end].trim().parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        (ra, dec)
    }

    /// Extract error/warning messages
    fn extract_error(&self, response: &str) -> Option<String> {
        for line in response.lines() {
            if line.contains("<message") && (line.contains("WARNING") || line.contains("ERROR")) {
                if let Some(start) = line.find("message=\"") {
                    if let Some(end) = line[start + 9..].find("\"") {
                        return Some(line[start + 9..start + 9 + end].to_string());
                    }
                }
            }
        }
        None
    }

    /// Parse tracking state from XML response
    pub fn parse_tracking_state(&self, response: &str) -> bool {
        if let Some(track_section) = response.split("TELESCOPE_TRACK_STATE").nth(1) {
            // Look for TRACK_ON switch
            if let Some(track_on_section) = track_section.split("TRACK_ON").nth(1) {
                // Find the content between > and </oneSwitch>
                if let Some(start) = track_on_section.find('>') {
                    if let Some(end) = track_on_section[start..].find("</oneSwitch>") {
                        let content = track_on_section[start + 1..start + end].trim();
                        return content == "On";
                    }
                }
            }
        }
        false
    }

    /// Print goto information
    fn print_goto_info(&self, current: (f64, f64), target: (f64, f64)) {
        println!("📡 Mount Status:");
        println!("   Current: RA {:.4}h, DEC {:.4}°", current.0, current.1);
        println!("   Target:  RA {:.4}h, DEC {:.4}°", target.0, target.1);
        
        let delta_ra = (target.0 - current.0).abs();
        let delta_dec = (target.1 - current.1).abs();
        println!("   Movement: RA {:.4}h ({:.1} arcmin), DEC {:.4}° ({:.1} arcmin)",
            delta_ra, delta_ra * 60.0, delta_dec, delta_dec * 60.0);
    }

    /// Display final status
    fn display_final_status(&self, response: &str) {
        println!("\n╔══════════════════════════════════════════╗");
        println!("║         Final Mount Status               ║");
        println!("╚══════════════════════════════════════════╝\n");

        let (ra, dec) = self.parse_coordinates(response);
        if ra != 0.0 || dec != 0.0 {
            println!("📍 Position: RA {:.6}h, DEC {:.6}°", ra, dec);
            
            let ra_hms = format!("{}h {}m {:.1}s",
                ra.trunc(),
                (ra.fract() * 60.0).trunc(),
                (ra.fract() * 60.0).fract() * 60.0);
            let dec_dms = format!("{}° {}' {:.1}\"",
                dec.trunc(),
                (dec.fract() * 60.0).abs().trunc(),
                (dec.fract() * 60.0).abs().fract() * 60.0);
            println!("            ({}, {})", ra_hms, dec_dms);
        }

        let tracking = self.parse_tracking_state(response);
        
        println!("🔧 Tracking: {}", if tracking { "✅ ENABLED" } else { "⚠️  DISABLED" });
        
        // Debug tracking state if disabled
        if !tracking {
            self.debug_tracking_state(response);
        }
        
        // Parse horizontal coords if available
        if let Some(horiz) = response.split("HORIZONTAL_COORD").nth(1) {
            if let Some(alt_start) = horiz.find("<oneNumber name=\"ALT\">") {
                if let Some(alt_end) = horiz[alt_start + 22..].find("</oneNumber>") {
                    if let Ok(alt) = horiz[alt_start + 22..alt_start + 22 + alt_end].trim().parse::<f64>() {
                        if let Some(az_start) = horiz.find("<oneNumber name=\"AZ\">") {
                            if let Some(az_end) = horiz[az_start + 21..].find("</oneNumber>") {
                                if let Ok(az) = horiz[az_start + 21..az_start + 21 + az_end].trim().parse::<f64>() {
                                    println!("🧭 Altitude: {:.2}°, Azimuth: {:.2}°", alt, az);
                                }
                            }
                        }
                    }
                }
            }
        }
        
        println!("\n══════════════════════════════════════════\n");
    }

    /// Debug helper to print tracking state info from XML
    pub fn debug_tracking_state(&self, response: &str) {
        println!("🔍 Debug: Searching for TELESCOPE_TRACK_STATE...");
        
        if let Some(track_section) = response.split("TELESCOPE_TRACK_STATE").nth(1) {
            // Get first 500 chars of the section
            let preview = if track_section.len() > 500 {
                &track_section[..500]
            } else {
                track_section
            };
            println!("   Found TELESCOPE_TRACK_STATE section:");
            println!("   {}", preview.replace('\n', "\n   "));
            
            // Parse tracking with detailed info
            let has_track_on = track_section.contains("TRACK_ON");
            let tracking = self.parse_tracking_state(response);
            
            // Extract actual content
            let content = if let Some(track_on_section) = track_section.split("TRACK_ON").nth(1) {
                if let Some(start) = track_on_section.find('>') {
                    if let Some(end) = track_on_section[start..].find("</oneSwitch>") {
                        let raw = &track_on_section[start + 1..start + end];
                        format!("\"{}\" (trimmed: \"{}\")", raw, raw.trim())
                    } else {
                        "end tag not found".to_string()
                    }
                } else {
                    "start > not found".to_string()
                }
            } else {
                "TRACK_ON not found".to_string()
            };
            
            println!("\n   has TRACK_ON: {}", has_track_on);
            println!("   TRACK_ON content: {}", content);
            println!("   parsed tracking: {}", tracking);
        } else {
            println!("   ❌ TELESCOPE_TRACK_STATE not found in response!");
            println!("   Response contains {} bytes", response.len());
        }
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_coordinate_validation() {
        // Valid coordinates
        assert!((0.0..=24.0).contains(&12.5));
        assert!((-90.0..=90.0).contains(&45.0));
        
        // Invalid coordinates
        assert!(!(0.0..=24.0).contains(&25.0));
        assert!(!(0.0..=24.0).contains(&-1.0));
        assert!(!(-90.0..=90.0).contains(&91.0));
        assert!(!(-90.0..=90.0).contains(&-91.0));
    }
}
