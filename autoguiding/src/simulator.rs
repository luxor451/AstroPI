// src/simulator.rs
use crate::traits::{MountDriver, Camera};
use image::GenericImageView;
use std::f64::consts::PI;
use image::Pixel;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
pub struct SimMount {
    pub x: f64, 
    pub y: f64,
    time_step: f64,
    pe_amplitude: f64, 
    pe_period: f64,   
    drift_per_step: f64,
}

impl SimMount {
    pub fn new(start_x: f64, start_y: f64) -> Self {
        Self {
            x: start_x,
            y: start_y,
            time_step: 0.0,
            pe_amplitude: 2.0, // Large error to visualize clearly
            pe_period: 300.0,
            drift_per_step: 1.0,
        }
    }

    pub fn update(&mut self) {
        self.time_step += 1.0;
        // Periodic Error Simulation (Sine Wave)
        let pe = (self.time_step / self.pe_period * 2.0 * PI).sin() * self.pe_amplitude + self.drift_per_step ;
        // Apply error to X axis
        self.x += pe * 0.02; 
        self.y += self.drift_per_step * 0.01;
    }
}

impl MountDriver for SimMount {
    // RA affects X axis (East-West direction)
    fn guide_ra(&mut self, nb: f64) { self.x += nb; }
    // Dec affects Y axis (North-South direction)
    fn guide_dec(&mut self, nb: f64)  { self.y += nb; }
    fn get_position(&self) -> (f64, f64) { (self.x, self.y) }
}

pub struct SimCamera {
    sky_image: image::DynamicImage,
    fov_w: u32,
    fov_h: u32,
    rng: std::cell::RefCell<StdRng>,
}

impl SimCamera {
    pub fn new(path: &str, w: u32, h: u32) -> Self {
        let img = image::open(path).expect("Failed to open sky_map.jpg");
        // Use a fixed seed for deterministic behavior. Change seed value to get different noise patterns.
        let rng = StdRng::seed_from_u64(42);
        Self { 
            sky_image: img, 
            fov_w: w, 
            fov_h: h,
            rng: std::cell::RefCell::new(rng),
        }
    }
}

impl Camera for SimCamera {
    fn capture_frame(&self, center_x: f64, center_y: f64) -> (u32, u32, Vec<u8>) {
        let x = (center_x as u32).saturating_sub(self.fov_w / 2);
        let y = (center_y as u32).saturating_sub(self.fov_h / 2);
        let crop = self.sky_image.view(x, y, self.fov_w, self.fov_h);
        
        let mut rng = self.rng.borrow_mut();
        // Random brightness variation: ±2%
        let brightness_factor = 1.0 + rng.random_range(-0.02..0.02);
        
        let mut buffer = Vec::with_capacity((self.fov_w * self.fov_h) as usize);
        for p in crop.pixels() {
            let pixel_value = p.2.to_luma()[0] as f64;
            
            // Apply brightness variation
            let varied = pixel_value * brightness_factor;
            
            // Add random noise (Gaussian-like, ±2 intensity units)
            let noise = rng.random_range(-2.0..2.0);
            let noisy = varied + noise;
            
            // Clamp to valid range [0, 255]
            let final_value = noisy.clamp(0.0, 255.0) as u8;
            
            buffer.push(final_value);
        }
        (self.fov_w, self.fov_h, buffer)
    }
}