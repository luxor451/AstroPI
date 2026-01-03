use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use astro_pi_plate_solving::{solve_plate, CoordinateEquatorial, RaHoursMinutesSeconds, Arcdegrees};

use crate::read_csv::{MessierObject, extract_messier_number, find_messier_object};

/// Parse RA string like "14:03:12.54" to RaHoursMinutesSeconds
fn parse_ra(ra_str: &str) -> Option<RaHoursMinutesSeconds> {
    let parts: Vec<&str> = ra_str.split(':').collect();
    if parts.len() == 3 {
        let hours: i64 = parts[0].parse().ok()?;
        let minutes: i64 = parts[1].parse().ok()?;
        let seconds: f64 = parts[2].parse().ok()?;
        Some(RaHoursMinutesSeconds::new(hours, minutes, seconds))
    } else {
        None
    }
}

/// Parse Dec string like "+54:20:56.2" or "-23:10:44.7" to Arcdegrees
fn parse_dec(dec_str: &str) -> Option<Arcdegrees> {
    let dec_str = dec_str.trim();
    let (sign, rest) = if dec_str.starts_with('-') {
        (-1, &dec_str[1..])
    } else if dec_str.starts_with('+') {
        (1, &dec_str[1..])
    } else {
        (1, dec_str)
    };

    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() == 3 {
        let degrees: i64 = parts[0].parse().ok()?;
        let arcminutes: i64 = parts[1].parse().ok()?;
        let arcseconds: f64 = parts[2].parse().ok()?;
        Some(Arcdegrees::new(sign * degrees, arcminutes, arcseconds))
    } else {
        None
    }
}

/// Run the TUI for Messier object lookup and plate solving
pub fn run_tui(catalogue: &[MessierObject]) -> Result<(), Box<dyn std::error::Error>> {
    // Ask user for Messier object
    print!("Enter Messier object (e.g., M101, M31, M42): ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    // Parse the Messier number from input
    let messier_number = match extract_messier_number(input) {
        Some(n) => n,
        None => {
            eprintln!("Invalid input: '{}'. Please enter a valid Messier object like M101.", input);
            return Ok(());
        }
    };

    // Binary search for the object
    let messier_obj = match find_messier_object(catalogue, messier_number) {
        Some(obj) => obj,
        None => {
            eprintln!("Messier object M{} not found in catalogue.", messier_number);
            return Ok(());
        }
    };

    println!("Found: {} - RA: {}, Dec: {}", messier_obj.name, messier_obj.ra, messier_obj.dec);

    // Parse coordinates
    let ra = match parse_ra(&messier_obj.ra) {
        Some(r) => r,
        None => {
            eprintln!("Failed to parse RA: {}", messier_obj.ra);
            return Ok(());
        }
    };

    let dec = match parse_dec(&messier_obj.dec) {
        Some(d) => d,
        None => {
            eprintln!("Failed to parse Dec: {}", messier_obj.dec);
            return Ok(());
        }
    };

    let initial = CoordinateEquatorial::new(ra, dec);
    println!("Using initial coordinates: RA={}, Dec={}", initial.ra, initial.dec);

    let image_path = Path::new("camera_img/IMG_8993.CR3");

    println!("\nStarting plate solving...");
    let start = Instant::now();
    let result = solve_plate(image_path, &initial)?;
    let elapsed = start.elapsed();

    println!("\n{}", result);
    println!("Execution time: {:.2?}", elapsed);
    Ok(())
}
