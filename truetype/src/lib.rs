//! TrueType fonts: reading a font file's tables and turning its glyphs
//! into anti-aliased bitmaps.
//!
//! A TrueType file is a directory of tables, each found by a four-letter
//! tag. The ones needed to draw text are:
//!
//! - `head`: units per em (the grid the outlines are drawn on) and the
//!   format of `loca`;
//! - `maxp`: the number of glyphs;
//! - `cmap`: which glyph draws each character;
//! - `hhea` and `hmtx`: the line height, and how far each glyph moves the
//!   pen (its advance);
//! - `loca` and `glyf`: where each glyph's outline is, and the outlines.
//!
//! An outline is a set of closed contours made of straight lines and
//! quadratic Bézier curves. Composite glyphs (accented letters, usually)
//! are other glyphs placed and scaled. [`Font::rasterize`] scales an
//! outline to a pixel size, flattens the curves into lines, and works out
//! how much of each pixel is covered (see `raster.rs`).
//!
//! Fonts' hinting programs (bytecode that nudges outlines onto the pixel
//! grid) are ignored: anti-aliasing alone looks right on today's screens,
//! and the interpreter would be most of the code.

#![no_std]

extern crate alloc;

mod raster;

use alloc::vec::Vec;

pub use raster::fill;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The file ends early or a table points outside it.
    Truncated,
    /// A table every font needs is missing.
    MissingTable(&'static str),
    /// A font of a kind this doesn't read (PostScript outlines, say).
    Unsupported(&'static str),
}

/// A point in font units (y grows upwards) or pixels (y grows downwards).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn midpoint(self, other: Point) -> Point {
        Point::new((self.x + other.x) / 2.0, (self.y + other.y) / 2.0)
    }
}

/// A piece of an outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Line(Point, Point),
    /// From, control point, to.
    Quad(Point, Point, Point),
}

/// The vertical measurements of a line of text, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    /// How far the tallest glyphs reach above the baseline.
    pub ascent: f32,
    /// How far glyphs reach below the baseline (positive).
    pub descent: f32,
    /// Extra space the font asks for between lines.
    pub line_gap: f32,
}

impl LineMetrics {
    /// From one baseline to the next.
    pub fn height(&self) -> f32 {
        self.ascent + self.descent + self.line_gap
    }
}

/// A rasterized glyph: how much of each pixel the glyph covers (0 to
/// 255), row by row, and where the bitmap goes relative to the pen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    /// From the pen to the bitmap's left edge, in pixels.
    pub left: i32,
    /// From the baseline up to the bitmap's top edge, in pixels.
    pub top: i32,
    pub coverage: Vec<u8>,
}

/// Where a table is in the file.
#[derive(Debug, Clone, Copy)]
struct Table {
    offset: usize,
    length: usize,
}

/// The character map subtable in use.
#[derive(Debug, Clone, Copy)]
enum CharacterMap {
    /// Format 4: 16-bit characters in segments.
    Segments(usize),
    /// Format 12: any character, in groups of consecutive glyphs.
    Groups(usize),
    None,
}

/// Composite glyphs can contain composites; this deep is enough for any
/// real font and stops a malicious one from recursing forever.
const MAX_COMPOSITE_DEPTH: u32 = 8;

pub struct Font {
    data: Vec<u8>,
    units_per_em: u16,
    glyph_count: u16,
    long_offsets: bool,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    metric_count: u16,
    loca: Table,
    glyf: Table,
    hmtx: Table,
    character_map: CharacterMap,
}

impl Font {
    /// Reads a font file's tables.
    pub fn parse(data: Vec<u8>) -> Result<Font, Error> {
        match read_u32(&data, 0).ok_or(Error::Truncated)? {
            0x0001_0000 | 0x7472_7565 => {} // TrueType ("true" on old Macs)
            0x4F54_544F => return Err(Error::Unsupported("PostScript (CFF) outlines")),
            0x7474_6366 => return Err(Error::Unsupported("font collections")),
            _ => return Err(Error::Unsupported("not a TrueType font")),
        }
        let table = |tag: &'static str| find_table(&data, tag)?.ok_or(Error::MissingTable(tag));
        let head = table("head")?;
        let maxp = table("maxp")?;
        let hhea = table("hhea")?;
        let hmtx = table("hmtx")?;
        let loca = table("loca")?;
        let glyf = table("glyf")?;
        let cmap = table("cmap")?;

        let truncated = Error::Truncated;
        let units_per_em = read_u16(&data, head.offset + 18).ok_or(truncated)?;
        if !(16..=16384).contains(&units_per_em) {
            return Err(Error::Unsupported("strange units per em"));
        }
        let character_map = choose_character_map(&data, cmap).ok_or(truncated)?;
        Ok(Font {
            units_per_em,
            glyph_count: read_u16(&data, maxp.offset + 4).ok_or(truncated)?,
            long_offsets: read_u16(&data, head.offset + 50).ok_or(truncated)? != 0,
            ascender: read_i16(&data, hhea.offset + 4).ok_or(truncated)?,
            descender: read_i16(&data, hhea.offset + 6).ok_or(truncated)?,
            line_gap: read_i16(&data, hhea.offset + 8).ok_or(truncated)?,
            metric_count: read_u16(&data, hhea.offset + 34).ok_or(truncated)?.max(1),
            loca,
            glyf,
            hmtx,
            character_map,
            data,
        })
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    pub fn glyph_count(&self) -> u16 {
        self.glyph_count
    }

    /// The glyph that draws `c`, or 0 (the font's "missing" glyph, usually
    /// a box) if the font doesn't have it.
    pub fn glyph_index(&self, c: char) -> u16 {
        let c = c as u32;
        let glyph = match self.character_map {
            CharacterMap::Segments(at) => self.lookup_segments(at, c),
            CharacterMap::Groups(at) => self.lookup_groups(at, c),
            CharacterMap::None => None,
        };
        glyph.filter(|&glyph| glyph < self.glyph_count).unwrap_or(0)
    }

    /// Whether the font has a glyph for `c`.
    pub fn has(&self, c: char) -> bool {
        self.glyph_index(c) != 0
    }

    fn lookup_segments(&self, at: usize, c: u32) -> Option<u16> {
        let data = &self.data;
        let c = u16::try_from(c).ok()?;
        let segments = usize::from(read_u16(data, at + 6)? / 2);
        let ends = at + 14;
        let starts = ends + segments * 2 + 2;
        let deltas = starts + segments * 2;
        let ranges = deltas + segments * 2;
        // The first segment ending at or after `c` (they're sorted).
        let (mut low, mut high) = (0, segments);
        while low < high {
            let middle = (low + high) / 2;
            if read_u16(data, ends + middle * 2)? < c {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let i = low;
        if i == segments {
            return None;
        }
        let start = read_u16(data, starts + i * 2)?;
        if start > c {
            return None;
        }
        let delta = read_u16(data, deltas + i * 2)?;
        let range = usize::from(read_u16(data, ranges + i * 2)?);
        if range == 0 {
            return Some(c.wrapping_add(delta));
        }
        // An offset from this very field into the glyph array.
        let glyph = read_u16(data, ranges + i * 2 + range + usize::from(c - start) * 2)?;
        if glyph == 0 { None } else { Some(glyph.wrapping_add(delta)) }
    }

    fn lookup_groups(&self, at: usize, c: u32) -> Option<u16> {
        let data = &self.data;
        let count = read_u32(data, at + 12)? as usize;
        let groups = at + 16;
        let (mut low, mut high) = (0, count);
        while low < high {
            let middle = (low + high) / 2;
            let group = groups + middle * 12;
            if read_u32(data, group + 4)? < c {
                low = middle + 1;
            } else if read_u32(data, group)? > c {
                high = middle;
            } else {
                let glyph = read_u32(data, group + 8)? + (c - read_u32(data, group)?);
                return u16::try_from(glyph).ok();
            }
        }
        None
    }

    /// How far `glyph` moves the pen, in font units.
    pub fn advance_width(&self, glyph: u16) -> u16 {
        let index = glyph.min(self.metric_count - 1);
        read_u16(&self.data, self.hmtx.offset + usize::from(index) * 4).unwrap_or(0)
    }

    /// The advance in pixels at `size` pixels per em.
    pub fn advance(&self, glyph: u16, size: f32) -> f32 {
        f32::from(self.advance_width(glyph)) * self.scale(size)
    }

    pub fn line_metrics(&self, size: f32) -> LineMetrics {
        let scale = self.scale(size);
        LineMetrics {
            ascent: f32::from(self.ascender) * scale,
            descent: -f32::from(self.descender) * scale,
            line_gap: f32::from(self.line_gap) * scale,
        }
    }

    fn scale(&self, size: f32) -> f32 {
        size / f32::from(self.units_per_em)
    }

    /// Where `glyph`'s outline is in `glyf` (empty for blank glyphs like
    /// the space).
    fn glyph_data(&self, glyph: u16) -> Option<&[u8]> {
        if glyph >= self.glyph_count {
            return None;
        }
        let index = usize::from(glyph);
        let (start, end) = if self.long_offsets {
            let at = self.loca.offset + index * 4;
            (read_u32(&self.data, at)? as usize, read_u32(&self.data, at + 4)? as usize)
        } else {
            let at = self.loca.offset + index * 2;
            (usize::from(read_u16(&self.data, at)?) * 2, usize::from(read_u16(&self.data, at + 2)?) * 2)
        };
        if start >= end || end > self.glyf.length {
            return None;
        }
        self.data.get(self.glyf.offset + start..self.glyf.offset + end)
    }

    /// `glyph`'s outline in font units (y up). Empty for blank glyphs and
    /// for any the font gets wrong.
    pub fn outline(&self, glyph: u16) -> Vec<Segment> {
        let mut segments = Vec::new();
        self.add_outline(glyph, &Transform::IDENTITY, 0, &mut segments);
        segments
    }

    fn add_outline(&self, glyph: u16, transform: &Transform, depth: u32, out: &mut Vec<Segment>) {
        let Some(data) = self.glyph_data(glyph) else { return };
        let Some(contours) = read_i16(data, 0) else { return };
        if contours >= 0 {
            // A malformed glyph adds what it got before going wrong.
            let _ = add_simple_glyph(data, contours as usize, transform, out);
        } else if depth < MAX_COMPOSITE_DEPTH {
            let _ = self.add_composite_glyph(data, transform, depth, out);
        }
    }

    /// A composite glyph: a list of other glyphs, each with an offset and
    /// maybe a scale or a 2x2 matrix.
    fn add_composite_glyph(
        &self,
        data: &[u8],
        transform: &Transform,
        depth: u32,
        out: &mut Vec<Segment>,
    ) -> Option<()> {
        const WORD_ARGUMENTS: u16 = 0x0001;
        const ARGUMENTS_ARE_OFFSETS: u16 = 0x0002;
        const SCALE: u16 = 0x0008;
        const MORE: u16 = 0x0020;
        const X_AND_Y_SCALE: u16 = 0x0040;
        const TWO_BY_TWO: u16 = 0x0080;

        let mut at = 10;
        loop {
            let flags = read_u16(data, at)?;
            let component = read_u16(data, at + 2)?;
            at += 4;
            let (dx, dy) = if flags & WORD_ARGUMENTS != 0 {
                at += 4;
                (read_i16(data, at - 4)?, read_i16(data, at - 2)?)
            } else {
                at += 2;
                (i16::from(*data.get(at - 2)? as i8), i16::from(*data.get(at - 1)? as i8))
            };
            // Otherwise the arguments are point numbers to line up, which
            // only hinted fonts for odd scripts use: place it unmoved.
            let (dx, dy) = if flags & ARGUMENTS_ARE_OFFSETS != 0 { (f32::from(dx), f32::from(dy)) } else { (0.0, 0.0) };
            let f2dot14 = |at: usize| read_i16(data, at).map(|value| f32::from(value) / 16384.0);
            let mut part = Transform { xx: 1.0, xy: 0.0, yx: 0.0, yy: 1.0, dx, dy };
            if flags & SCALE != 0 {
                part.xx = f2dot14(at)?;
                part.yy = part.xx;
                at += 2;
            } else if flags & X_AND_Y_SCALE != 0 {
                part.xx = f2dot14(at)?;
                part.yy = f2dot14(at + 2)?;
                at += 4;
            } else if flags & TWO_BY_TWO != 0 {
                part.xx = f2dot14(at)?;
                part.yx = f2dot14(at + 2)?;
                part.xy = f2dot14(at + 4)?;
                part.yy = f2dot14(at + 6)?;
                at += 8;
            }
            self.add_outline(component, &transform.then(&part), depth + 1, out);
            if flags & MORE == 0 {
                return Some(());
            }
        }
    }

    /// `glyph` drawn at `size` pixels per em: how much of each pixel it
    /// covers. Blank glyphs give an empty bitmap.
    pub fn rasterize(&self, glyph: u16, size: f32) -> Bitmap {
        let scale = self.scale(size);
        let segments = self.outline(glyph);
        let empty = Bitmap { width: 0, height: 0, left: 0, top: 0, coverage: Vec::new() };
        let Some((min, max)) = bounds(&segments) else { return empty };
        let left = raster::floor(min.x * scale) as i32;
        let right = raster::ceil(max.x * scale) as i32;
        let top = raster::ceil(max.y * scale) as i32;
        let bottom = raster::floor(min.y * scale) as i32;
        let (width, height) = ((right - left) as usize, (top - bottom) as usize);
        if width == 0 || height == 0 {
            return empty;
        }
        // To pixels: y downwards from the bitmap's top-left corner.
        let place = |p: Point| Point::new(p.x * scale - left as f32, top as f32 - p.y * scale);
        let placed: Vec<Segment> = segments
            .iter()
            .map(|segment| match *segment {
                Segment::Line(a, b) => Segment::Line(place(a), place(b)),
                Segment::Quad(a, b, c) => Segment::Quad(place(a), place(b), place(c)),
            })
            .collect();
        Bitmap { width, height, left, top, coverage: fill(&placed, width, height) }
    }
}

/// An affine transform of font units (for composite glyphs' parts).
#[derive(Debug, Clone, Copy)]
struct Transform {
    xx: f32,
    xy: f32,
    yx: f32,
    yy: f32,
    dx: f32,
    dy: f32,
}

impl Transform {
    const IDENTITY: Transform = Transform { xx: 1.0, xy: 0.0, yx: 0.0, yy: 1.0, dx: 0.0, dy: 0.0 };

    fn apply(&self, x: f32, y: f32) -> Point {
        Point::new(self.xx * x + self.xy * y + self.dx, self.yx * x + self.yy * y + self.dy)
    }

    /// `inner` first, then this.
    fn then(&self, inner: &Transform) -> Transform {
        let origin = self.apply(inner.dx, inner.dy);
        Transform {
            xx: self.xx * inner.xx + self.xy * inner.yx,
            xy: self.xx * inner.xy + self.xy * inner.yy,
            yx: self.yx * inner.xx + self.yy * inner.yx,
            yy: self.yx * inner.xy + self.yy * inner.yy,
            dx: origin.x,
            dy: origin.y,
        }
    }
}

/// A simple glyph: the contours' last point numbers, the hinting program
/// (skipped), then a flag byte per point (on the curve or a control point,
/// and how its coordinates are stored), the x deltas, and the y deltas.
fn add_simple_glyph(data: &[u8], contours: usize, transform: &Transform, out: &mut Vec<Segment>) -> Option<()> {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;

    if contours == 0 {
        return Some(());
    }
    let mut ends = Vec::with_capacity(contours);
    for i in 0..contours {
        ends.push(usize::from(read_u16(data, 10 + i * 2)?));
    }
    let points = ends[contours - 1] + 1;
    let instructions = usize::from(read_u16(data, 10 + contours * 2)?);
    let mut at = 12 + contours * 2 + instructions;

    let mut flags = Vec::with_capacity(points);
    while flags.len() < points {
        let flag = *data.get(at)?;
        at += 1;
        let mut count = 1;
        if flag & REPEAT != 0 {
            count += usize::from(*data.get(at)?);
            at += 1;
        }
        for _ in 0..count.min(points - flags.len()) {
            flags.push(flag);
        }
    }

    // Coordinates are deltas from the previous point: a byte with the sign
    // in the flags, a repeat of the last one, or a signed 16-bit number.
    let mut read_coordinates = |short: u8, same_or_positive: u8| -> Option<Vec<i32>> {
        let mut values = Vec::with_capacity(points);
        let mut value = 0i32;
        for &flag in &flags {
            if flag & short != 0 {
                let delta = i32::from(*data.get(at)?);
                at += 1;
                value += if flag & same_or_positive != 0 { delta } else { -delta };
            } else if flag & same_or_positive == 0 {
                value += i32::from(read_i16(data, at)?);
                at += 2;
            }
            values.push(value);
        }
        Some(values)
    };
    let xs = read_coordinates(X_SHORT, X_SAME_OR_POSITIVE)?;
    let ys = read_coordinates(Y_SHORT, Y_SAME_OR_POSITIVE)?;

    let mut start = 0;
    for &end in &ends {
        if end < start || end >= points {
            return None;
        }
        let contour: Vec<(Point, bool)> =
            (start..=end).map(|i| (transform.apply(xs[i] as f32, ys[i] as f32), flags[i] & ON_CURVE != 0)).collect();
        add_contour(&contour, out);
        start = end + 1;
    }
    Some(())
}

/// Turns a contour's points into segments. Two control points in a row
/// have an implied on-curve point halfway between them, and the contour
/// may start with a control point (or even consist of nothing else).
fn add_contour(points: &[(Point, bool)], out: &mut Vec<Segment>) {
    let Some(&(first, first_on)) = points.first() else { return };
    let &(last, last_on) = points.last().unwrap_or(&(first, first_on));
    let (start, rest) = if first_on {
        (first, &points[1..])
    } else if last_on {
        (last, &points[..points.len() - 1])
    } else {
        (last.midpoint(first), points)
    };
    let mut current = start;
    let mut control: Option<Point> = None;
    for &(point, on_curve) in rest {
        match (on_curve, control) {
            (true, Some(c)) => out.push(Segment::Quad(current, c, point)),
            (true, None) => out.push(Segment::Line(current, point)),
            (false, Some(c)) => {
                let middle = c.midpoint(point);
                out.push(Segment::Quad(current, c, middle));
                current = middle;
            }
            (false, None) => {}
        }
        if on_curve {
            current = point;
            control = None;
        } else {
            control = Some(point);
        }
    }
    match control {
        Some(c) => out.push(Segment::Quad(current, c, start)),
        None if current != start => out.push(Segment::Line(current, start)),
        None => {}
    }
}

/// The smallest box around all the segments' points (control points
/// included, which bound the curves).
fn bounds(segments: &[Segment]) -> Option<(Point, Point)> {
    let mut points = segments.iter().flat_map(|segment| match *segment {
        Segment::Line(a, b) => [a, b, b],
        Segment::Quad(a, b, c) => [a, b, c],
    });
    let first = points.next()?;
    Some(points.fold((first, first), |(min, max), p| {
        (Point::new(min.x.min(p.x), min.y.min(p.y)), Point::new(max.x.max(p.x), max.y.max(p.y)))
    }))
}

/// Picks the character map: a full-Unicode one (format 12) if there is
/// one, otherwise one covering the first 65536 characters (format 4).
fn choose_character_map(data: &[u8], cmap: Table) -> Option<CharacterMap> {
    let count = usize::from(read_u16(data, cmap.offset + 2)?);
    let mut best = CharacterMap::None;
    for i in 0..count {
        let record = cmap.offset + 4 + i * 8;
        let platform = read_u16(data, record)?;
        let encoding = read_u16(data, record + 2)?;
        let at = cmap.offset + read_u32(data, record + 4)? as usize;
        // Unicode, or Windows' Unicode encodings (BMP and full).
        let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
        if !unicode {
            continue;
        }
        match read_u16(data, at)? {
            12 => return Some(CharacterMap::Groups(at)),
            4 => best = CharacterMap::Segments(at),
            _ => {}
        }
    }
    Some(best)
}

fn find_table(data: &[u8], tag: &str) -> Result<Option<Table>, Error> {
    let count = usize::from(read_u16(data, 4).ok_or(Error::Truncated)?);
    for i in 0..count {
        let record = 12 + i * 16;
        let name = data.get(record..record + 4).ok_or(Error::Truncated)?;
        if name == tag.as_bytes() {
            let offset = read_u32(data, record + 8).ok_or(Error::Truncated)? as usize;
            let length = read_u32(data, record + 12).ok_or(Error::Truncated)? as usize;
            if offset.checked_add(length).is_none_or(|end| end > data.len()) {
                return Err(Error::Truncated);
            }
            return Ok(Some(Table { offset, length }));
        }
    }
    Ok(None)
}

// TrueType numbers are big-endian.

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn read_i16(data: &[u8], at: usize) -> Option<i16> {
    read_u16(data, at).map(|value| value as i16)
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests;
