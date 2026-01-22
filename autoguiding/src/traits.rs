pub trait MountDriver {
    fn guide_ra(&mut self, correction: f64);
    fn guide_dec(&mut self, correction: f64);
    fn get_position(&self) -> (f64, f64);
}

pub trait Camera {
    // Returns (width, height, raw_grayscale_bytes)
    fn capture_frame(&self, x: f64, y: f64) -> (u32, u32, Vec<u8>);
}
