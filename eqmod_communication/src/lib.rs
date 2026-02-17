use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant};
use thiserror::Error;
use roxmltree::Document;
use std::collections::HashMap;
use std::sync::Arc;

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

#[derive(Debug, Clone)]
pub struct IndiProperty {
    pub name: String,
    pub device: String,
    pub state: String, // "Idle", "Ok", "Busy", "Alert"
    pub timestamp: String,
    // Stores element_name -> value (e.g., "CONNECT" -> "On", "RA" -> "12:00:00")
    pub elements: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct IndiState {
    // device_name -> property_name -> Property Data
    pub devices: HashMap<String, HashMap<String, IndiProperty>>,
}

/// INDI client for communicating with telescope mounts
pub struct IndiClient {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    state: Arc<RwLock<IndiState>>,
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

        let (reader, writer) = stream.into_split();
        let state = Arc::new(RwLock::new(IndiState::default()));
        
        // Spawn the continuous reader loop
        let state_clone = state.clone();
        tokio::spawn(async move {
            Self::reader_loop(reader, state_clone).await;
        });

        let client = IndiClient {
            writer: Arc::new(Mutex::new(writer)),
            state,
            device_name: device_name.to_string(),
        };

        // Send initial getProperties
        println!("Sending initial getProperties...");
        client.send_command("<getProperties version=\"1.7\" />").await?;
        
        Ok(client)
    }

    async fn reader_loop(mut reader: OwnedReadHalf, state: Arc<RwLock<IndiState>>) {
        let mut buffer = Vec::new();
        let mut temp_buf = [0u8; 4096];

        loop {
            match reader.read(&mut temp_buf).await {
                Ok(0) => {
                    eprintln!("INDI Server closed connection.");
                    break;
                }
                Ok(n) => {
                    buffer.extend_from_slice(&temp_buf[..n]);
                    
                    while let Some((msg, bytes_consumed)) = Self::extract_one_xml_message(&buffer) {
                        if let Err(_e) = Self::process_indi_message(&msg, &state).await {
                            // eprintln!("Error parsing INDI message: {}", e);
                        }
                        buffer.drain(0..bytes_consumed);
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from INDI socket: {}", e);
                    break;
                }
            }
        }
    }

    fn extract_one_xml_message(buffer: &[u8]) -> Option<(String, usize)> {
        let s = match std::str::from_utf8(buffer) {
            Ok(v) => v,
            Err(_) => return None,
        };

        let start_idx = s.find('<')?;
        let suffix = &s[start_idx..];
        
        if let Some(end_tag_close) = suffix.find('>') {
            let tag_content_end = start_idx + end_tag_close + 1;
            
            // Check for self-closing tag like <getProperties />
            if s[start_idx..tag_content_end].ends_with("/>") {
                 return Some((s[start_idx..tag_content_end].to_string(), tag_content_end));
            }

            // It has a body. We need to find the closing tag name.
            let tag_body = &s[start_idx + 1 .. tag_content_end - 1];
            // Get just the tag name (e.g. "defTextVector")
            let tag_name = tag_body.split_whitespace().next().unwrap_or(tag_body);
            
            let close_token = format!("</{}>", tag_name);
            if let Some(close_idx) = suffix.find(&close_token) {
                 let total_len = start_idx + close_idx + close_token.len();
                 return Some((s[start_idx..total_len].to_string(), total_len));
            }
        }
        None
    }

    async fn process_indi_message(xml_str: &str, state: &Arc<RwLock<IndiState>>) -> std::result::Result<(), String> {
        let doc = Document::parse(xml_str).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let tag_name = root.tag_name().name();

        if tag_name.ends_with("Vector") {
            let device = root.attribute("device").unwrap_or("unknown").to_string();
            let name = root.attribute("name").unwrap_or("unknown").to_string();
            let prop_state = root.attribute("state").unwrap_or("Idle").to_string();
            let timestamp = root.attribute("timestamp").unwrap_or("").to_string();

            let mut elements = HashMap::new();
            for child in root.children() {
                if child.is_element() {
                     let elem_name = child.attribute("name").unwrap_or("").to_string();
                     if let Some(text) = child.text() {
                        elements.insert(elem_name, text.trim().to_string());
                     }
                }
            }
            
            // Critical: Only update if we have meaningful data or status change
            // Just overwrite for now
            let mut state_lock = state.write().await;
            let device_map = state_lock.devices.entry(device.clone()).or_default();
            
            let property = IndiProperty {
                name: name.clone(),
                device,
                state: prop_state,
                timestamp,
                elements,
            };
            
            device_map.insert(name, property);
        } else if tag_name == "message" {
             if let Some(msg) = root.attribute("message") {
                 let device = root.attribute("device").unwrap_or("System");
                 println!("INDI Message [{}]: {}", device, msg);
             }
        }
        Ok(())
    }

    /// Connect to the telescope mount
    ///
    /// This must be called before sending any commands to the mount
    /// Waits for the mount to fully initialize and report its properties
    pub async fn connect(&mut self) -> Result<()> {
        println!("Sending CONNECTION command...");
        self.send_switch("CONNECTION", &[("CONNECT", true), ("DISCONNECT", false)]).await?;
        
        // Wait for connection to complete
        println!("Waiting for mount to report connected...");
        for i in 0..10 {
            sleep(Duration::from_secs(1)).await;
            
            // Check state
            let can_track = {
                let state = self.state.read().await;
                if let Some(dev_props) = state.devices.get(&self.device_name) {
                    if let Some(conn) = dev_props.get("CONNECTION") {
                         if let Some(val) = conn.elements.get("CONNECT") {
                             if val == "On" {
                                 true
                             } else { false }
                         } else { false }
                    } else { false }
                } else { false }
            };

            if can_track {
                println!("Mount connected!");
                 // Now enable tracking
                self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
                return Ok(());
            } else {
                println!("Waiting... ({}/10)", i+1);
            }
        }
        
        Err(IndiError::Connection(
            "Timeout waiting for mount to connect.".to_string()
        ))
    }

    /// Disconnect from the telescope mount
    pub async fn disconnect(&mut self) -> Result<()> {
        self.send_switch("CONNECTION", &[("CONNECT", false), ("DISCONNECT", true)]).await
    }
    

    /// Set geographic location for the mount
    ///
    /// # Arguments
    /// * `latitude` - Latitude in degrees (-90 to +90, positive is North)
    /// * `longitude` - Longitude in degrees (-180 to +180, positive is East)
    /// * `elevation` - Elevation in meters above sea level (optional, defaults to 0)
    ///
    /// # Example
    /// ```no_run
    /// # tokio_test::block_on(async {
    /// use eqmod_communication::IndiClient;
    ///
    /// let mut client = IndiClient::new("localhost", 7624, "EQMod Mount").await.unwrap();
    /// client.connect().await.unwrap();
    /// // Set location to Paris, France
    /// client.set_location(48.8566, 2.3522, 35.0).await.unwrap();
    /// # });
    /// ```
    pub async fn set_location(&self, latitude: f64, longitude: f64, elevation: f64) -> Result<()> {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(IndiError::Device(format!(
                "Invalid latitude: {}. Must be -90.0 to +90.0", latitude)));
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(IndiError::Device(format!(
                "Invalid longitude: {}. Must be -180.0 to +180.0", longitude)));
        }

        println!("Setting location: Lat={:.4}°, Lon={:.4}°, Elev={:.1}m", latitude, longitude, elevation);
        
        self.send_numbers("GEOGRAPHIC_COORD", &[
            ("LAT", latitude),
            ("LONG", longitude),
            ("ELEV", elevation)
        ]).await?;
        
        Ok(())
    }

    /// Set UTC time and date for the mount
    pub async fn set_time(&self, utc_datetime: &str) -> Result<()> {
        println!("Setting UTC time: {}", utc_datetime);
        
        self.send_command(&format!(
            "<newTextVector device=\"{}\" name=\"TIME_UTC\">\n  <oneText name=\"UTC\">{}</oneText>\n  <oneText name=\"OFFSET\">0.0</oneText>\n</newTextVector>\n",
            self.device_name, utc_datetime
        )).await?;
        
        Ok(())
    }

    /// Send a goto command to the telescope mount
    pub async fn goto(&mut self, ra: f64, dec: f64) -> Result<()> {
        self.validate_coordinates(ra, dec)?;
        
        // Get current position
        let current_pos = self.get_current_position().await?;
        self.print_goto_info(current_pos, (ra, dec));

        // Setup mount for goto
        // For EQMod and many INDI drivers:
        // 1. Ensure we are tracking (TELESCOPE_TRACK_STATE = TRACK_ON)
        // 2. Set ON_COORD_SET to SLEW (or TRACK, but SLEW often forces the move more reliably)
        // 3. Send coordinates
        
        println!("DEBUG: Ensuring Tracking is ON");
        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        sleep(Duration::from_millis(500)).await;

        println!("DEBUG: Setting ON_COORD_SET=TRACK for GOTO");
        // Try explicitly clearing others?
        // Some drivers are picky. Let's try sending just the one we want true.
        // Although send_switch logic sends all provided pairs.
        self.send_switch("ON_COORD_SET", &[
            ("TRACK", true), 
            ("SLEW", false), 
            ("SYNC", false)
        ]).await?;
        sleep(Duration::from_millis(500)).await;
        
        // Send coordinates
        println!("DEBUG: Sending Target Coordinates (GOTO trigger)");
        self.send_numbers("EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]).await?;

        println!("\nGOTO command sent, monitoring slew progress...\n");
        
        // Monitor slew
        self.monitor_slew(ra, dec).await?;
        
        Ok(())
    }

    /// Get current mount position
    pub async fn get_current_position(&self) -> Result<(f64, f64)> {
        let state = self.state.read().await;
        
        if let Some(props) = state.devices.get(&self.device_name) {
            if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                let ra = coord.elements.get("RA").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let dec = coord.elements.get("DEC").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                return Ok((ra, dec));
            }
        }
        
        // If not found, check if we have any data at all
        if state.devices.is_empty() {
             println!("Warning: No device data received yet.");
        }
        
        Ok((0.0, 0.0))
    }

    /// Get Local Sidereal Time from the mount
    pub async fn get_lst(&self) -> Result<f64> {
        // Wait reasonably for data if it's missing (simple polling)
        for _ in 0..5 {
            let state = self.state.read().await;
            if let Some(props) = state.devices.get(&self.device_name) {
                if let Some(time_lst) = props.get("TIME_LST") {
                    if let Some(lst_str) = time_lst.elements.get("LST") {
                        if let Ok(val) = lst_str.parse::<f64>() {
                            return Ok(val);
                        }
                    }
                }
            }
            drop(state);
            sleep(Duration::from_millis(500)).await;
        }

        Err(IndiError::Device("Could not find TIME_LST property or LST value in cached state".to_string()))
    }


    /// Monitor the slew progress
    async fn monitor_slew(&mut self, target_ra: f64, target_dec: f64) -> Result<()> {
        let start = Instant::now();
        let mut last_pos: (f64, f64) = (0.0, 0.0);
        
        for _ in 0..60 { // Extended timeout for slew
            sleep(Duration::from_millis(500)).await;
            
            let state = self.state.read().await;
            
            // Check for connection/device errors if we implemented error capturing
            // For now, check position
            
            let (ra, dec) = if let Some(props) = state.devices.get(&self.device_name) {
                if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                    let r: f64 = coord.elements.get("RA").and_then(|v| v.parse().ok()).unwrap_or(last_pos.0);
                    let d: f64 = coord.elements.get("DEC").and_then(|v| v.parse().ok()).unwrap_or(last_pos.1);
                    (r, d)
                } else { last_pos }
            } else { last_pos };

            // Determine status
            // Check ON_COORD_SET for "Busy" vs "Ok" state
            // Or TELESCOPE_TRACK_STATE
            let mut status = "UNKNOWN";
            if let Some(props) = state.devices.get(&self.device_name) {
                 if let Some(coord_prop) = props.get("EQUATORIAL_EOD_COORD") {
                     if coord_prop.state == "Busy" {
                         status = "🔄 SLEWING";
                     } else if coord_prop.state == "Ok" {
                         status = "✅ TRACKING"; // Likely
                     }
                 }
            }

            let elapsed = start.elapsed().as_secs();
            let delta_ra = (ra - last_pos.0).abs();
            let delta_dec = (dec - last_pos.1).abs();

            println!("[{:2}s] {} - RA: {:.6}h, DEC: {:.6}° (ΔRA:{:.4}h, ΔDEC:{:.4}°)",
                elapsed, status, ra, dec, delta_ra, delta_dec);

            last_pos = (ra, dec);

            // Check if near target
            if (ra - target_ra).abs() < 0.01 && (dec - target_dec).abs() < 0.1 {
                // Also check if state is no longer Busy
                let is_busy = if let Some(props) = state.devices.get(&self.device_name) {
                     props.get("EQUATORIAL_EOD_COORD").map(|p| p.state == "Busy").unwrap_or(false)
                } else { false };
                
                if !is_busy {
                    println!("\n✅ Mount reached target position!");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Send a switch vector command
    async fn send_switch(&self, name: &str, switches: &[(&str, bool)]) -> Result<()> {
        let mut xml = format!("<newSwitchVector device=\"{}\" name=\"{}\">\n", self.device_name, name);
        for (switch_name, on) in switches {
            xml.push_str(&format!("  <oneSwitch name=\"{}\">{}</oneSwitch>\n",
                switch_name, if *on { "On" } else { "Off" }));
        }
        xml.push_str("</newSwitchVector>\n");
        
        self.send_command(&xml).await
    }

    /// Send a number vector command
    async fn send_numbers(&self, name: &str, numbers: &[(&str, f64)]) -> Result<()> {
        let mut xml = format!("<newNumberVector device=\"{}\" name=\"{}\">\n", self.device_name, name);
        for (number_name, value) in numbers {
            xml.push_str(&format!("  <oneNumber name=\"{}\">{:.10}</oneNumber>\n", number_name, value));
        }
        xml.push_str("</newNumberVector>\n");
        
        self.send_command(&xml).await
    }

    /// Send raw command
    pub async fn send_command(&self, cmd: &str) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(cmd.as_bytes()).await
            .map_err(|e| IndiError::Communication(format!("Failed to send command: {}", e)))?;
        writer.flush().await
            .map_err(|e| IndiError::Communication(format!("Failed to flush command: {}", e)))?;
        Ok(())
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

    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}