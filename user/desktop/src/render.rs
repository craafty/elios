//! The screen: everything is composed in a back buffer in ordinary
//! memory (drawn with `ui::draw`), and only the parts that changed are
//! copied to the screen, which is write-combined video memory (fast to
//! write in order, very slow to read).

use alloc::vec;
use alloc::vec::Vec;

use ui::Canvas;
pub use ui::draw::{Font, Rect};

use crate::sys;

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

    fn canvas(&mut self, clip: Rect) -> Canvas<'_> {
        let mut canvas = Canvas::new(&mut self.back, self.width, self.height);
        canvas.set_clip(clip);
        canvas
    }

    pub fn fill(&mut self, rect: Rect, color: u32) {
        self.canvas(rect).fill(rect, color);
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
        self.canvas(clip).blit(pixels, width, height, stride, x, y, alpha);
    }

    /// Draws `text` with its top-left corner at (`x`, `y`), in `color`
    /// over whatever is there.
    pub fn text(&mut self, font: &mut Font, x: i32, y: i32, text: &str, color: u32, clip: Rect) {
        self.canvas(clip).text(font, x, y, text, color);
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
