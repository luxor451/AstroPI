use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant};
use thiserror::Error;
use roxmltree::Document;
use std::collections::HashMap;
use std::sync::Arc;
use chrono::Utc;

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
    sender: Option<tokio::sync::broadcast::Sender<String>>,
}

impl IndiClient {
    /// Create a new INDI client connection
    ///
    /// # Arguments
    /// * `host` - INDI server host (e.g., "localhost")
    /// * `port` - INDI server port (default is 7624)
    /// * `device_name` - Name of the telescope device (e.g., "EQMod Mount")
    pub async fn new(host: &str, port: u16, device_name: &str, sender: Option<tokio::sync::broadcast::Sender<String>>) -> Result<Self> {
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
            sender,
        };

        // Send initial getProperties
        client.send_command("<getProperties version=\"1.7\" />").await?;
        
        Ok(client)
    }

    pub async fn init_date_pos(&self, latitude: f64, longitude: f64, elevation: f64) -> Result<()> {
        // Wait for initial properties to be received
        sleep(Duration::from_secs(1)).await;
        
        // Set location
        self.set_location(latitude, longitude, elevation).await?;

        self.set_time(&Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()).await
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
        } else {
            // For now, ignore other message types (e.g., <defTextVector>, <message>, etc.)
        }
        Ok(())
    }

    /// Connect to the telescope mount
    ///
    /// This must be called before sending any commands to the mount
    /// Waits for the mount to fully initialize and report its properties
    pub async fn connect(&mut self) -> Result<()> {
        self.send_switch("CONNECTION", &[("CONNECT", true), ("DISCONNECT", false)]).await?;
        
        // Wait for connection to complete
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
    async fn set_location(&self, latitude: f64, longitude: f64, elevation: f64) -> Result<()> {
        assert!(elevation >= -500.0 && elevation <= 9000.0, "Elevation must be between -500 and 9000 meters");
        assert!(latitude >= -90.0 && latitude <= 90.0, "Latitude must be between -90 and +90 degrees");
        assert!(longitude >= -180.0 && longitude <= 180.0, "Longitude must be between -180 and +180 degrees");

        self.send_numbers("GEOGRAPHIC_COORD", &[
            ("LAT", latitude),
            ("LONG", longitude),
            ("ELEV", elevation)
        ]).await?;
        
        Ok(())
    }

    /// Set UTC time and date for the mount
    async fn set_time(&self, utc_datetime: &str) -> Result<()> {      
        self.send_command(&format!(
            "<newTextVector device=\"{}\" name=\"TIME_UTC\">\n  <oneText name=\"UTC\">{}</oneText>\n  <oneText name=\"OFFSET\">0.0</oneText>\n</newTextVector>\n",
            self.device_name, utc_datetime
        )).await?;
        
        Ok(())
    }

    /// Send a goto command to the telescope mount
    pub async fn goto(&mut self, ra: f64, dec: f64) -> Result<()> {
        assert!(ra >= 0.0 && ra <= 24.0, "RA must be between 0 and 24 hours");
        assert!(dec >= -90.0 && dec <= 90.0, "DEC must be between -90 and +90 degrees");


        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        self.send_switch("ON_COORD_SET", &[
            ("TRACK", true), 
            ("SLEW", false), 
            ("SYNC", false)
        ]).await?;
        self.send_numbers("EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]).await?;
        // Monitor slew
        self.monitor_slew(ra, dec).await?;
        
        Ok(())
    }

    /// Monitor the slew progress
    async fn monitor_slew(&mut self, target_ra: f64, target_dec: f64) -> Result<()> {
        let start = Instant::now();
        let mut last_pos: (f64, f64) = (0.0, 0.0);
        
        for _ in 0..60 {
            sleep(Duration::from_millis(1000)).await;
            
            let state = self.state.read().await;
            

            let (ra, dec) = if let Some(props) = state.devices.get(&self.device_name) {
                if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                    let r: f64 = coord.elements.get("RA").and_then(|v| v.parse().ok()).unwrap_or(last_pos.0);
                    let d: f64 = coord.elements.get("DEC").and_then(|v| v.parse().ok()).unwrap_or(last_pos.1);
                    (r, d)
                } else { last_pos }
            } else { last_pos };


            let mut status = "UNKNOWN";
            if let Some(props) = state.devices.get(&self.device_name) {
                 if let Some(coord_prop) = props.get("EQUATORIAL_EOD_COORD") {
                     if coord_prop.state == "Busy" {
                         status = " SLEWING";
                     } else if coord_prop.state == "Ok" {
                         status = " TRACKING"; // Likely
                     }
                 }
            }

            let elapsed = start.elapsed().as_secs();
            let delta_ra = (ra - last_pos.0).abs();
            let delta_dec = (dec - last_pos.1).abs();

            let msg = format!("[{:2}s] {} Pointing to : - RA: {:.6}h, DEC: {:.6}° (ΔRA:{:.4}h, ΔDEC:{:.4}°)",
                elapsed, status, ra, dec, delta_ra, delta_dec);
            
            println!("{}", msg);
            if let Some(sender) = &self.sender {
                let _ = sender.send(msg);
            }

            last_pos = (ra, dec);

            // Check if near target
            if (ra - target_ra).abs() < 0.01 && (dec - target_dec).abs() < 0.1 {
                // Also check if state is no longer Busy
                let is_busy = if let Some(props) = state.devices.get(&self.device_name) {
                     props.get("EQUATORIAL_EOD_COORD").map(|p| p.state == "Busy").unwrap_or(false)
                } else { false };
                
                if !is_busy {
                    println!("\n Mount reached target position!");
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
    async fn send_command(&self, cmd: &str) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer.write_all(cmd.as_bytes()).await
            .map_err(|e| IndiError::Communication(format!("Failed to send command: {}", e)))?;
        writer.flush().await
            .map_err(|e| IndiError::Communication(format!("Failed to flush command: {}", e)))?;
        Ok(())
    }
}