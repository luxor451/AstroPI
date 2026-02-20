#[cfg(test)]
mod tests {
    use crate::*;


    #[tokio::test]
    #[ignore = "Requires camera in Bulb mode"]
    async fn test_connect() {
        let camera = CameraController::connect().await.expect("Failed to connect");
        println!("Connected to: {}", camera.model().await);
    }

    #[tokio::test]
    #[ignore = "Requires camera in Bulb mode"]
    async fn test_iso_options() {
        let camera = CameraController::connect().await.expect("Failed to connect");
        let options = camera.get_iso_options().await.expect("Failed to get ISO");
        println!("ISO options: {:?}", options);
        assert!(!options.is_empty());
    }

    #[tokio::test]
    #[ignore = "Requires camera in Bulb mode - takes 5s exposure"]
    async fn test_take_photo_5s() {
        let camera = CameraController::connect().await.expect("Failed to connect");
        let temp = std::env::temp_dir();

        println!("Taking 5 second exposure at ISO 800...");
        let path = camera.take_photo(800, None, 5, &temp, None).await.expect("Failed to take photo");
        
        println!("Saved to: {}", path.display());
        assert!(path.exists());
        
        // Cleanup
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    #[ignore = "Requires camera in Bulb mode - takes 60s exposure"]
    async fn test_take_photo_60s() {
        let camera = CameraController::connect().await.expect("Failed to connect");

        println!("Taking 60 second exposure at ISO 1600...");
        let path = camera
            .take_photo(1600,  None, 60, Path::new("."), None)
            .await
            .expect("Failed to take photo");

        println!("Saved to: {}", path.display());
        assert!(path.exists());
    }

    #[tokio::test]
    #[ignore = "Requires camera in Bulb mode and connected to an actual lens"]
    async fn test_aperture_options() {
        let camera = CameraController::connect().await.expect("Failed to connect");
        let options = camera.get_aperture_options().await.expect("Failed to get aperture");
        println!("Aperture options: {:?}", options);
        assert!(!options.is_empty());
    }
}