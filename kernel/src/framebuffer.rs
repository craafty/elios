//! The linear framebuffer the bootloader found.

use bootinfo::PixelFormat;

/// A color as 0xRRGGBB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

/// Exclusive access to video memory. Drawing happens elsewhere (see
/// `console`); this only knows the pixel layout and how to copy rows in.
pub struct Framebuffer {
    pixels: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    format: PixelFormat,
}

// SAFETY: a Framebuffer is the only handle to its memory, so moving it to
// another CPU is fine.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// # Safety
    /// `info` must describe a real framebuffer, mapped at `hhdm_offset` plus
    /// its physical address, that nothing else is drawing to.
    pub unsafe fn new(info: &bootinfo::Framebuffer, hhdm_offset: u64) -> Self {
        Self {
            pixels: (hhdm_offset + info.base) as *mut u32,
            width: info.width as usize,
            height: info.height as usize,
            stride: info.stride as usize,
            format: info.format,
        }
    }

    /// A framebuffer in ordinary memory, for tests.
    ///
    /// # Safety
    /// `pixels` must be valid for `stride * height` pixels for as long as the
    /// framebuffer is used.
    #[cfg(test)]
    pub unsafe fn from_raw(pixels: *mut u32, width: usize, height: usize, stride: usize, format: PixelFormat) -> Self {
        Self { pixels, width, height, stride, format }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The pixel value that displays as `color`.
    pub fn encode(&self, Color(rgb): Color) -> u32 {
        match self.format {
            // Bytes B, G, R in memory: exactly 0xRRGGBB as a little-endian u32.
            PixelFormat::Bgr => rgb,
            PixelFormat::Rgb => (rgb & 0x00FF00) | (rgb >> 16 & 0xFF) | (rgb & 0xFF) << 16,
            PixelFormat::Absent => 0,
        }
    }

    /// Copies already-encoded pixels into row `y`, starting at column `x`.
    /// Pixels past the right edge are dropped.
    pub fn write_span(&mut self, x: usize, y: usize, pixels: &[u32]) {
        assert!(x < self.width && y < self.height);
        let len = pixels.len().min(self.width - x);
        // SAFETY: the span lies within row `y`. Video memory only needs to be
        // written, never read back, so a plain copy is fine.
        unsafe { core::ptr::copy_nonoverlapping(pixels.as_ptr(), self.pixels.add(y * self.stride + x), len) };
    }

    #[cfg(test)]
    pub fn read_pixel(&self, x: usize, y: usize) -> u32 {
        assert!(x < self.width && y < self.height);
        // SAFETY: bounds checked above.
        unsafe { self.pixels.add(y * self.stride + x).read_volatile() }
    }
}
