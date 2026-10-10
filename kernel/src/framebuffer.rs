//! Drawing to the linear framebuffer the bootloader found.

use bootinfo::PixelFormat;

/// A color as 0xRRGGBB.
#[derive(Clone, Copy)]
pub struct Color(pub u32);

pub struct Framebuffer {
    pixels: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    format: PixelFormat,
}

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

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Fills a rectangle, clipped to the screen.
    pub fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: Color) {
        let pixel = self.encode(color);
        let x_end = (x + width).min(self.width);
        let y_end = (y + height).min(self.height);
        for row in y.min(y_end)..y_end {
            for column in x.min(x_end)..x_end {
                // SAFETY: clipped to the visible area, which lies inside the
                // framebuffer. Volatile because this is device memory.
                unsafe { self.pixels.add(row * self.stride + column).write_volatile(pixel) };
            }
        }
    }

    fn encode(&self, Color(rgb): Color) -> u32 {
        match self.format {
            // Bytes B, G, R in memory: exactly 0xRRGGBB as a little-endian u32.
            PixelFormat::Bgr => rgb,
            PixelFormat::Rgb => (rgb & 0x00FF00) | (rgb >> 16 & 0xFF) | (rgb & 0xFF) << 16,
            PixelFormat::Absent => 0,
        }
    }
}

const BACKGROUND_TOP: Color = Color(0x0B1026);
const BACKGROUND_BOTTOM: Color = Color(0x2A0F3D);
const LOGO_ELI: Color = Color(0xF2F2F7);
const LOGO_OS: Color = Color(0x4FD1FF);

/// 5x7 glyphs for the boot logo, one row per byte, leftmost pixel in bit 4.
const GLYPH_E: [u8; 7] = [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110];
const GLYPH_L: [u8; 7] = [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110];
const GLYPH_I: [u8; 7] = [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110];
const GLYPH_O: [u8; 7] = [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110];
const GLYPH_S: [u8; 7] = [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110];

/// Draws the eliOS boot splash: a gradient with the logo in the middle.
pub fn draw_splash(fb: &mut Framebuffer) {
    for row in 0..fb.height() {
        let color = blend(BACKGROUND_TOP, BACKGROUND_BOTTOM, row, fb.height());
        fb.fill_rect(0, row, fb.width(), 1, color);
    }

    let logo =
        [(&GLYPH_E, LOGO_ELI), (&GLYPH_L, LOGO_ELI), (&GLYPH_I, LOGO_ELI), (&GLYPH_O, LOGO_OS), (&GLYPH_S, LOGO_OS)];
    // Each glyph is 5 cells wide plus 1 cell of spacing; aim for the logo to
    // span about a third of the screen.
    let logo_cells = logo.len() * 6 - 1;
    let scale = (fb.width() / 3 / logo_cells).max(1);
    let left = (fb.width() - logo_cells * scale) / 2;
    let top = (fb.height() - 7 * scale) / 2;

    for (index, (glyph, color)) in logo.iter().enumerate() {
        let glyph_left = left + index * 6 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (0b10000 >> column) != 0 {
                    fb.fill_rect(glyph_left + column * scale, top + row * scale, scale, scale, *color);
                }
            }
        }
    }

    // An accent line under the logo.
    let line_top = top + 9 * scale;
    fb.fill_rect(left, line_top, logo_cells * scale, (scale / 4).max(1), LOGO_OS);
}

/// Linear interpolation between two colors, `step` of `steps` of the way.
fn blend(Color(from): Color, Color(to): Color, step: usize, steps: usize) -> Color {
    let channel = |shift: u32| {
        let a = (from >> shift & 0xFF) as usize;
        let b = (to >> shift & 0xFF) as usize;
        let value = (a * (steps - step) + b * step) / steps.max(1);
        (value as u32) << shift
    };
    Color(channel(16) | channel(8) | channel(0))
}
