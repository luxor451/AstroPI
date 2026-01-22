use crate::traits::{Camera, MountDriver};
use image::GenericImageView;
use image::Pixel;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::f64::consts::PI;
use crate::r#const::{CAM_SCALE};
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
            pe_amplitude: 15.0, // Large error to visualize clearly
            pe_period: 300.0,
            drift_per_step: 0.05, // Fast drift for visibility
        }
    }

    pub fn update(&mut self) {
        self.time_step += 1.0;
        // Periodic Error Simulation (Sine Wave only)
        let pe = (self.time_step / self.pe_period * 2.0 * PI).sin() * self.pe_amplitude;
        // Apply error to X axis (RA)
        self.x += pe * 0.02 + self.drift_per_step * 0.2;
        // Small drift on Y axis (DEC) for realism
        self.y += self.drift_per_step * 0.8;
    }
}

impl MountDriver for SimMount {
    // RA affects X axis (East-West direction)
    fn guide_ra(&mut self, nb: f64) {
        self.x += nb;
    }
    // Dec affects Y axis (North-South direction)
    fn guide_dec(&mut self, nb: f64) {
        self.y += nb;
    }
    fn get_position(&self) -> (f64, f64) {
        (self.x, self.y)
    }
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
        let rng = StdRng::seed_from_u64(0b01110011011001010110000101101100);
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
        // 2x upscaling: output is 2x larger, so 0.5 mount pixel movement = 1 camera pixel
        let out_w = self.fov_w * CAM_SCALE as u32;
        let out_h = self.fov_h * CAM_SCALE as u32;

        // Calculate the top-left corner in sky image coordinates (floating point)
        let top_left_x = center_x - (self.fov_w as f64 / 2.0);
        let top_left_y = center_y - (self.fov_h as f64 / 2.0);

        let mut rng = self.rng.borrow_mut();
        let brightness_factor = 1.0 + rng.random_range(-0.1..0.1); // ±10% brightness variation

        let mut buffer = Vec::with_capacity((out_w * out_h) as usize);

        let (img_w, img_h) = self.sky_image.dimensions();

        for out_y in 0..out_h {
            for out_x in 0..out_w {
                // Map output pixel back to sky image coordinates (with sub-pixel precision)
                let sky_x = top_left_x + (out_x as f64 / CAM_SCALE as u32 as f64);
                let sky_y = top_left_y + (out_y as f64 / CAM_SCALE as u32 as f64);

                // Bilinear interpolation
                let x0 = sky_x.floor() as i32;
                let y0 = sky_y.floor() as i32;
                let x1 = x0 + 1;
                let y1 = y0 + 1;

                let fx = sky_x - x0 as f64;
                let fy = sky_y - y0 as f64;

                // Sample 4 neighboring pixels (with bounds checking)
                let sample = |ix: i32, iy: i32| -> f64 {
                    if ix < 0 || iy < 0 || ix >= img_w as i32 || iy >= img_h as i32 {
                        0.0
                    } else {
                        self.sky_image.get_pixel(ix as u32, iy as u32).to_luma()[0] as f64
                    }
                };

                let p00 = sample(x0, y0);
                let p10 = sample(x1, y0);
                let p01 = sample(x0, y1);
                let p11 = sample(x1, y1);

                // Bilinear interpolation formula
                let pixel_value = p00 * (1.0 - fx) * (1.0 - fy)
                    + p10 * fx * (1.0 - fy)
                    + p01 * (1.0 - fx) * fy
                    + p11 * fx * fy;

                let varied = pixel_value * brightness_factor;
                let noise = rng.random_range(-20.0..20.0); // High noise
                let noisy = varied + noise;
                let final_value = noisy.clamp(0.0, 255.0) as u8;

                buffer.push(final_value);
            }
        }
        (out_w, out_h, buffer)
    }
}
