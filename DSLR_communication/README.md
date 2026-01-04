# Camera Control

A Rust library for controlling DSLR/mirrorless cameras via USB using gphoto2. Designed for astrophotography with long exposure (bulb mode) support.

## Features

- Connect to cameras via USB
- Take long exposure photos (bulb mode)
- Set ISO values with validation
- Set aperture (f-stop) with validation (optional)
- Automatic file download from camera
- Works with Canon EOS cameras (tested on Canon R8)

## Requirements

### System Dependencies

Install libgphoto2:

```bash
# Ubuntu/Debian
sudo apt install libgphoto2-dev

# Fedora
sudo dnf install libgphoto2-devel

# Arch Linux
sudo pacman -S libgphoto2
```

### Camera Setup

1. Connect your camera via USB
2. Set camera to **Bulb (B) mode** on the mode dial
3. Kill any conflicting processes:
   ```bash
   pkill gvfs-gphoto2-volume-monitor
   ```

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
camera_control = { path = "path/to/camera_control" }
```

## Usage

### Basic Example

```rust
use camera_control::CameraController;
use std::path::Path;

fn main() {
    // Connect to camera
    let camera = CameraController::connect().expect("Failed to connect");
    println!("Connected to: {}", camera.model());

    // Show available options
    let iso_options = camera.get_iso_options().unwrap();
    println!("Available ISO: {:?}", iso_options);
    
    let aperture_options = camera.get_aperture_options().unwrap();
    println!("Available Aperture: {:?}", aperture_options);

    // Take a 30 second exposure at ISO 1600, f/2.8
    let path = camera.take_photo(1600, Some(2.8), 30, Path::new(".")).unwrap();
    println!("Saved to: {}", path.display());
    
    // Or keep current aperture setting
    let path = camera.take_photo(800, None, 60, Path::new(".")).unwrap();
}
```

### API Reference

#### `CameraController::connect() -> Result<CameraController>`
Connect to the first available camera.

#### `camera.model() -> String`
Get the camera model name.

#### `camera.take_photo(iso, aperture, exposure_seconds, save_path) -> Result<PathBuf>`
Take a bulb exposure photo.
- `iso`: ISO value (e.g., 100, 800, 1600, 3200)
- `aperture`: Optional aperture value (e.g., `Some(2.8)`, `Some(5.6)`). Use `None` to keep current setting.
- `exposure_seconds`: Exposure time in seconds
- `save_path`: Directory to save the image

#### `camera.get_iso_options() -> Result<Vec<u64>>`
Get list of available ISO values from the camera.

#### `camera.get_aperture_options() -> Result<Vec<f64>>`
Get list of available aperture (f-stop) values from the camera.

#### `camera.print_config() -> Result<()>`
Print the full camera configuration (for debugging).

## Testing

Tests require a camera connected in Bulb mode. They are ignored by default.

```bash
# Run all tests (skips camera tests)
cargo test

# Run connection test
cargo test test_connect -- --ignored --nocapture

# Run ISO options test
cargo test test_iso_options -- --ignored --nocapture

# Run aperture options test
cargo test test_aperture_options -- --ignored --nocapture

# Run 5-second exposure test
cargo test test_take_photo_5s -- --ignored --nocapture

# Run 60-second exposure test (astrophotography)
cargo test test_take_photo_60s -- --ignored --nocapture

# Run all camera tests (one at a time to avoid USB conflicts)
cargo test -- --ignored --nocapture --test-threads=1
```

## Running the Demo

```bash
cargo run
```

This will:
1. Connect to the camera
2. Print the camera model
3. Show available ISO options
4. Take a 5-second test exposure at ISO 800

## Troubleshooting

### "Could not claim USB device"
Another process is using the camera. Kill it:
```bash
pkill gvfs-gphoto2-volume-monitor
```

### "No camera detected"
- Check USB connection
- Ensure camera is powered on
- Try a different USB port

### "Bulb capture failed"
- Make sure the mode dial is set to **B** (Bulb)
- For Canon cameras, this is usually past Manual (M) mode
