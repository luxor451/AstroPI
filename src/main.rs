mod read_csv;
mod tui;

use read_csv::load_messier_catalogue;
use tui::run_tui;
use std::path::Path;
use astro_pi_plate_solving::{convert_cr3_to_dng, dng_to_png};


fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger - controlled via RUST_LOG env var
    env_logger::init();

    let cr3_path = Path::new("camera_img/M101.CR3");
    let dng_path = Path::new("stretched_temp.dng");
    let output_path = Path::new("stretched_output.png");
    
    println!("Converting {} to DNG...", cr3_path.display());
    convert_cr3_to_dng(cr3_path, dng_path)?;
    
    println!("Saving stretched image to {}...", output_path.display());
    dng_to_png(dng_path, output_path)?;
    
    println!("Done! Open {} to view the stretched image.", output_path.display());

    // Load Messier catalogue
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());
    let catalogue_path = format!("{}/messier.csv", manifest_dir);
    let catalogue = load_messier_catalogue(&catalogue_path)?;
    
    println!("Loaded {} Messier objects from catalogue", catalogue.len());

    // Run the TUI
    run_tui(&catalogue, cr3_path)
}

