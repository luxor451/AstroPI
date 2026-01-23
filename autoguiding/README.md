# Rust Autoguider

A telescope autoguiding system implementation in Rust, project for the computer vision class.

## Overview

This project implements a real-time autoguiding system that tracks stars and corrects telescope mount drift. It features:

- **Multi-star tracking** for improved accuracy
- **Web-based UI** for monitoring and control
- **Three star centroiding algorithms** for comparison
- **Closed-loop control** for smooth corrections
- **Simulated mount** with periodic error and drift for testing

## Project Structure

```
autoguiding/
├── src/
│   ├── main.rs          # Web server, simulation loop, WebSocket handling
│   ├── guider.rs        # PID controller, guide star selection
│   ├── find_star.rs     # Star centroiding algorithms (main focus)
│   ├── simulator.rs     # Simulated camera and mount
│   └── traits.rs        # Camera and MountDriver traits
│   └── const.rs         # Constant to configure
├── index.html           # Web UI dashboard
└── sky_map.jpg          # Test star field image (Image of M13 by NASA)
```

## Star Centroiding Algorithms (`src/find_star.rs`)

The core of this project implements three star centroiding algorithms with different accuracy/speed trade-offs:

### 1. Center of Gravity (CG) - `find_star_CG()`

The simplest and fastest algorithm. Computes the intensity-weighted centroid:

$$\bar{x} = \frac{\sum_{i} x_i \cdot I_i}{\sum_{i} I_i}, \quad \bar{y} = \frac{\sum_{i} y_i \cdot I_i}{\sum_{i} I_i}$$

**Pros:** Very fast (~10-400 µs)  
**Cons:** Sensitive to noise, bias from asymmetric PSF

### 2. Gaussian Fitting (GF) - `find_star_GF()`

Fits a 2D Gaussian to the star's Point Spread Function (PSF) using iterative optimization:

$$S(x, y) = A \cdot \exp\left(-\frac{(x - x_c)^2}{2\sigma_x^2} - \frac{(y - y_c)^2}{2\sigma_y^2}\right)$$

Uses the **Nelder-Mead** simplex method to minimize the residual between the model and observed pixel values.

**Pros:** Sub-pixel accuracy, robust to noise  
**Cons:** Slower (~3000-5000 µs), may not converge

### 3. Fast Gaussian Fitting (FGF) - `find_star_FGF()`

Based on the paper:  
> [**"Star Centroiding Based on Fast Gaussian Fitting for Star Sensors"**](https://www.mdpi.com/1424-8220/18/9/2836)

This algorithm achieves the accuracy of Gaussian fitting with much better performance by using a **closed-form approximation** instead of iterative optimization.

**Pros:** Fast (~50-100 µs) AND accurate (sub-pixel)  
**Cons:** Requires good initial estimate

## Running the Project

##### Prerequisite

Have Rust and cargo install, for this go to [rust-lang.org](https://rust-lang.org/tools/install/)

```bash
# download the project
git clone https://github.com/luxor451/AstroPI.git
# Switch to right branch
git switch computer_vision_exam
# access the right folder
cd autoguiding

# Build and run
cargo run --release

# Open browser to
http://127.0.0.1:3000
```

## Web UI Features

- **Live camera feed** with star overlay markers
- **Start/Stop guiding** button control
- **Real-time RMS tracking** error display
- **Algorithm comparison** with timing and position data
- **Zoomed views** of primary guide star for each algorithm

## Dependencies

- `axum` - Web server framework
- `tokio` - Async runtime
- `image` - Image processing
- `argmin` - Optimization library (for GF algorithm)
- `nalgebra` - Linear algebra (for FGF matrix operations)
- `serde` - JSON serialization