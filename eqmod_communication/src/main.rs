use eqmod_communication::IndiClient;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to INDI server...");
    let mut client = IndiClient::new("localhost", 7624, "EQMod Mount").await?;
    
    println!("Connecting to mount...");
    client.connect().await?;
    println!("Mount connected!");

    println!("\n═══════════════════════════════════════════════════════════════");
    println!("  Sending GOTO: RA=8h, DEC=60° (large movement to see action)");
    println!("═══════════════════════════════════════════════════════════════\n");
    
    client.goto(6.0, 60.0).await?;
    
    Ok(())
}
