use astro_pi_plate_solving::{solve_plate, CoordinateEquatorial, RaHoursMinutesSeconds, Arcdegrees};
use std::path::Path;


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let initial = CoordinateEquatorial::new(
        RaHoursMinutesSeconds::new(14, 15, 21.4),
        Arcdegrees::new(54, 1, 14.40),
    );
    let image_path = Path::new("camera_img/IMG_8993.CR3");

    let result = solve_plate(image_path, &initial)?;
    println!("{}", result);
    Ok(())
}
