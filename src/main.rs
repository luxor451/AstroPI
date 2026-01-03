mod read_csv;
mod tui;

use read_csv::load_messier_catalogue;
use tui::run_tui;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger - controlled via RUST_LOG env var
    env_logger::init();

    // Load Messier catalogue
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());
    let catalogue_path = format!("{}/messier.csv", manifest_dir);
    let catalogue = load_messier_catalogue(&catalogue_path)?;
    
    println!("Loaded {} Messier objects from catalogue", catalogue.len());

    // Run the TUI
    run_tui(&catalogue)
}

