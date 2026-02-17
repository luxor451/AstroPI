mod capture_solve;
mod read_csv;
mod tui;
mod goto_closed_loop;


use read_csv::load_messier_catalogue;
use tui::run_tui;
use std::path::Path;
use astro_pi_plate_solving::{convert_cr3_to_dng, dng_to_png};
use goto_closed_loop::{init_eqmod_goto, goto_closed_loop, init_eqmod_disconnect, GotoState};

use crate::capture_solve::planify_shoot;



// fn main() -> Result<(), Box<dyn std::error::Error>> {
    
//     // Initialize logger - controlled via RUST_LOG env var
//     env_logger::init();

//     let cr3_path = Path::new("camera_img/M101.CR3");
//     let dng_path = Path::new("stretched_temp.dng");
//     let output_path = Path::new("stretched_output.png");
    
//     println!("Converting {} to DNG...", cr3_path.display());
//     convert_cr3_to_dng(cr3_path, dng_path)?;
    
//     println!("Saving stretched image to {}...", output_path.display());
//     dng_to_png(dng_path, output_path)?;
    
//     println!("Done! Open {} to view the stretched image.", output_path.display());

//     // Load Messier catalogue
//     let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
//         .unwrap_or_else(|_| ".".to_string());
//     let catalogue_path = format!("{}/messier.csv", manifest_dir);
//     let catalogue = load_messier_catalogue(&catalogue_path)?;
    
//     println!("Loaded {} Messier objects from catalogue", catalogue.len());

//     // Run the TUI
//     run_tui(&catalogue, cr3_path)
// }

const LATITUDE: f64 = 42.960213;   // degrees North
const LONGITUDE: f64 = 1.609226;   // degrees East
const ELEVATION: f64 = 600.0;     // meters

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    use capture_solve::{capture_and_solve, make_initial_guess, CaptureSettings};
    use camera_control::CameraController;

        
    let platesolve_settings = CaptureSettings {
        iso: 3200,
        aperture: None,
        exposure_seconds: 10,
        save_directory: "imgs/goto/captures".into(),
    };

    let capture_settings = CaptureSettings {
        iso: 1600,
        aperture: None,
        exposure_seconds: 5,
        save_directory: "imgs/astro_captures".into(),
    };

    // let initial_guess = make_initial_guess(14, 3, 26.0, 54, 20, 57.0);
    let target = make_initial_guess(3, 0, 0.0, 45, 0, 0.0);
    let mut goto_state = GotoState::default();

    // Connect to camera
    let camera = CameraController::connect()?;

    let mut indi_client = init_eqmod_goto(LATITUDE, LONGITUDE, ELEVATION).await?; // Example: Paris coordinates

    // Create initial guess (M101 coordinates)
    
    goto_closed_loop(&mut indi_client, &camera, platesolve_settings,&mut goto_state, target).await?;
    planify_shoot(&camera, &capture_settings, 3, 2, 10)?;

    

    init_eqmod_disconnect(&mut indi_client).await?;


    println!("Goto complete!");
    Ok(())
}