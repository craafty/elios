//! The 8254 Programmable Interval Timer.
//!
//! Its clock runs at a fixed, known frequency, which makes it the reference
//! for measuring the other timers. eliOS uses it only for that: channel 2 can
//! be polled without interrupts, through the gate and output bits in port
//! 0x61.

use super::{inb, outb};

/// The PIT's input clock.
pub const FREQUENCY_HZ: u64 = 1_193_182;

const CHANNEL_2_DATA: u16 = 0x42;
const MODE_COMMAND: u16 = 0x43;
/// Port 0x61: bit 0 gates channel 2, bit 1 connects it to the speaker,
/// bit 5 reads channel 2's output.
const PORT_B: u16 = 0x61;

/// Busy-waits for `ticks` periods of the PIT clock (at most about 55 ms).
pub fn busy_wait(ticks: u16) {
    // SAFETY: programs only channel 2, which drives nothing but the speaker,
    // and the speaker stays disconnected.
    unsafe {
        let port_b = inb(PORT_B);
        outb(PORT_B, (port_b & !0x02) | 0x01);
        // Channel 2, low byte then high byte, mode 0 (interrupt on terminal
        // count: the output goes high when the count reaches zero), binary.
        outb(MODE_COMMAND, 0b1011_0000);
        outb(CHANNEL_2_DATA, ticks as u8);
        outb(CHANNEL_2_DATA, (ticks >> 8) as u8);
        while inb(PORT_B) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        outb(PORT_B, port_b);
    }
}
