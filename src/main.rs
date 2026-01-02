use astro_pi_plate_solving::{solve_plate, CoordinateEquatorial, RaHoursMinutesSeconds, Arcdegrees};
use std::path::Path;
use std::time::Instant;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger - controlled via RUST_LOG env var
    // e.g., RUST_LOG=trace cargo run, RUST_LOG=astro_pi_plate_solving=debug cargo run
    env_logger::init();

    let initial = CoordinateEquatorial::new(
        RaHoursMinutesSeconds::new(12, 15, 21.4),
        Arcdegrees::new(54, 1, 14.40),
    );
    let image_path = Path::new("camera_img/IMG_8993.CR3");

    let start = Instant::now();
    let result = solve_plate(image_path, &initial)?;
    let elapsed = start.elapsed();

    println!("{}", result);
    println!("Execution time: {:.2?}", elapsed);
    Ok(())
}
