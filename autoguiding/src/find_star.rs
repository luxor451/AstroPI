use crate::guider::*;
use argmin::core::{CostFunction, Error, Executor, State};
use argmin::solver::neldermead::NelderMead;
use nalgebra::{Matrix5, Vector5};

fn get_window(
    width: u32,
    height: u32,
    pixels: &[u8],
    window_size: u32,
    old_star_pos: StarPosition,
) -> Vec<Vec<u8>> {
    // Use rounding instead of truncation to center window on star position
    let star_window = (
        (old_star_pos.x.round() as i32 - (window_size / 2) as i32).max(0) as u32,
        (old_star_pos.y.round() as i32 - (window_size / 2) as i32).max(0) as u32,
        window_size,
        window_size,
    );

    let mut matrix = Vec::with_capacity(window_size as usize);
    for y in star_window.1..(star_window.1 + star_window.3) {
        let mut row = Vec::with_capacity(window_size as usize);
        for x in star_window.0..(star_window.0 + star_window.2) {
            if x >= width || y >= height {
                row.push(0);
                continue;
            }
            let pixel_idx = (y * width + x) as usize;
            row.push(pixels[pixel_idx]);
        }
        matrix.push(row);
    }
    matrix
}

fn exp(x: f64) -> f64 {
    return x.exp();
}

#[allow(non_snake_case)]
fn S(x_i: u32, y_i: u32, v: (f64, f64, f64, f64, f64)) -> f64 {
    let (A, x_c, y_c, sigma_x, sigma_y) = v;
    return A * exp(-((x_i as f64 - x_c).powi(2) / (2.0 * sigma_x.powi(2)))
        - ((y_i as f64 - y_c).powi(2) / (2.0 * sigma_y.powi(2))));
}

fn z_i(
    x_i: u32,
    y_i: u32,
    v: (f64, f64, f64, f64, f64),
    window_pixels: &[Vec<u8>],
    threshold: u8,
) -> f64 {
    let measured = (window_pixels[y_i as usize][x_i as usize] as f64 - threshold as f64).max(0.0);
    return measured - S(x_i, y_i, v);
}

#[allow(non_snake_case)]
fn obj_function(
    v: (f64, f64, f64, f64, f64),
    window_pixels: &[Vec<u8>],
    window_size: u32,
    threshold: u8,
) -> f64 {
    let mut sum = 0.0;
    for y_i in 0..window_size {
        for x_i in 0..window_size {
            let z = z_i(x_i, y_i, v, window_pixels, threshold);
            sum += z * z;
        }
    }
    return sum;
}

// Optimization problem for argmin
struct GaussianFitProblem {
    window_pixels: Vec<Vec<u8>>,
    window_size: u32,
    threshold: u8,
}

impl CostFunction for GaussianFitProblem {
    type Param = Vec<f64>;
    type Output = f64;

    fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
        let v = (p[0], p[1], p[2], p[3], p[4]);
        Ok(obj_function(
            v,
            &self.window_pixels,
            self.window_size,
            self.threshold,
        ))
    }
}

impl Guider {
    #[allow(non_snake_case)]
    pub fn find_star_CG(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
        old_star_pos: StarPosition,
    ) -> Option<StarPosition> {
        // This need to be big enough so that the star does not move out of the window between frames
        let window_size = 40;
        let window_pixels = get_window(width, height, pixels, window_size as u32, old_star_pos);

        let mut x_c = 0.0;
        let mut y_c = 0.0;
        let mut sum_I = 0.0;

        for y in 0..window_size {
            for x in 0..window_size {
                let I_i = window_pixels[y][x] as f64; // Fixed: row first, then column

                if I_i > self.threshold as f64 {
                    let intensity = I_i - self.threshold as f64;
                    x_c += (x as f64) * intensity;
                    y_c += (y as f64) * intensity;
                    sum_I += intensity;
                }
            }
        }

        if sum_I == 0.0 {
            return None; // No bright pixels found
        }

        x_c = x_c / sum_I;
        y_c = y_c / sum_I;

        Some(StarPosition {
            x: old_star_pos.x - (window_size as f64 / 2.0) + x_c,
            y: old_star_pos.y - (window_size as f64 / 2.0) + y_c,
        })
    }

    #[allow(non_snake_case)]
    pub fn find_star_GF(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
        old_star_pos: StarPosition,
    ) -> Option<StarPosition> {
        // This need to be big enough so that the star does not move out of the window between frames
        let window_size: u32 = 40;
        let window_pixels = get_window(width, height, pixels, window_size, old_star_pos);

        // Step 1: Find max intensity for amplitude estimate
        let mut max_I: f64 = 0.0;
        for y in 0..window_size {
            for x in 0..window_size {
                let pixel_val = window_pixels[y as usize][x as usize] as f64;
                if pixel_val > max_I {
                    max_I = pixel_val;
                }
            }
        }

        // Step 2: Initialize Gaussian parameters using initial guess or window center
        // If we have previous GF result, use it as starting point (in window coordinates)
        let (x_c, y_c) = (window_size as f64 / 2.0, window_size as f64 / 2.0);

        // v = (A, x_c, y_c, sigma_x, sigma_y)
        let A_init = max_I - self.threshold as f64;
        let sigma_init = 2.0; // Typical star PSF width
        let init_params = vec![A_init, x_c, y_c, sigma_init, sigma_init];

        // Step 3: Use Nelder-Mead solver to minimize obj_function
        let problem = GaussianFitProblem {
            window_pixels: window_pixels.clone(),
            window_size,
            threshold: self.threshold,
        };

        // Create initial simplex with 6 vertices for 5D optimization
        let delta = 0.2; // Smaller delta for finer convergence
        let simplex = vec![
            init_params.clone(),
            vec![A_init * 1.02, x_c, y_c, sigma_init, sigma_init],
            vec![A_init, x_c + delta, y_c, sigma_init, sigma_init],
            vec![A_init, x_c, y_c + delta, sigma_init, sigma_init],
            vec![A_init, x_c, y_c, sigma_init * 1.05, sigma_init],
            vec![A_init, x_c, y_c, sigma_init, sigma_init * 1.05],
        ];

        let solver = match NelderMead::new(simplex).with_sd_tolerance(1e-6) {
            Ok(s) => s,
            Err(_) => return None,
        };

        let result = Executor::new(problem, solver)
            .configure(|state| state.param(init_params).max_iters(300)) // More iterations for better accuracy
            .run();

        // Extract refined x_c and y_c from optimized parameters
        let (x_c_opt, y_c_opt) = match result {
            Ok(res) => {
                let params = res.state().get_best_param().unwrap();
                (params[1], params[2])
            }
            Err(_) => {
                return None; // Optimization failed
            }
        };

        // Use the optimized position directly without smoothing
        // Smoothing was biasing results toward window center
        Some(StarPosition {
            x: old_star_pos.x - (window_size as f64 / 2.0) + x_c_opt,
            y: old_star_pos.y - (window_size as f64 / 2.0) + y_c_opt,
        })
    }

    // Fast Gaussian Fitting - auxiliary function implementing closed-form solution
    #[allow(non_snake_case)]
    fn fast_gaussian_fit(
        &self,
        window_pixels: &[Vec<u8>],
        window_size: u32,
        U: &[(usize, usize)],
    ) -> Option<(f64, f64, f64, f64, f64)> {
        // Build the linear system for least squares: solve for m, n, p, q, k
        // H = sum of (m*I*x^2 + n*I*y^2 + p*I*x + q*I*y + k*I + I*ln(I))^2
        // Taking partial derivatives and setting to 0 gives us 5 linear equations
        // Only use the brightest pixels (U) for fitting

        let mut sum_Ix2_Ix2 = 0.0; // sum(I*x^2 * I*x^2)
        let mut sum_Ix2_Iy2 = 0.0; // sum(I*x^2 * I*y^2)
        let mut sum_Ix2_Ix = 0.0; // sum(I*x^2 * I*x)
        let mut sum_Ix2_Iy = 0.0; // sum(I*x^2 * I*y)
        let mut sum_Ix2_I = 0.0; // sum(I*x^2 * I)
        let mut sum_Ix2_h = 0.0; // sum(I*x^2 * (I*ln(I)))

        let mut sum_Iy2_Iy2 = 0.0;
        let mut sum_Iy2_Ix = 0.0;
        let mut sum_Iy2_Iy = 0.0;
        let mut sum_Iy2_I = 0.0;
        let mut sum_Iy2_h = 0.0;

        let mut sum_Ix_Ix = 0.0;
        let mut sum_Ix_Iy = 0.0;
        let mut sum_Ix_I = 0.0;
        let mut sum_Ix_h = 0.0;

        let mut sum_Iy_Iy = 0.0;
        let mut sum_Iy_I = 0.0;
        let mut sum_Iy_h = 0.0;

        let mut sum_I_I = 0.0;
        let mut sum_I_h = 0.0;

        // Accumulate sums only over brightest pixels (U)
        for &(x, y) in U {
            if x >= window_size as usize || y >= window_size as usize {
                continue;
            }

            let I_i = window_pixels[y][x] as f64;

            let x_f = x as f64;
            let y_f = y as f64;

            let ln_I = I_i.ln();
            let I_ln_I = I_i * ln_I;

            let Ix = I_i * x_f;
            let Iy = I_i * y_f;
            let Ix2 = I_i * x_f * x_f;
            let Iy2 = I_i * y_f * y_f;

            // Build symmetric matrix entries
            sum_Ix2_Ix2 += Ix2 * Ix2;
            sum_Ix2_Iy2 += Ix2 * Iy2;
            sum_Ix2_Ix += Ix2 * Ix;
            sum_Ix2_Iy += Ix2 * Iy;
            sum_Ix2_I += Ix2 * I_i;
            sum_Ix2_h += Ix2 * I_ln_I;

            sum_Iy2_Iy2 += Iy2 * Iy2;
            sum_Iy2_Ix += Iy2 * Ix;
            sum_Iy2_Iy += Iy2 * Iy;
            sum_Iy2_I += Iy2 * I_i;
            sum_Iy2_h += Iy2 * I_ln_I;

            sum_Ix_Ix += Ix * Ix;
            sum_Ix_Iy += Ix * Iy;
            sum_Ix_I += Ix * I_i;
            sum_Ix_h += Ix * I_ln_I;

            sum_Iy_Iy += Iy * Iy;
            sum_Iy_I += Iy * I_i;
            sum_Iy_h += Iy * I_ln_I;

            sum_I_I += I_i * I_i;
            sum_I_h += I_i * I_ln_I;
        }

        // Solve the 5x5 linear system using nalgebra
        // Matrix: A * [m, n, p, q, k]^T = b
        #[rustfmt::skip]
        let a_matrix = Matrix5::new(
            sum_Ix2_Ix2, sum_Ix2_Iy2, sum_Ix2_Ix, sum_Ix2_Iy, sum_Ix2_I,
            sum_Ix2_Iy2, sum_Iy2_Iy2, sum_Iy2_Ix, sum_Iy2_Iy, sum_Iy2_I,
            sum_Ix2_Ix,  sum_Iy2_Ix,  sum_Ix_Ix,  sum_Ix_Iy,  sum_Ix_I,
            sum_Ix2_Iy,  sum_Iy2_Iy,  sum_Ix_Iy,  sum_Iy_Iy,  sum_Iy_I,
            sum_Ix2_I,   sum_Iy2_I,   sum_Ix_I,   sum_Iy_I,   sum_I_I,
        );

        let b_vector = Vector5::new(-sum_Ix2_h, -sum_Iy2_h, -sum_Ix_h, -sum_Iy_h, -sum_I_h);

        // Solve using LU decomposition
        let lu = a_matrix.lu();
        let solution = lu.solve(&b_vector)?;

        let m = solution[0];
        let n = solution[1];
        let p = solution[2];
        let q = solution[3];
        let k = solution[4];

        // Convert to Gaussian parameters
        let x_c = -p / (2.0 * m);
        let y_c = -q / (2.0 * n);
        let sigma_x = 1.0 / ((2.0 * m.abs()).sqrt());
        let sigma_y = 1.0 / ((2.0 * n.abs()).sqrt());
        let A = exp(p.powi(2) / (4.0 * m) + q.powi(2) / (4.0 * n) - k);

        Some((x_c, y_c, sigma_x, sigma_y, A))
    }

    #[allow(non_snake_case)]
    fn S_i(x_i: f64, y_i: f64, A: f64, x_c: f64, y_c: f64, sigma_x: f64, sigma_y: f64) -> f64 {
        let res = A * exp(-((x_i as f64 - x_c).powi(2) / (2.0 * sigma_x.powi(2)))
            - ((y_i as f64 - y_c).powi(2) / (2.0 * sigma_y.powi(2))));
        res
    }

    #[allow(non_snake_case)]
    fn N_i(
        x_i: f64,
        y_i: f64,
        I: &[Vec<u8>],
        A: f64,
        x_c: f64,
        y_c: f64,
        sigma_x: f64,
        sigma_y: f64,
    ) -> f64 {
        return (I[y_i as usize][x_i as usize] as f64)
            - Self::S_i(x_i, y_i, A, x_c, y_c, sigma_x, sigma_y);
    }

    #[allow(non_snake_case)]
    fn SNR_i(
        x_i: f64,
        y_i: f64,
        window_pixels: &[Vec<u8>],
        A: f64,
        x_c: f64,
        y_c: f64,
        sigma_x: f64,
        sigma_y: f64,
    ) -> f64 {
        let S = Self::S_i(x_i, y_i, A, x_c, y_c, sigma_x, sigma_y);
        let N = Self::N_i(x_i, y_i, window_pixels, A, x_c, y_c, sigma_x, sigma_y);
        if N == 0.0 {
            return 0.0;
        }
        return (S / N).abs();
    }

    #[allow(non_snake_case)]
    pub fn find_star_FGF(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
        old_star_pos: StarPosition,
        T: u32,
    ) -> Option<StarPosition> {
        // Increased window size to handle larger displacements during multi-star tracking
        let window_size: u32 = 31;

        // (1)
        let window_pixels = get_window(width, height, pixels, window_size, old_star_pos);

        // (2) Find positions of brightest pixels above threshold
        let mut pixel_positions: Vec<(u8, usize, usize)> = Vec::new();
        for y in 0..window_size as usize {
            for x in 0..window_size as usize {
                let intensity = window_pixels[y][x];
                if intensity > self.threshold {
                    pixel_positions.push((intensity, x, y));
                }
            }
        }

        // Need at least some pixels for fitting
        if pixel_positions.len() < 10 {
            return None; // Not enough bright pixels
        }

        // (2) (i) Use top 30% of bright pixels for initial fit (more robust than just 5)
        pixel_positions.sort_unstable_by(|a, b| b.0.cmp(&a.0)); // Descending order
        let num_pixels_for_fit = (pixel_positions.len() * 30 / 100).max(10).min(50);
        let U: Vec<(usize, usize)> = pixel_positions
            .iter()
            .take(num_pixels_for_fit)
            .map(|(_, x, y)| (*x, *y))
            .collect();

        // (2) (ii) Initial Gaussian fit
        let (x_c, y_c, _sigma_x, _sigma_y, _A) =
            match self.fast_gaussian_fit(&window_pixels, window_size, &U) {
                Some(params) => params,
                None => return None,
            };

        // (3) (i)
        // Refine using only pixels with SNR > T
        let mut V: Vec<(usize, usize)> = Vec::new();
        for y in 0..window_size as usize {
            for x in 0..window_size as usize {
                let snr = Self::SNR_i(
                    x as f64,
                    y as f64,
                    &window_pixels,
                    _A,
                    x_c,
                    y_c,
                    _sigma_x,
                    _sigma_y,
                );
                if snr > T as f64 {
                    V.push((x, y));
                }
            }
        }

        // (3) (ii) Refinement step - only if we have enough pixels
        let (x_c_final, y_c_final) = if V.len() >= 10 {
            // Enough pixels for refinement
            match self.fast_gaussian_fit(&window_pixels, window_size, &V) {
                Some((x_c_refined, y_c_refined, _, _, _)) => {
                    // Validate refined position is reasonable (within window)
                    if x_c_refined >= 0.0
                        && x_c_refined < window_size as f64
                        && y_c_refined >= 0.0
                        && y_c_refined < window_size as f64
                    {
                        (x_c_refined, y_c_refined)
                    } else {
                        // Refined position is out of bounds, use initial fit
                        (x_c, y_c)
                    }
                }
                None => {
                    // Refinement failed, use initial fit
                    (x_c, y_c)
                }
            }
        } else {
            // Not enough pixels for refinement, use initial fit
            (x_c, y_c)
        };

        // Calculate actual window origin (must match get_window logic)
        let window_origin_x =
            (old_star_pos.x.round() as i32 - (window_size / 2) as i32).max(0) as f64;
        let window_origin_y =
            (old_star_pos.y.round() as i32 - (window_size / 2) as i32).max(0) as f64;

        Some(StarPosition {
            x: window_origin_x + x_c_final,
            y: window_origin_y + y_c_final,
        })
    }
}
