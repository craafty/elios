//! The framebuffer text console.
//!
//! Text is drawn into a shadow copy of the screen in RAM, and only changed
//! rows are copied to video memory. Video memory is very slow to read on real
//! hardware, so this keeps scrolling fast: it never reads it back.
//!
//! The console understands the common ANSI escape sequences (colors, cursor
//! movement, erasing), the same ones terminal programs emit, so the kernel log
//! looks the same here and on a serial terminal.

use core::fmt;

use crate::font;
use crate::framebuffer::{Color, Framebuffer};
use crate::sync::{SpinLock, TakeOnce};

pub const BACKGROUND: Color = Color(0x0B1026);
pub const FOREGROUND: Color = Color(0xD8DAE5);

/// The eliOS palette for the 16 ANSI colors.
const PALETTE: [Color; 16] = [
    Color(0x1B1E2E), // black
    Color(0xFF5C7A), // red
    Color(0x5CE08F), // green
    Color(0xFFD166), // yellow
    Color(0x5B8CFF), // blue
    Color(0xC77DFF), // magenta
    Color(0x4FD1FF), // cyan
    Color(0xD8DAE5), // white
    Color(0x565B73), // bright black
    Color(0xFF8FA3), // bright red
    Color(0x8CF5B4), // bright green
    Color(0xFFE29A), // bright yellow
    Color(0x8DB0FF), // bright blue
    Color(0xDDAEFF), // bright magenta
    Color(0x8FE4FF), // bright cyan
    Color(0xFFFFFF), // bright white
];

/// Room for a 1920x1200 screen. Larger screens use only the top part until
/// the kernel has a heap to size this properly.
const SHADOW_PIXELS: usize = 1920 * 1200;
static SHADOW: TakeOnce<[u32; SHADOW_PIXELS]> = TakeOnce::new([0; SHADOW_PIXELS]);

static CONSOLE: SpinLock<Option<Console<'static>>> = SpinLock::new(None);

/// Takes over the framebuffer for the console and clears the screen.
pub fn init(fb: Framebuffer) {
    let shadow = SHADOW.take().expect("console initialized twice");
    *CONSOLE.lock() = Some(Console::new(fb, shadow));
}

/// Runs `f` on the console, if there is one.
pub fn with<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    CONSOLE.lock().as_mut().map(f)
}

/// Like [`with`], but gives up instead of waiting if the console is busy.
/// For the panic handler, which may have interrupted a console write.
pub fn try_with<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    CONSOLE.try_lock()?.as_mut().map(f)
}

/// How a color was chosen, so bold can brighten palette colors after the fact.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
    Default,
    Palette(usize),
    Rgb(u32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Ground,
    /// Just saw ESC.
    Escape,
    /// In an escape with intermediate bytes, like `ESC ( B` (select
    /// character set), waiting for its final byte.
    EscapeIntermediate,
    /// Inside a Control Sequence Introducer (`ESC [`).
    Csi,
}

const MAX_PARAMS: usize = 16;

pub struct Console<'a> {
    fb: Framebuffer,
    shadow: &'a mut [u32],
    /// Size of the console area in pixels.
    width: usize,
    height: usize,
    /// Size of the console area in character cells.
    columns: usize,
    rows: usize,
    /// The cursor. `column == columns` means the line is full and the next
    /// character wraps, as on a real terminal.
    column: usize,
    row: usize,

    foreground_ink: Ink,
    background_ink: Ink,
    bold: bool,
    /// The current colors, already encoded for the framebuffer.
    foreground: u32,
    background: u32,

    escape: EscapeState,
    params: [u16; MAX_PARAMS],
    param_count: usize,

    /// Pixel rows that differ from video memory, as `start..end`.
    dirty: Option<(usize, usize)>,
}

impl<'a> Console<'a> {
    pub fn new(fb: Framebuffer, shadow: &'a mut [u32]) -> Self {
        let width = fb.width();
        let height = fb.height().min(shadow.len() / width);
        let mut console = Self {
            shadow: &mut shadow[..width * height],
            width,
            height,
            columns: width / font::WIDTH,
            rows: height / font::HEIGHT,
            column: 0,
            row: 0,
            foreground_ink: Ink::Default,
            background_ink: Ink::Default,
            bold: false,
            foreground: fb.encode(FOREGROUND),
            background: fb.encode(BACKGROUND),
            escape: EscapeState::Ground,
            params: [0; MAX_PARAMS],
            param_count: 0,
            dirty: None,
            fb,
        };
        console.clear();
        console
    }

    #[cfg(test)]
    pub fn cursor(&self) -> (usize, usize) {
        (self.column, self.row)
    }

    /// Erases the whole screen and moves the cursor to the top left.
    pub fn clear(&mut self) {
        self.shadow.fill(self.background);
        self.column = 0;
        self.row = 0;
        self.mark_dirty(0, self.height);
    }

    /// Fills a rectangle (clipped to the console) with a color. For drawing
    /// things that aren't text, like the logo.
    pub fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: Color) {
        let pixel = self.fb.encode(color);
        let x_end = (x + width).min(self.width);
        let y_end = (y + height).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        for row in y..y_end {
            self.shadow[row * self.width + x..row * self.width + x_end].fill(pixel);
        }
        self.mark_dirty(y, y_end);
    }

    /// Moves the cursor to the start of the first text row entirely below
    /// pixel row `y`.
    pub fn move_below(&mut self, y: usize) {
        self.column = 0;
        self.row = y.div_ceil(font::HEIGHT).min(self.rows - 1);
    }

    /// Copies everything drawn since the last flush to the screen.
    pub fn flush(&mut self) {
        if let Some((start, end)) = self.dirty.take() {
            for y in start..end {
                self.fb.write_row(y, &self.shadow[y * self.width..(y + 1) * self.width]);
            }
        }
    }

    pub fn write_char(&mut self, c: char) {
        match self.escape {
            EscapeState::Ground => match c {
                '\x1B' => self.escape = EscapeState::Escape,
                '\n' => self.newline(),
                '\r' => self.column = 0,
                '\t' => self.column = (self.column / 8 + 1).saturating_mul(8).min(self.columns),
                '\x08' => self.column = self.column.saturating_sub(1),
                c if c.is_control() => {}
                c => self.put(c),
            },
            EscapeState::Escape => {
                self.escape = match c {
                    '[' => {
                        self.params = [0; MAX_PARAMS];
                        self.param_count = 0;
                        EscapeState::Csi
                    }
                    '\x20'..='\x2F' => EscapeState::EscapeIntermediate,
                    // Other escapes aren't supported; drop them.
                    _ => EscapeState::Ground,
                };
            }
            EscapeState::EscapeIntermediate => {
                if !matches!(c, '\x20'..='\x2F') {
                    self.escape = EscapeState::Ground;
                }
            }
            EscapeState::Csi => match c {
                '0'..='9' => {
                    self.param_count = self.param_count.max(1);
                    let param = &mut self.params[self.param_count - 1];
                    *param = param.saturating_mul(10).saturating_add(c as u16 - u16::from(b'0'));
                }
                ';' => self.param_count = (self.param_count.max(1) + 1).min(MAX_PARAMS),
                '\x40'..='\x7E' => {
                    self.escape = EscapeState::Ground;
                    self.execute_csi(c);
                }
                // Private markers like `?` and intermediate bytes: accepted
                // and ignored.
                _ => {}
            },
        }
    }

    fn put(&mut self, c: char) {
        if self.column == self.columns {
            self.newline();
        }
        self.draw_glyph(c, self.column, self.row);
        self.column += 1;
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll_up();
        }
    }

    fn scroll_up(&mut self) {
        let line = font::HEIGHT * self.width;
        let text_end = self.rows * line;
        self.shadow.copy_within(line..text_end, 0);
        self.shadow[text_end - line..text_end].fill(self.background);
        self.mark_dirty(0, self.rows * font::HEIGHT);
    }

    fn draw_glyph(&mut self, c: char, column: usize, row: usize) {
        let glyph = font::glyph(c);
        let x = column * font::WIDTH;
        let y = row * font::HEIGHT;
        for (dy, bits) in glyph.iter().enumerate() {
            let start = (y + dy) * self.width + x;
            for (dx, pixel) in self.shadow[start..start + font::WIDTH].iter_mut().enumerate() {
                *pixel = if bits & (0x80 >> dx) != 0 { self.foreground } else { self.background };
            }
        }
        self.mark_dirty(y, y + font::HEIGHT);
    }

    /// Blanks cells `from..to` of a text row in the current background.
    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let to = to.min(self.columns);
        if from >= to {
            return;
        }
        let y = row * font::HEIGHT;
        for dy in 0..font::HEIGHT {
            let start = (y + dy) * self.width;
            self.shadow[start + from * font::WIDTH..start + to * font::WIDTH].fill(self.background);
        }
        self.mark_dirty(y, y + font::HEIGHT);
    }

    fn mark_dirty(&mut self, start: usize, end: usize) {
        self.dirty = Some(match self.dirty {
            Some((old_start, old_end)) => (old_start.min(start), old_end.max(end)),
            None => (start, end),
        });
    }

    /// Parameter `index` of the current sequence, or `default` if absent or 0.
    fn param(&self, index: usize, default: usize) -> usize {
        match self.params[index] {
            0 => default,
            value if index < self.param_count => value as usize,
            _ => default,
        }
    }

    fn execute_csi(&mut self, command: char) {
        let n = self.param(0, 1);
        match command {
            'm' => self.select_graphic_rendition(),
            'A' => self.row = self.row.saturating_sub(n),
            'B' => self.row = (self.row + n).min(self.rows - 1),
            'C' => self.column = (self.column + n).min(self.columns - 1),
            'D' => self.column = self.column.min(self.columns - 1).saturating_sub(n),
            'H' | 'f' => {
                self.row = (self.param(0, 1) - 1).min(self.rows - 1);
                self.column = (self.param(1, 1) - 1).min(self.columns - 1);
            }
            'J' => match self.param(0, 0) {
                0 => {
                    self.erase_cells(self.row, self.column, self.columns);
                    for row in self.row + 1..self.rows {
                        self.erase_cells(row, 0, self.columns);
                    }
                }
                2 | 3 => {
                    let (column, row) = (self.column, self.row);
                    self.clear();
                    (self.column, self.row) = (column, row);
                }
                _ => {}
            },
            'K' => match self.param(0, 0) {
                0 => self.erase_cells(self.row, self.column, self.columns),
                1 => self.erase_cells(self.row, 0, self.column + 1),
                2 => self.erase_cells(self.row, 0, self.columns),
                _ => {}
            },
            _ => {}
        }
    }

    /// `ESC [ ... m`: colors and attributes.
    fn select_graphic_rendition(&mut self) {
        let count = self.param_count.max(1);
        let mut i = 0;
        while i < count {
            match self.params[i] {
                0 => {
                    self.foreground_ink = Ink::Default;
                    self.background_ink = Ink::Default;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                code @ 30..=37 => self.foreground_ink = Ink::Palette(usize::from(code - 30)),
                code @ 90..=97 => self.foreground_ink = Ink::Palette(usize::from(code - 90 + 8)),
                39 => self.foreground_ink = Ink::Default,
                code @ 40..=47 => self.background_ink = Ink::Palette(usize::from(code - 40)),
                code @ 100..=107 => self.background_ink = Ink::Palette(usize::from(code - 100 + 8)),
                49 => self.background_ink = Ink::Default,
                code @ (38 | 48) => {
                    // Extended colors: `38;5;n` (256-color) or `38;2;r;g;b`.
                    let (ink, used) = match self.params[i + 1..count] {
                        [5, n, ..] if n < 16 => (Some(Ink::Palette(usize::from(n))), 2),
                        [5, _, ..] => (None, 2),
                        [2, r, g, b, ..] => {
                            let channel = |v: u16| u32::from(v.min(255) as u8);
                            (Some(Ink::Rgb(channel(r) << 16 | channel(g) << 8 | channel(b))), 4)
                        }
                        _ => (None, count - i - 1),
                    };
                    if let Some(ink) = ink {
                        if code == 38 { self.foreground_ink = ink } else { self.background_ink = ink }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
        self.update_colors();
    }

    fn update_colors(&mut self) {
        let foreground = match self.foreground_ink {
            Ink::Default if self.bold => PALETTE[15],
            Ink::Default => FOREGROUND,
            Ink::Palette(index) if self.bold && index < 8 => PALETTE[index + 8],
            Ink::Palette(index) => PALETTE[index],
            Ink::Rgb(rgb) => Color(rgb),
        };
        let background = match self.background_ink {
            Ink::Default => BACKGROUND,
            Ink::Palette(index) => PALETTE[index],
            Ink::Rgb(rgb) => Color(rgb),
        };
        self.foreground = self.fb.encode(foreground);
        self.background = self.fb.encode(background);
    }
}

impl fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use bootinfo::PixelFormat;

    use super::*;

    const COLUMNS: usize = 4;
    const ROWS: usize = 3;
    const WIDTH: usize = COLUMNS * font::WIDTH;
    const HEIGHT: usize = ROWS * font::HEIGHT;

    /// Runs `f` on a small console backed by ordinary memory.
    fn with_test_console(f: impl FnOnce(&mut Console)) {
        let mut video = [0u32; WIDTH * HEIGHT];
        let mut shadow = [0u32; WIDTH * HEIGHT];
        // SAFETY: `video` outlives the console, which is dropped inside.
        let fb = unsafe { Framebuffer::from_raw(video.as_mut_ptr(), WIDTH, HEIGHT, WIDTH, PixelFormat::Bgr) };
        f(&mut Console::new(fb, &mut shadow));
    }

    /// Whether the cell shows `c` in the given colors (in the shadow).
    fn shows(console: &Console, column: usize, row: usize, c: char, foreground: Color, background: Color) -> bool {
        let glyph = font::glyph(c);
        (0..font::HEIGHT).all(|dy| {
            (0..font::WIDTH).all(|dx| {
                let pixel = console.shadow[(row * font::HEIGHT + dy) * WIDTH + column * font::WIDTH + dx];
                let ink = glyph[dy] & (0x80 >> dx) != 0;
                pixel == if ink { foreground.0 } else { background.0 }
            })
        })
    }

    fn shows_default(console: &Console, column: usize, row: usize, c: char) -> bool {
        shows(console, column, row, c, FOREGROUND, BACKGROUND)
    }

    crate::ktest! {
        fn text_advances_the_cursor() {
            with_test_console(|console| {
                console.write_str("ab").unwrap();
                assert_eq!(console.cursor(), (2, 0));
                assert!(shows_default(console, 0, 0, 'a'));
                assert!(shows_default(console, 1, 0, 'b'));
            });
        }

        fn full_lines_wrap_on_the_next_character() {
            with_test_console(|console| {
                console.write_str("abcd").unwrap();
                assert_eq!(console.cursor(), (4, 0));
                console.write_str("e").unwrap();
                assert_eq!(console.cursor(), (1, 1));
                assert!(shows_default(console, 0, 1, 'e'));
            });
        }

        fn newline_on_the_last_row_scrolls() {
            with_test_console(|console| {
                console.write_str("A\nB\nC\nD").unwrap();
                assert_eq!(console.cursor(), (1, 2));
                assert!(shows_default(console, 0, 0, 'B'));
                assert!(shows_default(console, 0, 1, 'C'));
                assert!(shows_default(console, 0, 2, 'D'));
            });
        }

        fn sgr_sets_palette_and_true_colors() {
            with_test_console(|console| {
                console.write_str("\x1B[31mr\x1B[1mR\x1B[0m.\x1B[38;2;1;2;3;44mt").unwrap();
                assert!(shows(console, 0, 0, 'r', PALETTE[1], BACKGROUND));
                assert!(shows(console, 1, 0, 'R', PALETTE[9], BACKGROUND));
                assert!(shows_default(console, 2, 0, '.'));
                assert!(shows(console, 3, 0, 't', Color(0x010203), PALETTE[4]));
            });
        }

        fn unsupported_escapes_are_swallowed() {
            with_test_console(|console| {
                console.write_str("\x1B[?25l\x1B(BA").unwrap();
                assert_eq!(console.cursor(), (1, 0));
                assert!(shows_default(console, 0, 0, 'A'));
            });
        }

        fn cursor_position_and_erase_line() {
            with_test_console(|console| {
                console.write_str("abcd\x1B[1;2H\x1B[K").unwrap();
                assert_eq!(console.cursor(), (1, 0));
                assert!(shows_default(console, 0, 0, 'a'));
                for column in 1..COLUMNS {
                    assert!(shows_default(console, column, 0, ' '));
                }
            });
        }

        fn flush_copies_to_video_memory() {
            with_test_console(|console| {
                console.write_str("x").unwrap();
                console.flush();
                assert!(console.dirty.is_none());
                for y in 0..font::HEIGHT {
                    for x in 0..font::WIDTH {
                        assert_eq!(console.fb.read_pixel(x, y), console.shadow[y * WIDTH + x]);
                    }
                }
            });
        }
    }
}
