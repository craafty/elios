//! Drawing: everything is composed in a back buffer in ordinary memory,
//! and only the parts that changed are copied to the screen, which is
//! write-combined video memory (fast to write in order, very slow to
//! read). Pixels are 32-bit 0xAARRGGBB, which is also what Wayland's
//! ARGB8888 and XRGB8888 buffers hold and what the framebuffer takes
//! (blue in the lowest byte).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        Rect::new(x, y, right - x, bottom - y)
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Rect::new(x, y, right - x, bottom - y)
    }
}

/// The screen and the back buffer drawn into.
pub struct Screen {
    pub width: i32,
    pub height: i32,
    /// Video memory (stride in pixels).
    video: *mut u32,
    stride: usize,
    /// Red and blue swapped on screen (an RGB framebuffer).
    swap_red_blue: bool,
    pub back: Vec<u32>,
    /// What has to be redrawn and copied.
    pub damage: Rect,
}

const FBIOGET_VSCREENINFO: usize = 0x4600;
const FBIOGET_FSCREENINFO: usize = 0x4602;

impl Screen {
    /// Opens /dev/fb0 (which hides the text console) and maps it.
    pub fn open() -> Result<Self, &'static str> {
        let fd = sys::open("/dev/fb0", sys::O_RDWR).map_err(|_| "can't open /dev/fb0")?;
        let mut variable = [0u32; 40];
        let mut fixed = [0u8; 80];
        // SAFETY: buffers of the sizes the ioctls fill.
        unsafe {
            sys::ioctl(fd, FBIOGET_VSCREENINFO, variable.as_mut_ptr() as usize).map_err(|_| "not a framebuffer")?;
            sys::ioctl(fd, FBIOGET_FSCREENINFO, fixed.as_mut_ptr() as usize).map_err(|_| "not a framebuffer")?;
        }
        let (width, height) = (variable[0] as i32, variable[1] as i32);
        let line_length = u32::from_le_bytes(fixed[48..52].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(fixed[24..28].try_into().unwrap()) as usize;
        if variable[6] != 32 || width <= 0 || height <= 0 {
            return Err("unsupported framebuffer format");
        }
        let video = sys::map(fd, size, true).map_err(|_| "can't map the framebuffer")?;
        // The descriptor stays open (closing it would give the screen back
        // to the console); it's simply never closed.
        Ok(Self {
            width,
            height,
            video: video.cast(),
            stride: line_length / 4,
            swap_red_blue: variable[8] == 0,
            back: vec![0; (width * height) as usize],
            damage: Rect::new(0, 0, width, height),
        })
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn damage(&mut self, rect: Rect) {
        let clipped = rect.intersect(&self.bounds());
        if !clipped.is_empty() {
            self.damage = self.damage.union(&clipped);
        }
    }

    pub fn damage_all(&mut self) {
        self.damage = self.bounds();
    }

    /// Copies the damaged part of the back buffer to the screen.
    pub fn present(&mut self) {
        let area = self.damage.intersect(&self.bounds());
        self.damage = Rect::new(0, 0, 0, 0);
        if area.is_empty() {
            return;
        }
        for y in area.y..area.y + area.height {
            let row = (y * self.width + area.x) as usize;
            let source = &self.back[row..row + area.width as usize];
            // SAFETY: the row lies within the mapped framebuffer.
            let target = unsafe { self.video.add(y as usize * self.stride + area.x as usize) };
            if self.swap_red_blue {
                for (index, &pixel) in source.iter().enumerate() {
                    let swapped = pixel & 0xFF00_FF00 | (pixel >> 16 & 0xFF) | (pixel & 0xFF) << 16;
                    // SAFETY: as above.
                    unsafe { target.add(index).write(swapped) };
                }
            } else {
                // SAFETY: as above.
                unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), target, source.len()) };
            }
        }
    }

    pub fn fill(&mut self, rect: Rect, color: u32) {
        let area = rect.intersect(&self.bounds());
        if area.is_empty() {
            return;
        }
        for y in area.y..area.y + area.height {
            let row = (y * self.width + area.x) as usize;
            self.back[row..row + area.width as usize].fill(color);
        }
    }

    /// Draws `pixels` (`width` wide, rows `stride` apart) with its top left
    /// at (x, y), only within `clip`. With `alpha`, pixels are blended
    /// (premultiplied alpha, as Wayland buffers hold).
    #[allow(clippy::too_many_arguments, reason = "a blit has this many parameters")]
    pub fn blit(
        &mut self,
        pixels: &[u32],
        width: i32,
        height: i32,
        stride: usize,
        x: i32,
        y: i32,
        alpha: bool,
        clip: Rect,
    ) {
        let area = Rect::new(x, y, width, height).intersect(&clip).intersect(&self.bounds());
        if area.is_empty() {
            return;
        }
        for row in area.y..area.y + area.height {
            let source_start = (row - y) as usize * stride + (area.x - x) as usize;
            let Some(source) = pixels.get(source_start..source_start + area.width as usize) else { continue };
            let target_start = (row * self.width + area.x) as usize;
            let target = &mut self.back[target_start..target_start + area.width as usize];
            if !alpha {
                target.copy_from_slice(source);
                continue;
            }
            for (out, &pixel) in target.iter_mut().zip(source) {
                let a = pixel >> 24;
                if a == 0xFF {
                    *out = pixel;
                } else if a != 0 {
                    *out = blend(*out, pixel, a);
                }
            }
        }
    }

    /// Draws `text` with its top-left corner at (`x`, `y`), in `color`
    /// over whatever is there.
    pub fn text(&mut self, font: &mut Font, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        let clip = clip.intersect(&self.bounds());
        let baseline = y + font.ascent;
        let mut pen = x as f32;
        for c in text.chars() {
            let (glyph, advance) = font.glyph(c);
            let left = pen as i32 + glyph.left;
            let top = baseline - glyph.top;
            for row in 0..glyph.height {
                let py = top + row as i32;
                for column in 0..glyph.width {
                    let px = left + column as i32;
                    let coverage = u32::from(glyph.coverage[row * glyph.width + column]);
                    if coverage != 0 && clip.contains(px, py) {
                        let out = &mut self.back[(py * self.width + px) as usize];
                        *out = blend(*out, premultiply(color, coverage), coverage);
                    }
                }
            }
            pen += *advance;
        }
    }
}

/// `color` at `alpha` (0 to 255), premultiplied as `blend` wants it.
fn premultiply(color: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| (((color >> shift & 0xFF) * alpha + 127) / 255) << shift;
    alpha << 24 | channel(16) | channel(8) | channel(0)
}

/// `over` drawn on `under`, with `over`'s (premultiplied) alpha `a`.
fn blend(under: u32, over: u32, a: u32) -> u32 {
    let inverse = 255 - a;
    let channel = |shift: u32| {
        let value = (over >> shift & 0xFF) + ((under >> shift & 0xFF) * inverse + 127) / 255;
        value.min(255) << shift
    };
    0xFF00_0000 | channel(16) | channel(8) | channel(0)
}

/// The font the desktop writes with (the system's TrueType font at one
/// size), with each glyph rasterized the first time it's drawn.
pub struct Font {
    face: truetype::Font,
    size: f32,
    /// From the top of a line to its baseline, in pixels.
    pub ascent: i32,
    pub line_height: i32,
    glyphs: BTreeMap<char, (truetype::Bitmap, f32)>,
}

pub const FONT_PATH: &str = "/usr/share/fonts/dejavu/DejaVuSansMono.ttf";

impl Font {
    pub fn load(size: f32) -> Result<Self, String> {
        let data = eli::fs::read(FONT_PATH).map_err(|error| format!("can't read {FONT_PATH}: {error}"))?;
        let face = truetype::Font::parse(data).map_err(|error| format!("{FONT_PATH}: {error:?}"))?;
        let metrics = face.line_metrics(size);
        Ok(Self {
            ascent: metrics.ascent as i32 + 1,
            line_height: metrics.height() as i32 + 1,
            face,
            size,
            glyphs: BTreeMap::new(),
        })
    }

    /// `c`'s bitmap and advance.
    fn glyph(&mut self, c: char) -> &(truetype::Bitmap, f32) {
        let (face, size) = (&self.face, self.size);
        self.glyphs.entry(c).or_insert_with(|| {
            let glyph = face.glyph_index(c);
            (face.rasterize(glyph, size), face.advance(glyph, size))
        })
    }

    /// How wide `text` is, in pixels.
    pub fn width(&mut self, text: &str) -> i32 {
        text.chars().map(|c| self.glyph(c).1).sum::<f32>() as i32
    }
}

/// The mouse pointer drawn when no client sets one: an arrow, as rows of
/// '#' (black), 'o' (white) and '.' (clear).
const ARROW: [&str; 19] = [
    "#...........",
    "##..........",
    "#o#.........",
    "#oo#........",
    "#ooo#.......",
    "#oooo#......",
    "#ooooo#.....",
    "#oooooo#....",
    "#ooooooo#...",
    "#oooooooo#..",
    "#ooooooooo#.",
    "#oooooo#####",
    "#ooo#oo#....",
    "#oo#.#oo#...",
    "#o#..#oo#...",
    "##....#oo#..",
    "#.....#oo#..",
    "......#oo#..",
    ".......##...",
];

/// The arrow pointer as pixels: (width, height, pixels).
pub fn arrow() -> (i32, i32, Vec<u32>) {
    let pixels = ARROW
        .iter()
        .flat_map(|row| {
            row.bytes().map(|b| match b {
                b'#' => 0xFF00_0000,
                b'o' => 0xFFFF_FFFF,
                _ => 0,
            })
        })
        .collect();
    (12, 19, pixels)
}
