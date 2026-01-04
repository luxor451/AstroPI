use camera_control::CameraController;
use gphoto2::Result;
use std::path::Path;

fn main() -> Result<()> {
    // Connect to the camera
    println!("Connecting to camera...");
    let camera = CameraController::connect().expect("Failed to connect to camera");

    // Print the camera model
    println!("Connected to: {}", camera.model());

    // Print available ISO options
    println!("\n=== Available Options ===");
    if let Ok(iso) = camera.get_iso_options() {
        println!("ISO: {:?}", iso);
    }
    if let Ok(aperture) = camera.get_aperture_options() {
        println!("Aperture: {:?}", aperture);
    }

    // Take a 5 second exposure
    // Make sure camera is in Bulb (B) mode
    println!("\n=== Taking 5 Second Exposure ===");
    println!("Settings: ISO 800, 5 seconds");
    println!("Make sure camera is in Bulb mode!");

    match camera.take_photo(800, None, 5, Path::new(".")) {
        Ok(path) => println!("Saved to: {}", path.display()),
        Err(e) => println!("Failed: {}", e),
    }

    Ok(())
}
