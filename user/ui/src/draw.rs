//! Drawing into pixel buffers: rectangles, anti-aliased shapes and text.
//!
//! Pixels are 32-bit 0xAARRGGBB with premultiplied alpha, which is what
//! Wayland's ARGB8888 and XRGB8888 buffers hold and what the framebuffer
//! takes (blue in the lowest byte). Colors passed in are plain (not
//! premultiplied) 0xAARRGGBB.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use truetype::{Point, Segment};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    pub const fn right(&self) -> i32 {
        self.x + self.width
    }

    pub const fn bottom(&self) -> i32 {
        self.y + self.height
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

    /// Shrunk by `amount` on every side.
    pub fn inset(&self, amount: i32) -> Rect {
        Rect::new(self.x + amount, self.y + amount, self.width - 2 * amount, self.height - 2 * amount)
    }
}

/// A pixel buffer being drawn into, with a clip rectangle nothing is drawn
/// outside of.
pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    pub width: i32,
    pub height: i32,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    /// `pixels` holds `width` by `height` pixels, row after row.
    pub fn new(pixels: &'a mut [u32], width: i32, height: i32) -> Self {
        assert!(pixels.len() >= (width.max(0) * height.max(0)) as usize);
        Self { pixels, width, height, clip: Rect::new(0, 0, width, height) }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Limits drawing to `rect` (within the canvas); returns the clip it
    /// replaces, to put back afterwards.
    pub fn set_clip(&mut self, rect: Rect) -> Rect {
        let old = self.clip;
        self.clip = rect.intersect(&self.bounds());
        old
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        self.pixels
    }

    /// Fills `rect` with `color`, blended if it's translucent.
    pub fn fill(&mut self, rect: Rect, color: u32) {
        let area = rect.intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        let alpha = color >> 24;
        let premultiplied = premultiply(color, alpha);
        for y in area.y..area.bottom() {
            let row = (y * self.width + area.x) as usize;
            let pixels = &mut self.pixels[row..row + area.width as usize];
            if alpha == 0xFF {
                pixels.fill(color);
            } else {
                pixels.iter_mut().for_each(|pixel| *pixel = blend(*pixel, premultiplied, alpha));
            }
        }
    }

    /// A `thickness` pixel line around the inside of `rect`.
    pub fn border(&mut self, rect: Rect, thickness: i32, color: u32) {
        let t = thickness.min(rect.width / 2).min(rect.height / 2).max(1);
        self.fill(Rect::new(rect.x, rect.y, rect.width, t), color);
        self.fill(Rect::new(rect.x, rect.bottom() - t, rect.width, t), color);
        self.fill(Rect::new(rect.x, rect.y + t, t, rect.height - 2 * t), color);
        self.fill(Rect::new(rect.right() - t, rect.y + t, t, rect.height - 2 * t), color);
    }

    /// Draws `pixels` (`width` wide, rows `stride` apart) with its top left
    /// at (x, y). With `alpha`, pixels are blended (premultiplied alpha, as
    /// Wayland buffers hold).
    #[allow(clippy::too_many_arguments, reason = "a blit has this many parameters")]
    pub fn blit(&mut self, pixels: &[u32], width: i32, height: i32, stride: usize, x: i32, y: i32, alpha: bool) {
        let area = Rect::new(x, y, width, height).intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        for row in area.y..area.bottom() {
            let source_start = (row - y) as usize * stride + (area.x - x) as usize;
            let Some(source) = pixels.get(source_start..source_start + area.width as usize) else { continue };
            let target_start = (row * self.width + area.x) as usize;
            let target = &mut self.pixels[target_start..target_start + area.width as usize];
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

    /// Moves the pixels inside `rect` up by `dy` (down if negative); what's
    /// uncovered keeps its old pixels, to be drawn over. Scrolling this way
    /// only needs the new part drawn.
    pub fn scroll(&mut self, rect: Rect, dy: i32) {
        let area = rect.intersect(&self.bounds());
        if area.is_empty() || dy == 0 || dy.abs() >= area.height {
            return;
        }
        let width = self.width as usize;
        let span = area.width as usize;
        let rows: Vec<i32> =
            if dy > 0 { (area.y..area.bottom() - dy).collect() } else { (area.y - dy..area.bottom()).rev().collect() };
        for y in rows {
            let to = y as usize * width + area.x as usize;
            let from = (y + dy) as usize * width + area.x as usize;
            self.pixels.copy_within(from..from + span, to);
        }
    }

    /// Writes `text` with its top-left corner at (`x`, `y`); returns where
    /// the next character would go.
    pub fn text(&mut self, font: &mut Font, x: i32, y: i32, text: &str, color: u32) -> i32 {
        let mut pen = x as f32;
        for c in text.chars() {
            pen += self.glyph(font, pen as i32, y, c, color);
        }
        pen as i32
    }

    /// Writes one character with the top-left corner of its line at
    /// (`x`, `y`); returns how far it moves the pen.
    pub fn glyph(&mut self, font: &mut Font, x: i32, y: i32, c: char, color: u32) -> f32 {
        let baseline = y + font.ascent;
        let clip = self.clip;
        let (glyph, advance) = font.glyph(c);
        let left = x + glyph.left;
        let top = baseline - glyph.top;
        let alpha = color >> 24;
        let area = Rect::new(left, top, glyph.width as i32, glyph.height as i32).intersect(&clip);
        for py in area.y..area.bottom() {
            let row = (py - top) as usize * glyph.width;
            for px in area.x..area.right() {
                let coverage = u32::from(glyph.coverage[row + (px - left) as usize]) * alpha / 255;
                if coverage != 0 {
                    let out = &mut self.pixels[(py * self.width + px) as usize];
                    *out = blend(*out, premultiply(color, coverage), coverage);
                }
            }
        }
        *advance
    }

    /// Fills the closed outline `path`, anti-aliased.
    pub fn fill_path(&mut self, path: &Path, color: u32) {
        let Some((min, max)) = path.bounds() else { return };
        let left = floor(min.x);
        let top = floor(min.y);
        let width = (ceil(max.x) - left) as usize;
        let height = (ceil(max.y) - top) as usize;
        let shift = |p: Point| Point::new(p.x - left, p.y - top);
        let segments: Vec<Segment> = path
            .segments
            .iter()
            .map(|segment| match *segment {
                Segment::Line(a, b) => Segment::Line(shift(a), shift(b)),
                Segment::Quad(a, b, c) => Segment::Quad(shift(a), shift(b), shift(c)),
            })
            .collect();
        let coverage = truetype::fill(&segments, width, height);
        let (left, top) = (left as i32, top as i32);
        let area = Rect::new(left, top, width as i32, height as i32).intersect(&self.clip);
        let alpha = color >> 24;
        for py in area.y..area.bottom() {
            for px in area.x..area.right() {
                let c = u32::from(coverage[(py - top) as usize * width + (px - left) as usize]) * alpha / 255;
                if c != 0 {
                    let out = &mut self.pixels[(py * self.width + px) as usize];
                    *out = blend(*out, premultiply(color, c), c);
                }
            }
        }
    }

    /// Fills `rect` with its corners rounded off by `radius` pixels.
    pub fn rounded(&mut self, rect: Rect, radius: f32, color: u32) {
        let radius = radius.min(rect.width as f32 / 2.0).min(rect.height as f32 / 2.0);
        if radius <= 0.0 {
            self.fill(rect, color);
            return;
        }
        // The straight middle is a plain fill; only the rows with corners
        // need the anti-aliased path (the top and bottom halves of a
        // rounded rectangle just 2r high, so the rasterizer's buffer stays
        // small).
        let r = ceil(radius) as i32;
        self.fill(Rect::new(rect.x, rect.y + r, rect.width, rect.height - 2 * r), color);
        let old = self.clip;
        let band = Path::rounded_rect(Rect::new(rect.x, rect.y, rect.width, 2 * r), radius);
        self.clip = Rect::new(rect.x, rect.y, rect.width, r).intersect(&old);
        self.fill_path(&band, color);
        let band = Path::rounded_rect(Rect::new(rect.x, rect.bottom() - 2 * r, rect.width, 2 * r), radius);
        self.clip = Rect::new(rect.x, rect.bottom() - r, rect.width, r).intersect(&old);
        self.fill_path(&band, color);
        self.clip = old;
    }
}

/// An outline made of straight lines and quadratic curves, in pixels.
#[derive(Default, Clone)]
pub struct Path {
    segments: Vec<Segment>,
    start: Option<Point>,
    current: Point,
}

impl Path {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.close();
        self.start = Some(Point::new(x, y));
        self.current = Point::new(x, y);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        let to = Point::new(x, y);
        self.segments.push(Segment::Line(self.current, to));
        self.current = to;
        self
    }

    /// A curve to (x, y) pulled towards (cx, cy).
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        let to = Point::new(x, y);
        self.segments.push(Segment::Quad(self.current, Point::new(cx, cy), to));
        self.current = to;
        self
    }

    /// Joins the current contour back to its start.
    pub fn close(&mut self) -> &mut Self {
        if let Some(start) = self.start.take()
            && start != self.current
        {
            self.segments.push(Segment::Line(self.current, start));
            self.current = start;
        }
        self
    }

    /// A polygon through `points`.
    pub fn polygon(points: &[(f32, f32)]) -> Self {
        let mut path = Path::new();
        if let Some(&(x, y)) = points.first() {
            path.move_to(x, y);
            points[1..].iter().for_each(|&(x, y)| {
                path.line_to(x, y);
            });
            path.close();
        }
        path
    }

    pub fn rounded_rect(rect: Rect, radius: f32) -> Self {
        let (x0, y0) = (rect.x as f32, rect.y as f32);
        let (x1, y1) = (rect.right() as f32, rect.bottom() as f32);
        let r = radius;
        let mut path = Path::new();
        path.move_to(x0 + r, y0)
            .line_to(x1 - r, y0)
            .quad_to(x1, y0, x1, y0 + r)
            .line_to(x1, y1 - r)
            .quad_to(x1, y1, x1 - r, y1)
            .line_to(x0 + r, y1)
            .quad_to(x0, y1, x0, y1 - r)
            .line_to(x0, y0 + r)
            .quad_to(x0, y0, x0 + r, y0)
            .close();
        path
    }

    /// The outline after this one, so both are filled together (holes go
    /// the other way round).
    pub fn append(&mut self, other: &Path) -> &mut Self {
        self.close();
        self.segments.extend_from_slice(&other.segments);
        self
    }

    fn bounds(&self) -> Option<(Point, Point)> {
        let mut points = self.segments.iter().flat_map(|segment| match *segment {
            Segment::Line(a, b) => [a, b, b],
            Segment::Quad(a, b, c) => [a, b, c],
        });
        let first = points.next()?;
        Some(points.fold((first, first), |(min, max), p| {
            (Point::new(min.x.min(p.x), min.y.min(p.y)), Point::new(max.x.max(p.x), max.y.max(p.y)))
        }))
    }
}

fn floor(x: f32) -> f32 {
    let truncated = x as i32 as f32;
    if truncated > x { truncated - 1.0 } else { truncated }
}

fn ceil(x: f32) -> f32 {
    let truncated = x as i32 as f32;
    if truncated < x { truncated + 1.0 } else { truncated }
}

/// `color` at `alpha` (0 to 255), premultiplied as `blend` wants it.
pub fn premultiply(color: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| (((color >> shift & 0xFF) * alpha + 127) / 255) << shift;
    alpha << 24 | channel(16) | channel(8) | channel(0)
}

/// `over` drawn on `under`, with `over`'s (premultiplied) alpha `a`. The
/// result is opaque: everything drawn ends up on an opaque background.
pub fn blend(under: u32, over: u32, a: u32) -> u32 {
    let inverse = 255 - a;
    let channel = |shift: u32| {
        let value = (over >> shift & 0xFF) + ((under >> shift & 0xFF) * inverse + 127) / 255;
        value.min(255) << shift
    };
    0xFF00_0000 | channel(16) | channel(8) | channel(0)
}

/// Mixes two opaque colors: `amount` (0 to 255) of `b` into `a`.
pub fn mix(a: u32, b: u32, amount: u32) -> u32 {
    blend(a, premultiply(b, amount), amount)
}

pub const FONT_PATH: &str = "/usr/share/fonts/dejavu/DejaVuSansMono.ttf";

/// The system's TrueType font at one size, with each glyph rasterized the
/// first time it's drawn.
pub struct Font {
    face: Rc<truetype::Font>,
    size: f32,
    /// From the top of a line to its baseline, in pixels.
    pub ascent: i32,
    pub line_height: i32,
    glyphs: BTreeMap<char, (truetype::Bitmap, f32)>,
}

impl Font {
    /// The system font at `size` pixels per em.
    pub fn load(size: f32) -> Result<Self, String> {
        let data = eli::fs::read(FONT_PATH).map_err(|error| format!("can't read {FONT_PATH}: {error}"))?;
        let face = truetype::Font::parse(data).map_err(|error| format!("{FONT_PATH}: {error:?}"))?;
        Ok(Self::new(Rc::new(face), size))
    }

    /// The same font at another size (sharing the font file).
    pub fn resized(&self, size: f32) -> Self {
        Self::new(self.face.clone(), size)
    }

    fn new(face: Rc<truetype::Font>, size: f32) -> Self {
        let metrics = face.line_metrics(size);
        Self {
            ascent: metrics.ascent as i32 + 1,
            line_height: metrics.height() as i32 + 1,
            face,
            size,
            glyphs: BTreeMap::new(),
        }
    }

    pub fn size(&self) -> f32 {
        self.size
    }

    pub fn has(&self, c: char) -> bool {
        self.face.has(c)
    }

    /// `c`'s bitmap and advance.
    pub fn glyph(&mut self, c: char) -> &(truetype::Bitmap, f32) {
        let (face, size) = (&self.face, self.size);
        self.glyphs.entry(c).or_insert_with(|| {
            let glyph = face.glyph_index(c);
            (face.rasterize(glyph, size), face.advance(glyph, size))
        })
    }

    /// How far `c` moves the pen, in pixels (fractional).
    pub fn advance(&mut self, c: char) -> f32 {
        self.glyph(c).1
    }

    /// How wide `text` is, in pixels.
    pub fn width(&mut self, text: &str) -> i32 {
        text.chars().map(|c| self.glyph(c).1).sum::<f32>() as i32
    }

    /// The longest start of `text` that fits in `room` pixels, with "..."
    /// on the end if it had to be cut.
    pub fn trim(&mut self, text: &str, room: i32) -> String {
        if self.width(text) <= room {
            return String::from(text);
        }
        let mut shown = String::from(text);
        while !shown.is_empty() {
            shown.pop();
            let candidate = format!("{shown}...");
            if self.width(&candidate) <= room {
                return candidate;
            }
        }
        shown
    }
}
