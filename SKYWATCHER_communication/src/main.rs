mod heq5;
use heq5::Heq5;
use std::time::Duration;

fn main() {
    let port = "/dev/ttyUSB0"; 

    println!("Connecting to HEQ5...");
    let mut mount = Heq5::new(port).expect("Failed to open serial port");

    // --- SETUP & SLEW ---
    println!("Stopping RA axis...");
    mount.stop(1).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    println!("Initializing RA axis...");
    mount.init_axis(1).expect("Init failed");

    println!("Slewing RA by +10 degrees...");
    let target = mount.slew_relative_deg(1, 10.0).expect("Slew failed");

    mount.wait_until_target(1, target).expect("Wait failed");
    println!("Slew Complete.");

    // IMPORTANT: Wait for vibration/momentum to settle
    std::thread::sleep(Duration::from_secs(2));

    // --- START TRACKING ---
    println!("Starting Sidereal Tracking...");
    match mount.start_sidereal_tracking(1) {
        Ok(_) => println!("Tracking command sent."),
        Err(e) => eprintln!("Error: {}", e),
    }

    // --- VERIFICATION LOOP ---
    println!("\n=== VERIFYING TRACKING ===");
    println!("Expected Diff/sec: ~194 (Sidereal) | 0 (Stopped)");
    
    let mut last_pos = mount.get_position(1).unwrap_or(0);
    
    loop {
        std::thread::sleep(Duration::from_secs(1));
        
        match mount.get_position(1) {
            Ok(curr_pos) => {
                // Handle 24-bit wrap-around logic
                let diff = if curr_pos >= last_pos {
                    curr_pos - last_pos
                } else {
                    (16_777_216 - last_pos) + curr_pos 
                };

                let status = if diff >= 180 && diff <= 210 {
                    "YES (Sidereal)"
                } else if diff > 0 {
                    "DRIFTING (Too Slow)"
                } else {
                    "STOPPED"
                };

                println!("Position: {:8} | Diff: {:4} | Tracking: {}", 
                    curr_pos, diff, status);
                
                last_pos = curr_pos;
            },
            Err(e) => println!("Read error: {}", e),
        }
    }
}