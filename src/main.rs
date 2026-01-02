use plate_solving::{plate_solve, CoordinateEquatorial, RaHoursMinutesSeconds, Arcdegrees};


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let initial = CoordinateEquatorial::new(
        RaHoursMinutesSeconds::new(14, 15, 21.4),
        Arcdegrees::new(54, 1, 14.40),
    );

    let solved = plate_solve("camera_img/IMG_8993.CR3", &initial)?;
    println!("Solved: {}", solved);
    Ok(())
}
