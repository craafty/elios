//! The legacy 8259 interrupt controllers, which eliOS doesn't use.
//!
//! They still have to be dealt with: firmware may leave them enabled, and
//! their default vectors overlap CPU exceptions. So we move them out of the
//! way and mask every line. The APICs take over from here.

use super::interrupts::PIC_BASE;
use super::{io_wait, outb};

const PRIMARY_COMMAND: u16 = 0x20;
const PRIMARY_DATA: u16 = 0x21;
const SECONDARY_COMMAND: u16 = 0xA0;
const SECONDARY_DATA: u16 = 0xA1;

pub fn disable() {
    // SAFETY: the standard initialization sequence, which ends with every
    // interrupt line masked.
    unsafe {
        // ICW1: start initialization, expect ICW4.
        outb(PRIMARY_COMMAND, 0x11);
        io_wait();
        outb(SECONDARY_COMMAND, 0x11);
        io_wait();
        // ICW2: vector offsets, so any spurious interrupt lands on vectors
        // we recognize instead of on CPU exceptions.
        outb(PRIMARY_DATA, PIC_BASE);
        io_wait();
        outb(SECONDARY_DATA, PIC_BASE + 8);
        io_wait();
        // ICW3: the secondary is cascaded on the primary's line 2.
        outb(PRIMARY_DATA, 1 << 2);
        io_wait();
        outb(SECONDARY_DATA, 2);
        io_wait();
        // ICW4: 8086 mode.
        outb(PRIMARY_DATA, 0x01);
        io_wait();
        outb(SECONDARY_DATA, 0x01);
        io_wait();
        // Mask everything.
        outb(PRIMARY_DATA, 0xFF);
        outb(SECONDARY_DATA, 0xFF);
    }
}
