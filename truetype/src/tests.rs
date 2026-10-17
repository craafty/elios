extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use std::string::String;

use super::*;

/// The font the system ships, so these check what people will see.
fn font() -> Font {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rootfs/usr/share/fonts/dejavu/DejaVuSansMono.ttf");
    Font::parse(std::fs::read(path).expect("the bundled font")).expect("a TrueType font")
}

#[test]
fn tables_and_metrics() {
    let font = font();
    assert_eq!(font.units_per_em(), 2048);
    assert!(font.glyph_count() > 3000);
    // Monospaced: every glyph moves the pen the same distance.
    let advance = font.advance_width(font.glyph_index('M'));
    assert_eq!(advance, 1233);
    for c in ['i', 'W', ' ', '.', 'é', '─'] {
        assert_eq!(font.advance_width(font.glyph_index(c)), advance, "{c}");
    }
    let metrics = font.line_metrics(2048.0);
    assert_eq!((metrics.ascent, metrics.descent), (1901.0, 483.0));
}

#[test]
fn character_map() {
    let font = font();
    let a = font.glyph_index('A');
    assert_ne!(a, 0);
    assert_eq!(font.glyph_index('B'), a + 1);
    assert!(font.has('€') && font.has('█') && font.has('ж'));
    assert!(!font.has('\u{10FFFF}') && !font.has('\u{E000}'));
}

#[test]
fn outlines_are_closed() {
    let font = font();
    for c in ['A', 'O', 'g', '@', 'Ä', 'ő', '%'] {
        let outline = font.outline(font.glyph_index(c));
        assert!(!outline.is_empty(), "{c}");
        // Each segment starts where one ended, and every contour returns
        // to its start, so the ends cancel out.
        let (mut x, mut y) = (0.0, 0.0);
        for segment in &outline {
            let (from, to) = match *segment {
                Segment::Line(a, b) | Segment::Quad(a, _, b) => (a, b),
            };
            x += to.x - from.x;
            y += to.y - from.y;
        }
        assert!(x.abs() < 0.01 && y.abs() < 0.01, "{c}: {x} {y}");
    }
    assert!(font.outline(font.glyph_index(' ')).is_empty());
}

#[test]
fn composite_glyphs() {
    let font = font();
    // "Ä" is "A" plus a diaeresis placed above it.
    let a = font.outline(font.glyph_index('A'));
    let umlaut = font.outline(font.glyph_index('Ä'));
    assert!(umlaut.len() > a.len());
    let top = |outline: &[Segment]| bounds(outline).unwrap().1.y;
    assert!(top(&umlaut) > top(&a) + 200.0);
}

#[test]
fn half_pixel_square() {
    // A 2x2 square shifted half a pixel: quarter-covered corners,
    // half-covered edges, a fully covered middle.
    let p = |x, y| Point::new(x, y);
    let square = [
        Segment::Line(p(0.5, 0.5), p(0.5, 2.5)),
        Segment::Line(p(0.5, 2.5), p(2.5, 2.5)),
        Segment::Line(p(2.5, 2.5), p(2.5, 0.5)),
        Segment::Line(p(2.5, 0.5), p(0.5, 0.5)),
    ];
    let coverage = fill(&square, 3, 3);
    assert_eq!(coverage, vec![64, 128, 64, 128, 255, 128, 64, 128, 64]);
}

#[test]
fn diagonal_halves() {
    // A triangle cut along a diagonal: the pixels it crosses are half
    // covered, those below it fully.
    let p = |x, y| Point::new(x, y);
    let triangle = [
        Segment::Line(p(0.0, 0.0), p(4.0, 4.0)),
        Segment::Line(p(4.0, 4.0), p(0.0, 4.0)),
        Segment::Line(p(0.0, 4.0), p(0.0, 0.0)),
    ];
    let coverage = fill(&triangle, 4, 4);
    for row in 0..4 {
        for column in 0..4 {
            let expected = match column.cmp(&row) {
                core::cmp::Ordering::Less => 255,
                core::cmp::Ordering::Equal => 128,
                core::cmp::Ordering::Greater => 0,
            };
            assert_eq!(coverage[row * 4 + column], expected, "({column}, {row})");
        }
    }
}

#[test]
fn full_block() {
    // U+2588 fills the whole cell: inside, every pixel is fully covered,
    // and the total is the block's area.
    let font = font();
    let size = 32.0;
    let block = font.rasterize(font.glyph_index('█'), size);
    let rows: Vec<&[u8]> = block.coverage.chunks(block.width).collect();
    for row in &rows[1..block.height - 1] {
        assert!(row[1..block.width - 1].iter().all(|&c| c == 255));
    }
    let outline = font.outline(font.glyph_index('█'));
    let (min, max) = bounds(&outline).unwrap();
    let scale = size / 2048.0;
    let area = (max.x - min.x) * (max.y - min.y) * scale * scale;
    let total: f32 = block.coverage.iter().map(|&c| f32::from(c) / 255.0).sum();
    assert!((total - area).abs() < 0.5, "{total} vs {area}");
}

#[test]
fn letters_look_right() {
    let font = font();
    // "l" at 48 pixels: a solid stem down the middle.
    let l = font.rasterize(font.glyph_index('|'), 48.0);
    let middle = l.width / 2;
    assert!((2..l.height - 2).all(|row| l.coverage[row * l.width + middle] == 255));
    // "O": solid at the sides of the middle row, empty in the centre.
    let o = font.rasterize(font.glyph_index('O'), 48.0);
    let row = &o.coverage[o.height / 2 * o.width..][..o.width];
    assert_eq!(row[o.width / 2], 0);
    assert!(row[1..4].contains(&255) && row[o.width - 4..o.width - 1].contains(&255));
    // Placed on the baseline: "O" sits on it, "g" hangs below.
    assert!(o.top > 30 && o.top - o.height as i32 <= 0);
    let g = font.rasterize(font.glyph_index('g'), 48.0);
    assert!(g.top - (g.height as i32) < -5);
}

/// `TRUETYPE_SAMPLE=/tmp/sample.pgm cargo test -p truetype` draws some
/// text, to look at.
#[test]
fn sample() {
    let Ok(path) = std::env::var("TRUETYPE_SAMPLE") else { return };
    let font = font();
    let lines = [
        (11.0, "The quick brown fox jumps over the lazy dog 0123456789"),
        (13.0, "The quick brown fox jumps over the lazy dog {}[]()<>"),
        (15.0, "The quick brown fox jumps over the lazy dog Ää Øø ß €"),
        (17.0, "$ eli add firefox   ~/Documents/notes.txt  ┌─┐"),
        (24.0, "Il1| O0o ag @&% ┌─┬─┐"),
        (48.0, "Ag@ß"),
    ];
    let (width, height) = (800usize, 220usize);
    let mut image = vec![255u8; width * height];
    let mut baseline = 0.0;
    for (size, text) in lines {
        let metrics = font.line_metrics(size);
        baseline += metrics.height();
        let mut pen = 4.0;
        for c in text.chars() {
            let glyph = font.glyph_index(c);
            let bitmap = font.rasterize(glyph, size);
            let x0 = pen as i32 + bitmap.left;
            let y0 = baseline as i32 - bitmap.top;
            for y in 0..bitmap.height {
                for x in 0..bitmap.width {
                    let (px, py) = (x0 + x as i32, y0 + y as i32);
                    if px >= 0 && py >= 0 && (px as usize) < width && (py as usize) < height {
                        let pixel = &mut image[py as usize * width + px as usize];
                        *pixel = pixel.saturating_sub(bitmap.coverage[y * bitmap.width + x]);
                    }
                }
            }
            pen += font.advance(glyph, size);
        }
    }
    let mut file = String::from("P5\n").into_bytes();
    file.extend_from_slice(std::format!("{width} {height}\n255\n").as_bytes());
    file.extend_from_slice(&image);
    std::fs::write(path, file).unwrap();
}
