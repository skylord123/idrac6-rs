//! Video decoding for the iDRAC6 virtual console.

pub mod aspeed;

/// Decoded screen contents. Pixels are 0x00RRGGBB, row-major.
#[derive(Clone, Default)]
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

impl Framebuffer {
    /// Resize, keeping old contents where they still fit is not needed: the
    /// server sends a full frame after every resolution change.
    pub fn resize(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.pixels = vec![0; width * height];
        }
    }
}
