use core::f64;

use eqmod_communication::{IndiClient, IndiError};
use astro_pi_plate_solving::{CoordinateEquatorial};
use crate::capture_solve::{capture_and_solve, CaptureSettings};
use camera_control::CameraController;

const DEVICE_NAME: &str = "EQMod Mount";
const PORT: u16 = 7624;
const TARGET_TOLLERANCE_ARCSEC: f64 = 180.0; // Tolerance for considering the goto successful
const MAX_ITERATIONS: usize = 5; // Max iterations for closed-loop correction


pub struct GotoState {
    offset_ra_h: f64,
    offset_dec_deg: f64,
}   

impl Default for GotoState {
    fn default() -> Self {
        Self {
            offset_ra_h: 0.0,
            offset_dec_deg: 0.0,
        }
    }
}

pub async fn init_eqmod_goto(latitude : f64, longitude: f64, elevation: f64) -> Result<IndiClient, IndiError> {
    // Connect to EQMOD via INDI
    let mut indi_client = IndiClient::new("localhost", PORT, DEVICE_NAME).await?;
    indi_client.connect().await?;
    indi_client.init_date_pos(latitude, longitude, elevation).await?;
    Ok(indi_client)
}

pub async fn goto_closed_loop(client : &mut IndiClient, camera : &CameraController, setting : CaptureSettings, state : &mut GotoState, target_pos: CoordinateEquatorial , close_loop : bool) -> Result<(), Box<dyn std::error::Error>> {
    // Set target coordinates

    let mut error_ra_h = state.offset_ra_h;
    let mut error_dec_deg = state.offset_dec_deg;

    let target_ra = target_pos.ra.to_hours() + error_ra_h;
    let target_dec = target_pos.dec.to_degrees() + error_dec_deg;
    client.goto(target_ra, target_dec).await?;

    if !close_loop {
        return Ok(());
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the mount to stop moving


    let result_platesolve = capture_and_solve(&camera, &target_pos, &setting)?;

    let solved_coordinate = CoordinateEquatorial::from_radians(result_platesolve.solution.optical_axis_ra, result_platesolve.solution.optical_axis_dec) ;

    let solved_ra = solved_coordinate.ra.to_hours();
    let solved_dec = solved_coordinate.dec.to_degrees();

    let diff_ra_h = target_ra - solved_ra;
    let diff_dec_deg = target_dec - solved_dec;


    error_ra_h += diff_ra_h;
    error_dec_deg += diff_dec_deg;

    let mut distance_arcsec = (diff_ra_h * 15.0).hypot(diff_dec_deg) * 3600.0;
    println!("Initial position is {:.2}\" from target.", distance_arcsec);

    let mut i = 0;



    while distance_arcsec > TARGET_TOLLERANCE_ARCSEC && i < MAX_ITERATIONS {

        println!("Current position is {:.2}\" from target. Adjusting...", distance_arcsec);
        client.goto(target_ra + (target_ra - solved_ra), target_dec + (target_dec - solved_dec)).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // Wait for the mount to stop moving

        let result_platesolve = capture_and_solve(&camera, &target_pos, &setting)?;
        
        let solved_coordinate = CoordinateEquatorial::from_radians(result_platesolve.solution.optical_axis_ra, result_platesolve.solution.optical_axis_dec) ;

        let solved_ra = solved_coordinate.ra.to_hours();
        let solved_dec = solved_coordinate.dec.to_degrees();

        let diff_ra_h = target_ra - solved_ra;
        let diff_dec_deg = target_dec - solved_dec;
        
        error_ra_h += diff_ra_h;
        error_dec_deg += diff_dec_deg;

        distance_arcsec = ((diff_ra_h) * 15.0).hypot(diff_dec_deg) * 3600.0;
        println!("Iteration {}: Current position is {:.2}\" from target.", i + 1, distance_arcsec);
        i += 1;
    }

    state.offset_ra_h = error_ra_h;
    state.offset_dec_deg = error_dec_deg;
    Ok(())
}

pub async fn init_eqmod_disconnect(client : &mut IndiClient) -> Result<(), IndiError> {
    client.disconnect().await
}