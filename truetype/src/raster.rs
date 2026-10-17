//! Filling outlines with exact anti-aliasing.
//!
//! Each line of the outline is walked a row of pixels at a time, and adds
//! to every pixel it passes the signed area between it and the pixel's
//! left edge (positive going down, negative going up). Summing along a row
//! then gives, for each pixel, how much of it lies inside the outline: an
//! edge going down at x adds its height to everything to its right, the
//! edge coming back up takes it away again, and pixels the edges pass
//! through get the fractional part. It's exact for straight lines, needs
//! no sorting of edges or sample points, and costs one pass over the lines
//! plus one over the pixels. Curves are flattened into short lines first,
//! fine enough that the error is a small fraction of a pixel.
//!
//! The sums run across row ends too: every row's areas add up to zero
//! (each row is crossed downwards as often as upwards), so a line touching
//! a row's last pixel boundary can spill into the next row's first cell
//! without changing anything.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Point, Segment};

/// How far (squared, in pixels) a curve may bend before it's split into
/// more than one line.
const FLAT_ENOUGH: f32 = 0.333;
/// Curve flattening tolerance: higher means more lines per curve.
const TOLERANCE: f32 = 3.0;

/// The coverage (0 to 255) of each pixel of a `width` by `height` bitmap by
/// the closed outline `segments` (in pixels, y downwards, inside the
/// bitmap).
pub fn fill(segments: &[Segment], width: usize, height: usize) -> Vec<u8> {
    // Spare cells for lines on the last pixel's right edge.
    let mut areas = vec![0.0f32; width * height + 2];
    let mut canvas = Canvas { areas: &mut areas, width, height };
    for segment in segments {
        match *segment {
            Segment::Line(a, b) => canvas.line(a, b),
            Segment::Quad(a, b, c) => canvas.quad(a, b, c),
        }
    }
    let mut sum = 0.0;
    areas[..width * height]
        .iter()
        .map(|&area| {
            sum += area;
            // Nonzero winding: overlapping parts count once.
            let coverage = if sum < 0.0 { -sum } else { sum };
            if coverage >= 1.0 { 255 } else { (coverage * 255.0 + 0.5) as u8 }
        })
        .collect()
}

struct Canvas<'a> {
    areas: &'a mut [f32],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    fn quad(&mut self, from: Point, control: Point, to: Point) {
        // How far the curve bends away from a straight line.
        let bend_x = from.x - 2.0 * control.x + to.x;
        let bend_y = from.y - 2.0 * control.y + to.y;
        let bend = bend_x * bend_x + bend_y * bend_y;
        if bend < FLAT_ENOUGH {
            self.line(from, to);
            return;
        }
        let pieces = 1 + floor(sqrt(sqrt(TOLERANCE * bend))) as usize;
        let mut previous = from;
        for i in 1..=pieces {
            let t = i as f32 / pieces as f32;
            let u = 1.0 - t;
            let point = Point::new(
                u * u * from.x + 2.0 * u * t * control.x + t * t * to.x,
                u * u * from.y + 2.0 * u * t * control.y + t * t * to.y,
            );
            self.line(previous, point);
            previous = point;
        }
    }

    fn line(&mut self, from: Point, to: Point) {
        if from.y == to.y {
            return; // Horizontal lines cover nothing.
        }
        let (direction, top, bottom) = if from.y < to.y { (1.0, from, to) } else { (-1.0, to, from) };
        let slope = (bottom.x - top.x) / (bottom.y - top.y);
        let right_edge = self.width as f32;
        let clamp_x = |x: f32| x.clamp(0.0, right_edge);
        let first_row = floor(top.y.max(0.0)) as usize;
        let last_row = (ceil(bottom.y) as usize).min(self.height);
        let mut x = clamp_x(top.x + (top.y.max(0.0) - top.y) * slope);
        for row in first_row..last_row {
            let row_start = row * self.width;
            let y0 = (row as f32).max(top.y);
            let y1 = ((row + 1) as f32).min(bottom.y);
            let height = y1 - y0;
            let next_x = clamp_x(x + slope * height);
            let d = height * direction;
            let (x0, x1) = if x < next_x { (x, next_x) } else { (next_x, x) };
            let x0_floor = floor(x0);
            let x0_cell = x0_floor as usize;
            let x1_cell = ceil(x1) as usize;
            if x1_cell <= x0_cell + 1 {
                // Within one pixel: split by the line's average position.
                let inside = (x + next_x) / 2.0 - x0_floor;
                self.areas[row_start + x0_cell] += d - d * inside;
                self.areas[row_start + x0_cell + 1] += d * inside;
            } else {
                // Across several: a triangle in the first pixel, strips in
                // the middle, and a triangle in the last.
                let per_x = 1.0 / (x1 - x0);
                let first_fraction = x0 - x0_floor;
                let first_area = 0.5 * per_x * (1.0 - first_fraction) * (1.0 - first_fraction);
                let last_fraction = x1 - x1_cell as f32 + 1.0;
                let last_area = 0.5 * per_x * last_fraction * last_fraction;
                self.areas[row_start + x0_cell] += d * first_area;
                if x1_cell == x0_cell + 2 {
                    self.areas[row_start + x0_cell + 1] += d * (1.0 - first_area - last_area);
                } else {
                    let second_area = per_x * (1.5 - first_fraction);
                    self.areas[row_start + x0_cell + 1] += d * (second_area - first_area);
                    for cell in x0_cell + 2..x1_cell - 1 {
                        self.areas[row_start + cell] += d * per_x;
                    }
                    let before_last = second_area + (x1_cell - x0_cell - 3) as f32 * per_x;
                    self.areas[row_start + x1_cell - 1] += d * (1.0 - before_last - last_area);
                }
                self.areas[row_start + x1_cell] += d * last_area;
            }
            x = next_x;
        }
    }
}

// `core` has no floor, ceil or square root for floats (they live in `std`,
// which calls the C library). These cover the ranges used here.

pub(crate) fn floor(x: f32) -> f32 {
    let truncated = x as i32 as f32;
    if truncated > x { truncated - 1.0 } else { truncated }
}

pub(crate) fn ceil(x: f32) -> f32 {
    let truncated = x as i32 as f32;
    if truncated < x { truncated + 1.0 } else { truncated }
}

fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    // Halving the exponent gets close; Newton's method does the rest.
    let mut root = f32::from_bits((x.to_bits() >> 1) + 0x1FC0_0000);
    for _ in 0..4 {
        root = 0.5 * (root + x / root);
    }
    root
}
