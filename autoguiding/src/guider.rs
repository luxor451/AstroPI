use crate::r#const::{
    FILTER_CUTOFF_FREQ_DEC, FILTER_CUTOFF_FREQ_RA, INTEGRAL_LIMIT_DEC, INTEGRAL_LIMIT_RA, KD_DEC,
    KD_RA, KI_DEC, KI_RA, KP_DEC, KP_RA,
};
use crate::T;
#[derive(Debug, Clone, Copy)]
pub struct StarPosition {
    pub x: f64,
    pub y: f64,
}

pub struct Guider {
    pub threshold: u8,
    pub guide_stars: Vec<StarPosition>,
    pub initial_guide_stars_average: StarPosition,
    // PID state
    prev_error_ra: f64,
    prev_error_dec: f64,
    integral_ra: f64,
    integral_dec: f64,
    last_time: Option<std::time::Instant>,
    // Low-pass filter state
    filtered_error_ra: f64,
    filtered_error_dec: f64,
}

fn derivatives(x_t: f64, x_t_minus_1: f64, dt: f64) -> f64 {
    (x_t - x_t_minus_1) / dt
}

fn integrals(prev_results: f64, x_t: f64, x_t_minus_1: f64, dt: f64) -> f64 {
    prev_results + (x_t + x_t_minus_1) * dt / 2.0
}

/// First-order low-pass filter (exponential moving average)
/// filtered_prev: previous filtered value
/// raw_value: new raw measurement
/// dt: time step
/// cutoff_freq: cutoff frequency in Hz
fn low_pass_filter(filtered_prev: f64, raw_value: f64, dt: f64, cutoff_freq: f64) -> f64 {
    // Calculate filter coefficient (alpha)
    // alpha = dt / (dt + RC), where RC = 1 / (2 * pi * cutoff_freq)
    let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff_freq);
    let alpha = dt / (dt + rc);

    // Apply exponential moving average
    alpha * raw_value + (1.0 - alpha) * filtered_prev
}

fn pid(
    kp: f64,
    ki: f64,
    kd: f64,
    error: f64,
    prev_error: f64,
    integral: f64,
    dt: f64,
) -> (f64, f64) {
    let derivative = derivatives(error, prev_error, dt);
    let new_integral = integrals(integral, error, prev_error, dt);
    let correction = kp * error + ki * new_integral + kd * derivative;
    (correction, new_integral)
}

impl Guider {
    pub fn new() -> Self {
        Self {
            threshold: 50,
            guide_stars: Vec::new(),
            initial_guide_stars_average: StarPosition { x: 0.0, y: 0.0 },
            prev_error_ra: 0.0,
            prev_error_dec: 0.0,
            integral_ra: 0.0,
            integral_dec: 0.0,
            last_time: None,
            filtered_error_ra: 0.0,
            filtered_error_dec: 0.0,
        }
    }

    /// Multi-star guiding: calculate correction based on average of multiple star positions
    /// tracked_stars: current positions of the stars being tracked
    /// This function expects the caller to provide the displacement/error for each star
    pub fn calculate_correction_multi_star(
        &mut self,
        tracked_stars: &[StarPosition],
    ) -> (f64, f64) {
        if tracked_stars.is_empty() {
            return (0.0, 0.0);
        }

        // Calculate average displacement across all tracked stars
        // The displacement should already be calculated by the caller (current_pos - initial_pos)
        let mut average_pos_x = 0.0;
        let mut average_pos_y = 0.0;
        let n = tracked_stars.len();

        for stars in tracked_stars {
            average_pos_x += stars.x;
            average_pos_y += stars.y;
        }

        average_pos_x /= n as f64;
        average_pos_y /= n as f64;

        // These are the RAW errors (displacement from where stars should be)
        let raw_error_ra = average_pos_x;
        let raw_error_dec = average_pos_y;

        // Calculate dt (time since last correction)
        let now = std::time::Instant::now();
        let dt = if let Some(last) = self.last_time {
            now.duration_since(last).as_secs_f64()
        } else {
            0.1 // Default dt for first iteration
        };
        self.last_time = Some(now);

        // Apply low-pass filter to error signals to reduce noise
        // Use different cutoff frequencies for RA and DEC
        // On first iteration, initialize filter to current value to avoid transient
        if self.filtered_error_ra == 0.0
            && self.filtered_error_dec == 0.0
            && (raw_error_ra.abs() > 0.01 || raw_error_dec.abs() > 0.01)
        {
            // First real measurement - initialize filters to avoid slow ramp-up
            self.filtered_error_ra = raw_error_ra;
            self.filtered_error_dec = raw_error_dec;
        } else {
            self.filtered_error_ra = low_pass_filter(
                self.filtered_error_ra,
                raw_error_ra,
                dt,
                FILTER_CUTOFF_FREQ_RA,
            );
            self.filtered_error_dec = low_pass_filter(
                self.filtered_error_dec,
                raw_error_dec,
                dt,
                FILTER_CUTOFF_FREQ_DEC,
            );
        }

        // Use filtered errors for PID calculations
        let error_ra = self.filtered_error_ra;
        let error_dec = self.filtered_error_dec;

        // RA PID calculation using filtered error
        let (correction_ra, new_integral_ra) = pid(
            KP_RA,
            KI_RA,
            KD_RA,
            error_ra,
            self.prev_error_ra,
            self.integral_ra,
            dt,
        );

        // DEC PID calculation using filtered error
        let (correction_dec, new_integral_dec) = pid(
            KP_DEC,
            KI_DEC,
            KD_DEC,
            error_dec,
            self.prev_error_dec,
            self.integral_dec,
            dt,
        );

        self.prev_error_ra = error_ra;
        self.prev_error_dec = error_dec;
        // Apply anti-windup: clamp integral term to prevent it from growing unbounded
        self.integral_ra = new_integral_ra.clamp(-INTEGRAL_LIMIT_RA, INTEGRAL_LIMIT_RA);
        self.integral_dec = new_integral_dec.clamp(-INTEGRAL_LIMIT_DEC, INTEGRAL_LIMIT_DEC);

        (correction_ra, correction_dec)
    }

    /// Find n isolated bright stars suitable for multi-star guiding
    /// Returns the positions of stars that are bright and well-isolated from other stars
    pub fn select_guide_star(
        &mut self,
        width: u32,
        height: u32,
        pixels: &[u8],
        n: usize,
    ) -> Vec<StarPosition> {
        let isolation_radius = 50.0; // Minimum distance from other bright regions
        let min_brightness = self.threshold + 100; // Star should be quite bright
        let edge_margin = 40.0; // Minimum distance from frame edges
        let min_selected_separation = 80.0; // Minimum distance between selected stars


        // Find all bright pixel clusters (potential stars)
        let mut star_candidates: Vec<(f64, f64, f64)> = Vec::new(); // (x, y, total_brightness)

        // Use a simple flood-fill approach to find distinct bright regions
        let mut visited = vec![false; pixels.len()];

        for i in 0..pixels.len() {
            if visited[i] || pixels[i] < min_brightness {
                continue;
            }

            // Found a bright pixel - analyze the region
            let mut region_mass = 0.0;
            let mut region_mx = 0.0;
            let mut region_my = 0.0;
            let mut region_peak = 0u8;
            let mut region_pixels: Vec<(f64, f64, u8)> = Vec::new(); // Store all pixels in region
            let mut stack = vec![i];

            while let Some(idx) = stack.pop() {
                if visited[idx] {
                    continue;
                }
                visited[idx] = true;

                let val = pixels[idx];
                if val < self.threshold {
                    continue;
                }

                let x = (idx as u32 % width) as f64;
                let y = (idx as u32 / width) as f64;
                let intensity = (val - self.threshold) as f64;

                region_mass += intensity;
                region_mx += x * intensity;
                region_my += y * intensity;
                region_peak = region_peak.max(val);
                region_pixels.push((x, y, val));

                // Add neighbors to stack (8-connected)
                let ix = idx as i32 % width as i32;
                let iy = idx as i32 / width as i32;

                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = ix + dx;
                        let ny = iy + dy;
                        if nx >= 0 && nx < width as i32 && ny >= 0 && ny < height as i32 {
                            let nidx = (ny * width as i32 + nx) as usize;
                            if !visited[nidx] && pixels[nidx] > self.threshold {
                                stack.push(nidx);
                            }
                        }
                    }
                }
            }

            // If this region is significant enough, check if it's a single star
            if region_mass > 500.0 && region_peak >= min_brightness {
                let cx = region_mx / region_mass;
                let cy = region_my / region_mass;

                // Check region compactness (variance from centroid)
                // Double stars will have higher spread than single stars
                let mut variance = 0.0;
                for &(px, py, _) in &region_pixels {
                    variance += (px - cx).powi(2) + (py - cy).powi(2);
                }
                variance /= region_pixels.len().max(1) as f64;
                let std_dev = variance.sqrt();

                // Reject very spread out regions (likely double stars or extended objects)
                // Single stars with upscaling typically have std_dev < 15
                if std_dev < 15.0 {
                    star_candidates.push((cx, cy, region_mass));
                }
            }
        }

        // Find the best isolated stars (prioritizing brightness)
        let mut scored_stars: Vec<(f64, f64, f64)> = Vec::new(); // (x, y, score, brightness)

        for (i, &(x, y, brightness)) in star_candidates.iter().enumerate() {
            // Skip stars too close to edges
            if x < edge_margin
                || x > (width as f64 - edge_margin)
                || y < edge_margin
                || y > (height as f64 - edge_margin)
            {
                continue;
            }

            // Calculate isolation score (distance to nearest other star)
            let mut min_distance = f64::INFINITY;

            for (j, &(ox, oy, _)) in star_candidates.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dist = ((x - ox).powi(2) + (y - oy).powi(2)).sqrt();
                min_distance = min_distance.min(dist);
            }

            // Ignore stars too close to others
            if min_distance < isolation_radius {
                continue;
            }

            scored_stars.push((x, y, brightness));
        }

        // Sort by score (descending) - brightest stars first
        scored_stars.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

        // Greedy selection: ensure minimum separation between SELECTED stars
        let mut selected_stars: Vec<(f64, f64, f64)> = Vec::new(); // (x, y, brightness)

        for (x, y, brightness) in &scored_stars {
            // Check if this star is far enough from all already-selected stars
            let mut too_close = false;
            for (sx, sy, _) in &selected_stars {
                let dist = ((x - sx).powi(2) + (y - sy).powi(2)).sqrt();
                if dist < min_selected_separation {
                    too_close = true;
                    break;
                }
            }

            if !too_close {
                selected_stars.push((*x, *y, *brightness));
                if selected_stars.len() >= n {
                    break;
                }
            }
        }

        // Use CG positions as initial estimates
        let cg_positions: Vec<StarPosition> = selected_stars
            .iter()
            .map(|(x, y, _)| StarPosition { x: *x, y: *y })
            .collect();

        // Refine each star position using FGF for consistency with tracking algorithm
        // This ensures initial positions match what FGF will find during tracking
        self.guide_stars = cg_positions
            .iter()
            .filter_map(|cg_pos| {
                self.find_star_FGF(width, height, pixels, *cg_pos, T)
            })
            .collect();

        // Store initial positions
        self.initial_guide_stars_average = {
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            for star in &self.guide_stars {
                sum_x += star.x;
                sum_y += star.y;
            }
            let count = self.guide_stars.len() as f64;
            StarPosition {
                x: sum_x / count,
                y: sum_y / count,
            }
        };

        println!(
            "✓ Selected {} guide stars for multi-star tracking (refined with FGF)",
            self.guide_stars.len()
        );
        for (i, star) in self.guide_stars.iter().enumerate() {
            println!("  Star {}: ({:.2}, {:.2})", i + 1, star.x, star.y);
        }

        self.guide_stars.clone()
    }
}
