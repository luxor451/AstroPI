use serialport::SerialPort;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const BAUD_RATE: u32 = 9600;

// SkyWatcher 24-bit Constants
const TOTAL_STEPS_24BIT: u32 = 16_777_216;
const PHYSICAL_STEPS_PER_REV: f64 = 1_728_000.0;
const SIDEREAL_DAY_SEC: f64 = 86164.0905;
const INTERNAL_CLOCK_FREQ: f64 = 6_000_000.0;

pub struct Heq5 {
    port: Box<dyn SerialPort>,
}

impl Heq5 {
    pub fn new(port_name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let port = serialport::new(port_name, BAUD_RATE)
            .timeout(Duration::from_millis(500))
            .open()?;
        Ok(Self { port })
    }

    fn send(&mut self, cmd: &str) -> Result<String, String> {
        let _ = self.port.clear(serialport::ClearBuffer::Input);

        let full = format!(":{}\r", cmd);
        self.port
            .write_all(full.as_bytes())
            .map_err(|e| e.to_string())?;

        let mut resp = String::new();
        let mut buf = [0u8; 1];

        loop {
            match self.port.read(&mut buf) {
                Ok(1) => {
                    let c = buf[0] as char;
                    if c == '\r' {
                        break;
                    }
                    resp.push(c);
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(resp)
    }

    pub fn stop(&mut self, axis: u8) -> Result<(), String> {
        self.send(&format!("K{}", axis)).map(|_| ())
    }

    pub fn init_axis(&mut self, axis: u8) -> Result<(), String> {
        let r = self.send(&format!("F{}", axis))?;
        if r != "=" {
            return Err(format!("Init failed: {}", r));
        }
        Ok(())
    }

    pub fn get_position(&mut self, axis: u8) -> Result<u32, String> {
        let r = self.send(&format!("j{}", axis))?;
        if !r.starts_with('=') {
            return Err(format!("Bad position response: {}", r));
        }

        let raw_val = u32::from_str_radix(&r[1..], 16)
            .map_err(|_| format!("Invalid hex: {}", r))?;

        // Decode Little Endian
        let b1 = (raw_val & 0xFF0000) >> 16;
        let b2 = (raw_val & 0x00FF00);
        let b3 = (raw_val & 0x0000FF) << 16;
        
        Ok(b3 | b2 | b1)
    }

    pub fn is_moving(&mut self, axis: u8) -> Result<bool, String> {
        let r = self.send(&format!("f{}", axis))?;
        if r.len() < 3 || !r.starts_with('=') {
            return Err(format!("Bad status response: {}", r));
        }
        // Check Low Byte for Status Bit 0
        let low_byte_str = &r[1..3];
        let status = u8::from_str_radix(low_byte_str, 16).unwrap_or(0);
        
        Ok((status & 1) == 1)
    }

    pub fn slew_relative_deg(&mut self, axis: u8, degrees: f64) -> Result<u32, String> {
        let start = self.get_position(axis)?;
        
        let delta = ((degrees / 360.0) * TOTAL_STEPS_24BIT as f64).round() as i64;
        let target = start.wrapping_add(delta as u32) % TOTAL_STEPS_24BIT;

        if delta >= 0 {
            self.send(&format!("G{}", axis))?;
        } else {
            self.send(&format!("H{}", axis))?;
        }

        self.send(&format!("I{}{:06X}", axis, 0x0100))?;
        self.send(&format!("S{}{:06X}", axis, target))?;
        
        let r = self.send(&format!("J{}", axis))?;
        if r != "=" { return Err(format!("Go failed: {}", r)); }

        Ok(target)
    }

    pub fn wait_until_target(&mut self, axis: u8, target: u32) -> Result<(), String> {
        println!("      > [DEBUG] Monitoring Loop. Target: {}", target);
        
        loop {
            let current = self.get_position(axis)?;
            
            let diff = if target >= current { target - current } else { current - target };
            println!("      > [DEBUG] Current Position: {}, Target: {}, difference : {}", current, target, diff);
            let distance = if diff > (TOTAL_STEPS_24BIT / 2) {
                TOTAL_STEPS_24BIT - diff
            } else {
                diff
            };

            if distance < 20000 {
                println!("      > [STOP] Threshold reached! Stopping.");
                self.stop(axis)?;
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }

        print!("      > [WAIT] Waiting for motor stop...");
        let start_wait = Instant::now();
        loop {
            if start_wait.elapsed().as_secs() > 3 { break; }
            if !self.is_moving(axis)? { 
                println!(" Done.");
                break; 
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

pub fn start_sidereal_tracking(&mut self, axis: u8) -> Result<(), String> {
        println!("      > [DEBUG] Starting Sidereal Tracking (Axis {})", axis);

        // 1. Ensure motor is fully stopped
        self.stop(axis)?;
        loop {
            if !self.is_moving(axis)? { break; }
            std::thread::sleep(Duration::from_millis(100));
        }

        // 2. Clear Ramp/Break Steps (U=0)
        // We do this first so it applies to the upcoming move calculation
        self.send(&format!("U{}{:06X}", axis, 0))?;

        // 3. Set Direction: 'H' (Reverse / West)
        self.send(&format!("H{}", axis))?;

        // 4. Set Speed (I) -- CRITICAL: MUST BE BEFORE 'S' (Target)
        // If we set Target first, the controller calculates ramps based on the OLD speed!
        let steps_per_sec = TOTAL_STEPS_24BIT as f64 / SIDEREAL_DAY_SEC;
        let clock_divisor = (INTERNAL_CLOCK_FREQ / steps_per_sec).round() as u32;
        self.send(&format!("I{}{:06X}", axis, clock_divisor))?;

        // 5. Set Target (S)
        // Now that Speed and Direction are correct, we set the destination.
        // We target ~40 degrees "behind" us.
        let current = self.get_position(axis)?;
        let delta: i32 = 200_000; 
        
        // Calculate target in Reverse direction
        let target = (current as i64 - delta as i64).rem_euclid(TOTAL_STEPS_24BIT as i64) as u32;
        
        self.send(&format!("S{}{:06X}", axis, target))?;

        // 6. Start (J)
        let r = self.send(&format!("J{}", axis))?;
        if r != "=" {
            return Err(format!("Start tracking failed: {}", r));
        }

        println!("      > [DEBUG] Tracking Started. Target: {}", target);
        Ok(())
    }
}
