use eqmod_communication::IndiClient;
use chrono::Utc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to INDI server...");
    let mut client = IndiClient::new("localhost", 7624, "EQMod Mount").await?;
    
    println!("Connecting to mount...");
    client.connect().await?;
    println!("Mount connected and ready!");


    let latitude = 42.960213;   // degrees North
    let longitude = 1.609226;   // degrees East
    let elevation = 600.0;     // meters
    
    client.set_location(latitude, longitude, elevation).await?;
    
    // Set current UTC time (critical for correct sidereal time calculation)
    let utc_now = Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    client.set_time(&utc_now).await?;

    // Give the mount a moment to process the time update and broadcast new LST
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    
    // Get Local Sidereal Time from the mount (it calculates it from time + location)
    let lst = client.get_lst().await?;
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    println!("\nLocal Sidereal Time from mount: {:.4}h ({:.0}h {:.0}m {:.1}s)", 
        lst, 
        lst.floor(), 
        (lst.fract() * 60.0).floor(), 
        (lst.fract() * 60.0).fract() * 60.0
    );

    
    // Now check the position
    let (ra, dec) = client.get_current_position().await?;
    println!("\nCurrent Position after sync: RA={:.4}h, DEC={:.4}°", ra, dec);
    
    // Now you can use goto commands and the mount will know its true position
    client.goto(3.0, 90.0).await?;
    
    Ok(())
}
