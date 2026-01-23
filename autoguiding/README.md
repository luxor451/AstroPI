# Rust Autoguider

A telescope autoguiding system implementation in Rust, project for the computer vision class.
The presentation slides are under `presentation/build/DOCHE_Dimitry_Computer_Vision_Exam`

## Overview

This project implements a real-time autoguiding system that tracks stars and corrects telescope mount drift. It features:

- **Multi-star tracking** for improved accuracy
- **Web-based UI** for monitoring and control
- **Three star centroiding algorithms** with live selection
- **Real-time algorithm switching** with automatic star re-calibration
- **Closed-loop PID control** with low-pass filtering for smooth corrections
- **Simulated mount** with periodic error and drift for testing

## Project Structure

```
autoguiding/
├── src/
│   ├── main.rs          # Web server, simulation loop, WebSocket handling
│   ├── guider.rs        # PID controller, guide star selection
│   ├── find_star.rs     # Star centroiding algorithms (main focus)
│   ├── simulator.rs     # Simulated camera and mount
│   ├── traits.rs        # Camera and MountDriver traits
│   └── const.rs         # Constants to configure (PID gains, thresholds, etc.)
├── index.html           # Web UI dashboard
└── sky_map.jpg          # Test star field image (Image of M13 by NASA)
```

## Star Centroiding Algorithms (`src/find_star.rs`)

The core of this project implements three star centroiding algorithms with different accuracy/speed trade-offs. You can switch between them in real-time via the UI dropdown.

### 1. Center of Gravity (CG) - `find_star_CG()`

The simplest and fastest algorithm. Computes the intensity-weighted centroid:

$$\bar{x} = \frac{\sum_{i} x_i \cdot I_i}{\sum_{i} I_i}, \quad \bar{y} = \frac{\sum_{i} y_i \cdot I_i}{\sum_{i} I_i}$$

where $I_i$ is the pixel intensity at position $(x_i, y_i)$.

**Pros:** Very fast (~10-400 µs)  
**Cons:** Sensitive to noise, bias from asymmetric PSF

### 2. Gaussian Fitting (GF) - `find_star_GF()`

Fits a 2D Gaussian to the star's Point Spread Function (PSF) using iterative optimization:

$$G(x, y) = A \cdot \exp\left(-\frac{(x - x_c)^2}{2\sigma_x^2} - \frac{(y - y_c)^2}{2\sigma_y^2}\right) + B$$

where $(x_c, y_c)$ is the star center, $A$ is the amplitude, $B$ is the background level, and $\sigma_x, \sigma_y$ are the Gaussian widths.

Uses the **Nelder-Mead** simplex method to minimize the sum of squared residuals:

$$\min_{x_c, y_c, A, \sigma_x, \sigma_y, B} \sum_{i,j} \left( I_{i,j} - G(x_i, y_j) \right)^2$$

**Pros:** Sub-pixel accuracy, robust to noise  
**Cons:** Slower (~3000-5000 µs), may not converge

### 3. Fast Gaussian Fitting (FGF) - `find_star_FGF()`

Based on the paper:  
> [**"Star Centroiding Based on Fast Gaussian Fitting for Star Sensors"**](https://www.mdpi.com/1424-8220/18/9/2836)

This algorithm achieves the accuracy of Gaussian fitting with much better performance by using a **closed-form solution** instead of iterative optimization. It linearizes the Gaussian model by taking the logarithm:

$$\ln(I - B) = \ln(A) - \frac{(x - x_c)^2}{2\sigma_x^2} - \frac{(y - y_c)^2}{2\sigma_y^2}$$

This transforms into a linear least-squares problem $\mathbf{A}\mathbf{x} = \mathbf{b}$ that can be solved directly using matrix operations.

**Pros:** Fast (~50-100 µs) AND accurate (sub-pixel)  
**Cons:** Requires good initial estimate, sensitive to background estimation

## Running the Project

### Prerequisites

Have Rust and Cargo installed. Visit [rust-lang.org](https://rust-lang.org/tools/install/) for installation instructions.

```bash
# download the project
git clone https://github.com/luxor451/AstroPI.git

# Switch to right branch
cd AstroPI
git switch computer_vision_exam

# access the right folder
cd autoguiding

# Build and run
cargo run --release

# Open browser to
http://127.0.0.1:3000
```


## Web UI Features

- **Live camera feed** with numbered guide star markers
- **Start/Stop guiding** button controls
- **Algorithm selector** - Switch between CG, GF, and FGF in real-time
- **Automatic re-calibration** when algorithm changes during guiding
- **Real-time RMS tracking** error display (RA/DEC/Total)
- **Algorithm comparison panel** with timing and position data
- **Zoomed views** of primary guide star for each algorithm
- **Sky map overview** showing current field of view
- **Live telemetry console** with debug information

## Configuration (`src/const.rs`)

Key parameters you can tune:

- `NB_STARS` - Number of guide stars to track
- `KP_RA/KP_DEC` - PID proportional gains
- `KI_RA/KI_DEC` - PID integral gains  
- `KD_RA/KD_DEC` - PID derivative gains
- `MAX_CORRECTION` - Maximum correction per frame
- `MAX_DEVIATION` - Outlier rejection threshold

## Dependencies

- `axum` - Web server framework
- `tokio` - Async runtime
- `image` - Image processing
- `argmin` - Optimization library (for GF algorithm)
- `nalgebra` - Linear algebra (for FGF matrix operations)
- `serde` / `serde_json` - JSON serialization
- `base64` - Image encoding for WebSocket transfer
