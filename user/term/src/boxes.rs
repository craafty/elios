//! Box-drawing and block characters, drawn rather than taken from the
//! font: a font's lines are drawn for its own line height and leave gaps
//! between rows of a terminal, so tables, borders and progress bars look
//! broken. Drawn to fill the cell exactly, they join up.

use ui::draw::{Path, mix};
use ui::{Canvas, Rect};

/// Arms of U+2500 to U+257F: up, right, down, left; 0 none, 1 light,
/// 2 heavy, 3 double. Dashed lines are drawn solid.
const LINES: [&[u8; 4]; 128] = [
    b"0101", b"0202", b"1010", b"2020", b"0101", b"0202", b"1010", b"2020", // ─━│┃┄┅┆┇
    b"0101", b"0202", b"1010", b"2020", b"0110", b"0210", b"0120", b"0220", // ┈┉┊┋┌┍┎┏
    b"0011", b"0012", b"0021", b"0022", b"1100", b"1200", b"2100", b"2200", // ┐┑┒┓└┕┖┗
    b"1001", b"1002", b"2001", b"2002", b"1110", b"1210", b"2110", b"1120", // ┘┙┚┛├┝┞┟
    b"2120", b"2210", b"1220", b"2220", b"1011", b"1012", b"2011", b"1021", // ┠┡┢┣┤┥┦┧
    b"2021", b"2012", b"1022", b"2022", b"0111", b"0112", b"0211", b"0212", // ┨┩┪┫┬┭┮┯
    b"0121", b"0122", b"0221", b"0222", b"1101", b"1102", b"1201", b"1202", // ┰┱┲┳┴┵┶┷
    b"2101", b"2102", b"2201", b"2202", b"1111", b"1112", b"1211", b"1212", // ┸┹┺┻┼┽┾┿
    b"2111", b"1121", b"2121", b"2112", b"2211", b"1122", b"1221", b"2212", // ╀╁╂╃╄╅╆╇
    b"1222", b"2122", b"2221", b"2222", b"0101", b"0202", b"1010", b"2020", // ╈╉╊╋╌╍╎╏
    b"0303", b"3030", b"0310", b"0130", b"0330", b"0013", b"0031", b"0033", // ═║╒╓╔╕╖╗
    b"1300", b"3100", b"3300", b"1003", b"3001", b"3003", b"1310", b"3130", // ╘╙╚╛╜╝╞╟
    b"3330", b"1013", b"3031", b"3033", b"0313", b"0131", b"0333", b"1303", // ╠╡╢╣╤╥╦╧
    b"3101", b"3303", b"1313", b"3131", b"3333", b"0110", b"0011", b"1001", // ╨╩╪╫╬╭╮╯
    b"1100", b"0000", b"0000", b"0000", b"0001", b"1000", b"0100", b"0010", // ╰╱╲╳╴╵╶╷
    b"0002", b"2000", b"0200", b"0020", b"0201", b"1020", b"0102", b"2010", // ╸╹╺╻╼╽╾╿
];

/// Draws `c` filling `cell`, if it's one of ours; returns whether it was.
pub fn draw(canvas: &mut Canvas, c: char, cell: Rect, color: u32, background: u32) -> bool {
    let code = c as u32;
    match code {
        0x2571..=0x2573 => diagonals(canvas, code, cell, color),
        0x256D..=0x2570 => arc(canvas, code, cell, color),
        0x2500..=0x257F => lines(canvas, LINES[(code - 0x2500) as usize], cell, color),
        0x2580..=0x259F => blocks(canvas, code, cell, color, background),
        _ => return false,
    }
    true
}

/// How thick a light line is in a cell this wide.
fn light(cell: Rect) -> i32 {
    (cell.width / 8).max(1)
}

fn lines(canvas: &mut Canvas, arms: &[u8; 4], cell: Rect, color: u32) {
    let weight = |arm: usize| arms[arm] - b'0';
    let (up, right, down, left) = (weight(0), weight(1), weight(2), weight(3));
    // Horizontal arms run along x; the arms across them are up and down.
    arm_pair(canvas, (right, left), (up, down), (cell.x, cell.width), (cell.y, cell.height), color, true);
    arm_pair(canvas, (down, up), (left, right), (cell.y, cell.height), (cell.x, cell.width), color, false);
}

/// Draws the two arms along one axis: `arms` is (towards the far edge,
/// towards the near edge) and `across` the arms on the other axis (near
/// side first). `along` and `side` are the cell's (start, length) on this
/// axis and the other; `horizontal` says which axis is x.
fn arm_pair(
    canvas: &mut Canvas,
    arms: (u8, u8),
    across: (u8, u8),
    along: (i32, i32),
    side: (i32, i32),
    color: u32,
    horizontal: bool,
) {
    let t = light(Rect::new(0, 0, if horizontal { along.1 } else { side.1 }, 0));
    let thickness = |weight: u8| if weight == 2 { t * 2 + 1 } else { t };
    // Where a stroke of thickness `thick` starts, centred, on each axis.
    let centre_along = |thick: i32| along.0 + (along.1 - thick) / 2;
    let centre_side = |thick: i32| side.0 + (side.1 - thick) / 2;
    let c0 = centre_along(t);
    let crossing = across.0.max(across.1);
    let end = along.0 + along.1;
    for (far, weight, opposite) in [(true, arms.0, arms.1), (false, arms.1, arms.0)] {
        if weight == 0 {
            continue;
        }
        // (offset across, thickness, the crossing arm on that side).
        let strokes: &[(i32, i32, u8)] = &match weight {
            3 => [(centre_side(t) - t, t, across.0), (centre_side(t) + t, t, across.1)],
            weight => [(centre_side(thickness(weight)), thickness(weight), 0); 2],
        };
        let strokes = if weight == 3 { strokes } else { &strokes[..1] };
        for &(offset, thick, perpendicular) in strokes {
            // The stretch of the middle this arm covers, as (start, end)
            // for an arm to the far edge; mirrored for the near one.
            let (from_far, to_near) = match (weight, crossing) {
                (_, 0) => (c0, c0 + t),
                // Double lines meeting double lines: the stroke on a side
                // with an arm stops at it; the other turns the corner.
                (3, 3) if perpendicular > 0 => (c0 + t, c0),
                (3, 3) => (c0 - t, c0 + 2 * t),
                (3, _) => (c0, c0 + t),
                // A single line meeting a double one: through both if it
                // carries on, else only to the nearer.
                (_, 3) if opposite > 0 => (c0 - t, c0 + 2 * t),
                (_, 3) => (c0 + t, c0),
                (_, crossing) => {
                    (centre_along(thickness(crossing)), centre_along(thickness(crossing)) + thickness(crossing))
                }
            };
            let (start, stop) = if far { (from_far, end) } else { (along.0, to_near) };
            let rect = if horizontal {
                Rect::new(start, offset, stop - start, thick)
            } else {
                Rect::new(offset, start, thick, stop - start)
            };
            canvas.fill(rect, color);
        }
    }
}

/// ╭╮╯╰: a light line turning a rounded corner.
fn arc(canvas: &mut Canvas, code: u32, cell: Rect, color: u32) {
    // Which way the arms go: right (+1) or left, down (+1) or up.
    let (h, v) = match code {
        0x256D => (1.0, 1.0),
        0x256E => (-1.0, 1.0),
        0x256F => (-1.0, -1.0),
        _ => (1.0, -1.0),
    };
    let t = light(cell);
    let half = t as f32 / 2.0;
    let xc = (cell.x + (cell.width - t) / 2) as f32 + half;
    let yc = (cell.y + (cell.height - t) / 2) as f32 + half;
    let x_end = if h > 0.0 { cell.right() as f32 } else { cell.x as f32 };
    let y_end = if v > 0.0 { cell.bottom() as f32 } else { cell.y as f32 };
    let (outer_x, outer_y) = (xc - h * half, yc - v * half);
    let (inner_x, inner_y) = (xc + h * half, yc + v * half);
    let mut path = Path::new();
    path.move_to(outer_x, y_end)
        .quad_to(outer_x, outer_y, x_end, outer_y)
        .line_to(x_end, inner_y)
        .quad_to(inner_x, inner_y, inner_x, y_end)
        .close();
    canvas.fill_path(&path, color);
}

/// ╱╲╳: corner to corner.
fn diagonals(canvas: &mut Canvas, code: u32, cell: Rect, color: u32) {
    let t = light(cell) as f32 * 0.75;
    let (left, top) = (cell.x as f32, cell.y as f32);
    let (right, bottom) = (cell.right() as f32, cell.bottom() as f32);
    if code != 0x2572 {
        canvas.fill_path(&Path::polygon(&[(right - t, top), (right, top), (left + t, bottom), (left, bottom)]), color);
    }
    if code != 0x2571 {
        canvas.fill_path(&Path::polygon(&[(left, top), (left + t, top), (right, bottom), (right - t, bottom)]), color);
    }
}

/// U+2580 to U+259F: halves, eighths, shades and quadrants.
fn blocks(canvas: &mut Canvas, code: u32, cell: Rect, color: u32, background: u32) {
    let (x, y, w, h) = (cell.x, cell.y, cell.width, cell.height);
    // The part of the cell `from..to` eighths across or down.
    let rows = |from: i32, to: i32| Rect::new(x, y + h * from / 8, w, h * to / 8 - h * from / 8);
    let columns = |from: i32, to: i32| Rect::new(x + w * from / 8, y, w * to / 8 - w * from / 8, h);
    let quadrant = |right: bool, lower: bool| {
        let (half_w, half_h) = (w / 2, h / 2);
        Rect::new(
            if right { x + half_w } else { x },
            if lower { y + half_h } else { y },
            if right { w - half_w } else { half_w },
            if lower { h - half_h } else { half_h },
        )
    };
    match code {
        0x2580 => canvas.fill(rows(0, 4), color),
        0x2581..=0x2588 => canvas.fill(rows(8 - (code - 0x2580) as i32, 8), color),
        0x2589..=0x258F => canvas.fill(columns(0, 8 - (code - 0x2588) as i32), color),
        0x2590 => canvas.fill(columns(4, 8), color),
        0x2591..=0x2593 => {
            let amount = (code - 0x2590) * 64;
            canvas.fill(cell, mix(background, color, amount));
        }
        0x2594 => canvas.fill(rows(0, 1), color),
        0x2595 => canvas.fill(columns(7, 8), color),
        _ => {
            // Quadrants: upper left, upper right, lower left, lower right.
            let parts: [bool; 4] = match code {
                0x2596 => [false, false, true, false],
                0x2597 => [false, false, false, true],
                0x2598 => [true, false, false, false],
                0x2599 => [true, false, true, true],
                0x259A => [true, false, false, true],
                0x259B => [true, true, true, false],
                0x259C => [true, true, false, true],
                0x259D => [false, true, false, false],
                0x259E => [false, true, true, false],
                _ => [false, true, true, true],
            };
            for (index, &on) in parts.iter().enumerate() {
                if on {
                    canvas.fill(quadrant(index % 2 == 1, index >= 2), color);
                }
            }
        }
    }
}
