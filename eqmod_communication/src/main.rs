use eqmod_communication::IndiClient;


const DEVICE_NAME: &str = "EQMod Mount";
const PORT: u16 = 7624;

const LATITUDE: f64 = 42.960213;   // degrees North
const LONGITUDE: f64 = 1.609226;   // degrees East
const ELEVATION: f64 = 600.0;     // meters

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = IndiClient::new("localhost", PORT, DEVICE_NAME).await?;

    client.connect().await?;
    println!("Mount connected and ready!");
    
    client.init_date_pos(LATITUDE, LONGITUDE, ELEVATION).await?;
    
    // Now you can use goto commands and the mount will know its true position
    client.goto(0.0, 15.0).await?;

    client.disconnect().await?;
    
    Ok(())
}
