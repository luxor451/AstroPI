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
    #[error("Command failed: {0}")]
    Command(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IndiError>;

/// Which side of the pier the telescope is on.
/// PIER_WEST means the scope points East (counterweight-down for most GEMs).
/// PIER_EAST means the scope points West.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PierSide {
    West,
    East,
    Unknown,
}

impl std::fmt::Display for PierSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PierSide::West => write!(f, "WEST"),
            PierSide::East => write!(f, "EAST"),
            PierSide::Unknown => write!(f, "UNKNOWN"),
        }
    }
}

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
#[derive(Clone)]
pub struct IndiClient {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    state: Arc<RwLock<IndiState>>,
    device_name: String,
    sender: Option<tokio::sync::broadcast::Sender<String>>,
    // We might need a way to signal errors to monitor_slew
    latest_message: Arc<Mutex<String>>,
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
        let latest_message = Arc::new(Mutex::new(String::new()));
        
        // Spawn the continuous reader loop
        let state_clone = state.clone();
        let sender_clone = sender.clone();
        let message_clone = latest_message.clone();

        tokio::spawn(async move {
            Self::reader_loop(reader, state_clone, sender_clone, message_clone).await;
        });

        let client = IndiClient {
            writer: Arc::new(Mutex::new(writer)),
            state,
            device_name: device_name.to_string(),
            sender,
            latest_message,
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

    async fn reader_loop(
        mut reader: OwnedReadHalf, 
        state: Arc<RwLock<IndiState>>, 
        sender: Option<tokio::sync::broadcast::Sender<String>>,
        latest_message: Arc<Mutex<String>>
    ) {
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
                        if let Err(_e) = Self::process_indi_message(&msg, &state, &sender, &latest_message).await {
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

    async fn process_indi_message(
        xml_str: &str, 
        state: &Arc<RwLock<IndiState>>, 
        sender: &Option<tokio::sync::broadcast::Sender<String>>,
        latest_message: &Arc<Mutex<String>>
    ) -> std::result::Result<(), String> {
        let doc = Document::parse(xml_str).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let tag_name = root.tag_name().name();

        // Handle delProperty — the driver removes properties (e.g. when parked)
        if tag_name == "delProperty" {
            let device = root.attribute("device").unwrap_or("unknown").to_string();
            let name = root.attribute("name");
            let mut state_lock = state.write().await;
            if let Some(dev_props) = state_lock.devices.get_mut(&device) {
                if let Some(prop_name) = name {
                    dev_props.remove(prop_name);
                } else {
                    // No name → driver deleted ALL properties for this device
                    dev_props.clear();
                }
            }
            return Ok(());
        }

        if tag_name == "message" {
             let device = root.attribute("device").unwrap_or("unknown");
             let message = root.attribute("message").unwrap_or("").to_string();
             
             if !message.is_empty() {
                 let log_msg = format!("INDI Message [{}]: {}", device, message);
                 println!("{}", log_msg);
                 
                 // Store latest message
                 {
                     let mut msg_lock = latest_message.lock().await;
                     *msg_lock = message.clone();
                 }

                 if let Some(s) = sender {
                     let _ = s.send(log_msg);
                 }
             }
             return Ok(());
        }

        // Handle setNumber/Switch etc.
        // We need to parse both "def" (definition) and "set" (update) vectors, maybe "new" too (client sent)
        // Usually server sends "def...Vector" initially, and "set...Vector" for updates.
        // Also "get...Vector" responses.
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
            
            // Log coordinate updates for debugging
            if name == "EQUATORIAL_EOD_COORD" {
                 // println!("DEBUG: Received Coords Update: {:?} state={}", elements, prop_state);
            }
            
            // Critical: Only update if we have meaningful data or status change
            // Just overwrite for now
            let mut state_lock = state.write().await;
            let device_map = state_lock.devices.entry(device.clone()).or_default();
            
            // If the property already exists, merge elements because sometimes updates only contain changed elements? 
            // Actually INDI standard says setVector contains all elements usually, but let's be safe.
            // For now, full replace is fine as we construct `elements` fully from the XML.
            
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
    pub async fn connect(&self) -> Result<()> {
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
                // Unpark first — EQMod rejects tracking & motion while parked
                self.unpark().await?;
                sleep(Duration::from_millis(500)).await;

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

    /// Get the current mount status
    pub async fn get_mount_status(&self) -> String {
        let state_lock = self.state.read().await;
        if let Some(props) = state_lock.devices.get(&self.device_name) {
            // Check if parked
            if let Some(park_prop) = props.get("TELESCOPE_PARK") {
                if park_prop.elements.get("PARK").map(|v| v == "On").unwrap_or(false) {
                    return "PARKED".to_string();
                }
            }
            
            // Check if slewing or tracking
            if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                if coord.state == "Busy" {
                    return "SLEWING".to_string();
                } else if coord.state == "Ok" {
                    return "TRACKING".to_string();
                } else if coord.state == "Idle" {
                    return "IDLE".to_string();
                } else if coord.state == "Alert" {
                    return "ERROR".to_string();
                }
            }
        }
        "UNKNOWN".to_string()
    }

    /// Get the current mount coordinates (RA, DEC)
    pub async fn get_coordinates(&self) -> (f64, f64) {
        let state_lock = self.state.read().await;
        if let Some(props) = state_lock.devices.get(&self.device_name) {
            if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                let ra = coord.elements.get("RA").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let dec = coord.elements.get("DEC").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                return (ra, dec);
            }
        }
        (0.0, 0.0)
    }

    /// Disconnect from the telescope mount
    pub async fn disconnect(&self) -> Result<()> {
        self.send_switch("CONNECTION", &[("CONNECT", false), ("DISCONNECT", true)]).await
    }
    

    /// Park the telescope
    pub async fn park(&self) -> Result<()> {
        println!("DEBUG: Parking telescope...");
        // Ensure explicit switches for OneOfMany property
        self.send_switch("TELESCOPE_PARK", &[("PARK", true), ("UNPARK", false)]).await
    }

    /// Unpark the telescope
    pub async fn unpark(&self) -> Result<()> {
        println!("DEBUG: Unparking telescope...");
        // Ensure explicit switches for OneOfMany property
        self.send_switch("TELESCOPE_PARK", &[("PARK", false), ("UNPARK", true)]).await
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
    pub async fn set_time(&self, utc_datetime: &str) -> Result<()> {      
        self.send_command(&format!(
            "<newTextVector device=\"{}\" name=\"TIME_UTC\">\n  <oneText name=\"UTC\">{}</oneText>\n  <oneText name=\"OFFSET\">0.0</oneText>\n</newTextVector>\n",
            self.device_name, utc_datetime
        )).await?;
        
        Ok(())
    }

    /// Send a goto command to the telescope mount
    pub async fn goto(&self, ra: f64, dec: f64, is_running: Option<&std::sync::atomic::AtomicBool>) -> Result<()> {
        assert!(ra >= 0.0 && ra <= 24.0, "RA must be between 0 and 24 hours");
        assert!(dec >= -90.0 && dec <= 90.0, "DEC must be between -90 and +90 degrees");

        // Ensure mount is unparked before goto
        self.unpark().await?;

        // Give it a moment to process unpark if needed
        sleep(Duration::from_millis(250)).await;

        self.send_switch("TELESCOPE_TRACK_STATE", &[("TRACK_ON", true), ("TRACK_OFF", false)]).await?;
        self.send_switch("ON_COORD_SET", &[
            ("TRACK", true), 
            ("SLEW", false), 
            ("SYNC", false)
        ]).await?;
        self.send_numbers("EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]).await?;
        
        println!("DEBUG: Sent Goto command. Starting monitor_slew...");
        
        // Monitor slew
        self.monitor_slew(ra, dec, is_running).await?;
        
        Ok(())
    }

    /// Sync the mount's internal position to the given coordinates.
    ///
    /// This tells the mount "you are actually pointing at (ra, dec)" so that
    /// subsequent GOTO commands use a corrected model.  Typically called after
    /// a successful plate solve.
    ///
    /// The method temporarily switches `ON_COORD_SET` to SYNC, sends the
    /// coordinates, then restores TRACK mode.
    pub async fn sync(&self, ra: f64, dec: f64) -> Result<()> {
        assert!(ra >= 0.0 && ra <= 24.0, "RA must be between 0 and 24 hours");
        assert!(dec >= -90.0 && dec <= 90.0, "DEC must be between -90 and +90 degrees");

        // Switch to SYNC mode
        self.send_switch("ON_COORD_SET", &[
            ("TRACK", false),
            ("SLEW", false),
            ("SYNC", true),
        ]).await?;

        // Send solved coordinates – the mount will update its internal state
        self.send_numbers("EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]).await?;

        // Small delay to let the mount process the sync
        sleep(Duration::from_millis(300)).await;

        // Restore TRACK mode for future GOTO commands
        self.send_switch("ON_COORD_SET", &[
            ("TRACK", true),
            ("SLEW", false),
            ("SYNC", false),
        ]).await?;

        println!("DEBUG: Synced mount to RA={:.6}h, DEC={:.6}°", ra, dec);
        Ok(())
    }

    /// List all property names currently defined for this device.
    /// Useful for debugging which properties the INDI driver has exposed.
    pub async fn list_properties(&self) -> Vec<String> {
        let state_lock = self.state.read().await;
        if let Some(dev_props) = state_lock.devices.get(&self.device_name) {
            dev_props.keys().cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Abort the current slew/motion
    pub async fn abort_motion(&self) -> Result<()> {
        println!("DEBUG: Sending TELESCOPE_ABORT_MOTION...");
        self.send_switch("TELESCOPE_ABORT_MOTION", &[("ABORT", true)]).await
    }

    /// Start or stop manual motion in a cardinal direction.
    ///
    /// `direction` must be one of `"north"`, `"south"`, `"east"`, `"west"`.
    /// When `start` is `true` motion begins; when `false` it stops.
    pub async fn manual_move(&self, direction: &str, start: bool) -> Result<()> {
        match direction {
            "north" => {
                self.send_switch("TELESCOPE_MOTION_NS", &[
                    ("MOTION_NORTH", start),
                    ("MOTION_SOUTH", false),
                ]).await
            }
            "south" => {
                self.send_switch("TELESCOPE_MOTION_NS", &[
                    ("MOTION_NORTH", false),
                    ("MOTION_SOUTH", start),
                ]).await
            }
            "east" => {
                self.send_switch("TELESCOPE_MOTION_WE", &[
                    ("MOTION_WEST", false),
                    ("MOTION_EAST", start),
                ]).await
            }
            "west" => {
                self.send_switch("TELESCOPE_MOTION_WE", &[
                    ("MOTION_WEST", start),
                    ("MOTION_EAST", false),
                ]).await
            }
            _ => Err(IndiError::Command(format!("Unknown direction: {}", direction))),
        }
    }

    /// Set the slew rate for manual motion.
    ///
    /// EQMod exposes a `TELESCOPE_SLEW_RATE` switch with labels `1x` … `9x`.
    /// `rate_index` is 0-based (0 = 1x, 1 = 2x, … , 8 = 9x).
    pub async fn set_slew_rate(&self, rate_index: u8) -> Result<()> {
        let labels = ["1x", "2x", "3x", "4x", "5x", "6x", "7x", "8x", "9x"];
        let idx = (rate_index as usize).min(labels.len() - 1);
        let switches: Vec<(&str, bool)> = labels
            .iter()
            .enumerate()
            .map(|(i, &lbl)| (lbl, i == idx))
            .collect();
        self.send_switch("TELESCOPE_SLEW_RATE", &switches).await
    }

    // =================================================================
    // Pier Side & Hour Angle helpers
    // =================================================================

    /// Read TELESCOPE_PIER_SIDE from the cached INDI state.
    ///
    /// Returns `PierSide::West` when the switch element `PIER_WEST` is `"On"`,
    /// `PierSide::East` when `PIER_EAST` is `"On"`, or `PierSide::Unknown`
    /// if the property has not been reported yet.
    pub async fn get_pier_side(&self) -> PierSide {
        let state_lock = self.state.read().await;
        if let Some(props) = state_lock.devices.get(&self.device_name) {
            if let Some(pier) = props.get("TELESCOPE_PIER_SIDE") {
                if pier.elements.get("PIER_WEST").map(|v| v == "On").unwrap_or(false) {
                    return PierSide::West;
                }
                if pier.elements.get("PIER_EAST").map(|v| v == "On").unwrap_or(false) {
                    return PierSide::East;
                }
            }
        }
        PierSide::Unknown
    }

    /// Compute the current Hour Angle in hours.
    ///
    /// HA = LST − RA  (range −12 … +12 h).
    ///
    /// Requires longitude (degrees, East-positive) so we can derive LST.
    /// Returns `None` if the mount has not yet reported coordinates.
    pub async fn get_hour_angle(&self, longitude_deg: f64) -> Option<f64> {
        let (ra_h, _dec) = self.get_coordinates().await;
        // RA == 0 could be valid, but if we never got coords the mount reports (0,0),
        // so only bail when the property is truly absent
        let has_coords = {
            let s = self.state.read().await;
            s.devices.get(&self.device_name)
                .and_then(|p| p.get("EQUATORIAL_EOD_COORD"))
                .is_some()
        };
        if !has_coords {
            return None;
        }

        let lst_h = Self::local_sidereal_time(longitude_deg);
        let mut ha = lst_h - ra_h;
        // Normalise to −12 … +12
        if ha < -12.0 { ha += 24.0; }
        if ha > 12.0  { ha -= 24.0; }
        Some(ha)
    }

    /// Returns `true` when the target has crossed the meridian and the mount
    /// should flip.  Crossing is detected when:
    ///   • pier side is WEST  **and**  HA > `post_meridian_limit_h`
    ///
    /// `post_meridian_limit_h` is a small positive value (e.g. 0.1 h ≈ 6 min)
    /// that lets you delay the flip until the target is slightly past the
    /// meridian, which is common practice to avoid flipping back and forth.
    pub async fn needs_meridian_flip(
        &self,
        longitude_deg: f64,
        post_meridian_limit_h: f64,
    ) -> bool {
        let pier = self.get_pier_side().await;
        if pier != PierSide::West {
            return false; // only flip when currently on the west side
        }
        match self.get_hour_angle(longitude_deg).await {
            Some(ha) => ha > post_meridian_limit_h,
            None => false,
        }
    }

    /// Compute the time (in fractional hours) until the mount would need a
    /// meridian flip, i.e. until HA reaches `post_meridian_limit_h`.
    ///
    /// Returns:
    ///  - `Some(positive)` if the flip has not happened yet (hours remaining).
    ///  - `Some(0.0)` or `Some(negative)` if the flip is already overdue.
    ///  - `None` if pier side is not West or coordinates are unavailable.
    ///
    /// The conversion factor from HA-hours to solar hours accounts for the
    /// sidereal rate (≈ 1.00274 HA-hours per solar hour).
    pub async fn time_to_meridian_flip(
        &self,
        longitude_deg: f64,
        post_meridian_limit_h: f64,
    ) -> Option<f64> {
        let pier = self.get_pier_side().await;
        if pier != PierSide::West {
            return None; // no flip expected when on the East side
        }
        let ha = self.get_hour_angle(longitude_deg).await?;
        // Sidereal rate: 24 HA-hours per 23.9345 solar hours
        const SIDEREAL_RATE: f64 = 24.0 / 23.9344696;
        let delta_ha = post_meridian_limit_h - ha;
        Some(delta_ha / SIDEREAL_RATE)
    }

    /// Execute a meridian flip by explicitly requesting the opposite pier side
    /// and then re-issuing a GOTO to the target coordinates.
    ///
    /// The sequence is:
    ///   1. Read current pier side.
    ///   2. Set `TELESCOPE_PIER_SIDE` to the **opposite** side so EQMod knows
    ///      which orientation to use for the slew.
    ///   3. Issue a regular GOTO to the target.
    ///   4. Verify the pier side actually changed.
    ///
    /// Returns the new pier side after the slew.
    pub async fn execute_meridian_flip(
        &self,
        target_ra_h: f64,
        target_dec_deg: f64,
        is_running: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<PierSide> {
        let before = self.get_pier_side().await;
        println!("Meridian flip: pier side before = {}", before);

        // Request the opposite pier side BEFORE issuing the goto.
        match before {
            PierSide::West => {
                println!("Meridian flip: requesting PIER_EAST");
                self.send_switch("TELESCOPE_PIER_SIDE", &[
                    ("PIER_EAST", true),
                    ("PIER_WEST", false),
                ]).await?;
            }
            PierSide::East => {
                println!("Meridian flip: requesting PIER_WEST");
                self.send_switch("TELESCOPE_PIER_SIDE", &[
                    ("PIER_WEST", true),
                    ("PIER_EAST", false),
                ]).await?;
            }
            PierSide::Unknown => {
                println!("Meridian flip: pier side unknown, proceeding with goto only");
            }
        }

        // Small delay to let EQMod process the pier side change request.
        sleep(Duration::from_millis(500)).await;

        // Re-issue goto – EQMod should now slew to the opposite side.
        self.goto(target_ra_h, target_dec_deg, is_running).await?;

        let after = self.get_pier_side().await;
        println!("Meridian flip: pier side after  = {}", after);

        if before != PierSide::Unknown && after == before {
            println!("WARNING: pier side did not change after meridian flip goto");
            if let Some(sender) = &self.sender {
                let _ = sender.send(
                    "WARNING: Pier side did not change after meridian flip. \
                     The mount may not support forced pier side changes."
                        .to_string(),
                );
            }
        }

        Ok(after)
    }

    // --------- internal helpers ---------

    /// Greenwich Mean Sidereal Time in hours for the current UTC instant.
    fn gmst_hours() -> f64 {
        let now = Utc::now();
        // Julian date
        let jd = now.timestamp() as f64 / 86400.0 + 2440587.5;
        let t = (jd - 2451545.0) / 36525.0;
        // IAU formula (hours)
        let gmst = 280.46061837
            + 360.98564736629 * (jd - 2451545.0)
            + 0.000387933 * t * t
            - (t * t * t) / 38710000.0;
        (gmst % 360.0 + 360.0) % 360.0 / 15.0
    }

    /// Local Sidereal Time in hours.
    fn local_sidereal_time(longitude_deg: f64) -> f64 {
        let gmst = Self::gmst_hours();
        let lst = gmst + longitude_deg / 15.0;
        ((lst % 24.0) + 24.0) % 24.0
    }

    /// Monitor the slew progress
    async fn monitor_slew(&self, target_ra: f64, target_dec: f64, is_running: Option<&std::sync::atomic::AtomicBool>) -> Result<()> {
        let _ = (target_ra, target_dec); // kept for signature compatibility; not used for comparison
        let start = Instant::now();
        let mut last_pos: (f64, f64) = (0.0, 0.0);
        let mut seen_busy = false;
        // Tracks consecutive seconds the mount has been in non-Busy state.
        // Used to detect fast correction slews that complete before polling starts.
        let mut stable_ok_secs: u64 = 0;
        let mut first_poll = true;

        loop {
            if let Some(running) = is_running {
                if !running.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(sender) = &self.sender {
                        let _ = sender.send("Slew aborted by user.".to_string());
                    }
                    return Ok(());
                }
            }

            // Check for horizon / limit error messages from the driver
            {
                let mut msg_lock = self.latest_message.lock().await;
                let msg = msg_lock.clone();
                if msg.to_lowercase().contains("horizon") || msg.to_lowercase().contains("limit") {
                    let err_msg = format!("Error: {}", msg);
                    println!("{}", err_msg);
                    if let Some(sender) = &self.sender {
                        let _ = sender.send(err_msg.clone());
                    }
                    *msg_lock = String::new();
                    return Err(IndiError::Command(msg));
                }
            }

            // First poll is immediate so we catch fast correction slews that
            // complete in < 1 second.  All subsequent polls are 1-second apart.
            if first_poll {
                first_poll = false;
            } else {
                sleep(Duration::from_millis(1000)).await;
            }

            let (ra, dec, state_str, is_busy, is_tracking) = {
                let state_lock = self.state.read().await;
                if let Some(props) = state_lock.devices.get(&self.device_name) {
                    let mut r = last_pos.0;
                    let mut d = last_pos.1;
                    let mut s = "UNKNOWN".to_string();
                    let mut busy = false;
                    let mut tracking = false;

                    if let Some(coord) = props.get("EQUATORIAL_EOD_COORD") {
                        r = coord.elements.get("RA").and_then(|v| v.parse().ok()).unwrap_or(last_pos.0);
                        d = coord.elements.get("DEC").and_then(|v| v.parse().ok()).unwrap_or(last_pos.1);

                        if coord.state == "Busy" {
                            s = " SLEWING".to_string();
                            busy = true;
                        } else if coord.state == "Ok" {
                            s = " TRACKING".to_string();
                            tracking = true;
                        }
                    }
                    (r, d, s, busy, tracking)
                } else {
                    (last_pos.0, last_pos.1, "UNKNOWN".to_string(), false, false)
                }
            };

            if is_busy {
                seen_busy = true;
                stable_ok_secs = 0;
            } else if is_tracking {
                // Only count stable-tracking seconds when the state is
                // definitively Ok — not when it is UNKNOWN (INDI not responding).
                stable_ok_secs += 1;
            } else {
                // UNKNOWN: don't advance the stable counter; INDI may just be
                // slow to respond on the first poll.
            }

            let elapsed = start.elapsed().as_secs();
            let delta_ra = (ra - last_pos.0).abs();
            let delta_dec = (dec - last_pos.1).abs();

            let msg = format!(
                "[{:2}s] {} Pointing to : - RA: {:.6}h, DEC: {:.6}° (ΔRA:{:.4}h, ΔDEC:{:.4}°)",
                elapsed, state_str, ra, dec, delta_ra, delta_dec
            );
            println!("{}", msg);
            if let Some(sender) = &self.sender {
                let _ = sender.send(msg);
            }

            last_pos = (ra, dec);

            // Primary: Busy → Ok transition means slew completed normally.
            if seen_busy && !is_busy {
                println!("Mount reached target position (Busy -> Ok transition).");
                break;
            }

            // Fallback: mount was never seen as Busy but has been in confirmed
            // Ok/Tracking state for ≥ 5 consecutive seconds.  This handles fast
            // correction slews (< 1 s) that complete before the first poll, and
            // the case where EQMod skips the Busy state for tiny corrections.
            // We require is_tracking (not just !is_busy) so that an UNKNOWN /
            // disconnected INDI state never falsely triggers this.
            if !seen_busy && stable_ok_secs >= 5 {
                println!("Mount already at target position (stable Tracking, no slew detected).");
                break;
            }

            if elapsed > 120 {
                return Err(IndiError::Command("Slew timed out after 120 s".to_string()));
            }
        }

        Ok(())
    }

    /// Wait until a property is defined in the INDI state for this device.
    ///
    /// Some properties (e.g. `TELESCOPE_SLEW_RATE`, `TELESCOPE_MOTION_NS`,
    /// `TELESCOPE_MOTION_WE`) are defined asynchronously by the driver after
    /// connection.  Sending a `newSwitchVector` before the corresponding
    /// `defSwitchVector` arrives causes a "Property … is not defined" error.
    #[allow(dead_code)]
    async fn wait_for_property(&self, property: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let state = self.state.read().await;
                if let Some(dev_props) = state.devices.get(&self.device_name) {
                    if dev_props.contains_key(property) {
                        return Ok(());
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(IndiError::Device(format!(
                    "Timeout waiting for property '{}' to be defined on '{}'",
                    property, self.device_name
                )));
            }
            sleep(Duration::from_millis(250)).await;
        }
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