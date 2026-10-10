//! The eliOS logo, drawn at the top of the console at boot.

use crate::console::Console;
use crate::framebuffer::Color;

const ELI: Color = Color(0xF2F2F7);
const OS: Color = Color(0x4FD1FF);

/// 5x7 letters, one row per byte, leftmost pixel in bit 4.
const E: [u8; 7] = [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110];
const L: [u8; 7] = [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110];
const I: [u8; 7] = [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110];
const O: [u8; 7] = [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110];
const S: [u8; 7] = [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110];

const SCALE: usize = 4;
const MARGIN: usize = 16;

/// Draws the logo in the top left corner and moves the cursor below it.
pub fn draw(console: &mut Console) {
    let letters = [(&E, ELI), (&L, ELI), (&I, ELI), (&O, OS), (&S, OS)];
    for (index, (letter, color)) in letters.iter().enumerate() {
        let left = MARGIN + index * 6 * SCALE;
        for (row, bits) in letter.iter().enumerate() {
            for column in 0..5 {
                if bits & (0b10000 >> column) != 0 {
                    console.fill_rect(left + column * SCALE, MARGIN + row * SCALE, SCALE, SCALE, *color);
                }
            }
        }
    }

    let underline_top = MARGIN + 9 * SCALE;
    let logo_width = (letters.len() * 6 - 1) * SCALE;
    console.fill_rect(MARGIN, underline_top, logo_width, SCALE / 2, OS);
    console.move_below(underline_top + MARGIN);
}
