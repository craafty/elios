//! PS/2 mice, on the keyboard controller's second port. Laptops wire their
//! pointing devices here: the ThinkPad T440p's touchpad and TrackPoint both
//! speak the PS/2 mouse protocol (the touchpad passes the TrackPoint's
//! movements through), so one driver handles both.
//!
//! A mouse sends 3-byte packets: buttons and sign bits, then X and Y
//! movement. Mice with a wheel switch to 4-byte packets (the extra byte is
//! the wheel) after a "magic knock": setting the sample rate to 200, 100
//! and 80 in a row, after which they report ID 3 instead of 0 (Microsoft's
//! IntelliMouse convention, which every wheel mouse follows).
//!
//! Touchpads also have their own richer protocols (absolute positions,
//! several fingers); this driver uses only the basic mouse one.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::interrupts::{self, IRQ_BASE, InterruptFrame};
use crate::arch::{apic, inb};
use crate::input::{self, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, REL_WHEEL, REL_X, REL_Y};
use crate::keyboard::{
    READ_CONFIG, STATUS_FROM_MOUSE, STATUS_OUTPUT_FULL, WRITE_CONFIG, read_data, send_command, write_data,
};
use crate::sync::ByteQueue;
use crate::task::{self, WaitQueue};
use crate::time;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;

const ENABLE_SECOND_PORT: u8 = 0xA8;
const WRITE_SECOND_PORT: u8 = 0xD4;
#[cfg(test)]
const WRITE_SECOND_PORT_OUTPUT: u8 = 0xD3;
const CONFIG_SECOND_PORT_INTERRUPT: u8 = 1 << 1;
const CONFIG_SECOND_PORT_CLOCK_DISABLED: u8 = 1 << 5;

// Mouse commands.
const RESET: u8 = 0xFF;
const SET_DEFAULTS: u8 = 0xF6;
const ENABLE_REPORTING: u8 = 0xF4;
const SET_SAMPLE_RATE: u8 = 0xF3;
const GET_ID: u8 = 0xF2;
const ACK: u8 = 0xFA;
const SELF_TEST_PASSED: u8 = 0xAA;

const MOUSE_IRQ: u8 = 12;
const MOUSE_VECTOR: u8 = IRQ_BASE + MOUSE_IRQ;

static BYTES: ByteQueue<256> = ByteQueue::new();
static INPUT: WaitQueue = WaitQueue::new();

/// Finds and starts a mouse on the second port, if there is one.
pub fn init() -> Result<(), &'static str> {
    send_command(ENABLE_SECOND_PORT)?;
    // While setting it up, the mouse's bytes are read by polling.
    set_config(|config| config & !CONFIG_SECOND_PORT_INTERRUPT)?;
    set_config(|config| config & !CONFIG_SECOND_PORT_CLOCK_DISABLED)?;

    command(RESET)?;
    // The self-test can take most of a second.
    if read_mouse_byte(1000)? != SELF_TEST_PASSED {
        return Err("the mouse failed its self-test");
    }
    let _ = read_mouse_byte(100); // its ID, 0
    command(SET_DEFAULTS)?;
    for rate in [200, 100, 80] {
        command(SET_SAMPLE_RATE)?;
        command(rate)?;
    }
    command(GET_ID)?;
    let id = read_mouse_byte(100)?;
    let wheel = id == 3;
    command(SET_SAMPLE_RATE)?;
    command(100)?;
    command(ENABLE_REPORTING)?;

    let mut relative = alloc::vec![REL_X, REL_Y];
    if wheel {
        relative.push(REL_WHEEL);
    }
    let capabilities = input::Capabilities {
        keys: alloc::vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
        relative,
        leds: Vec::new(),
        properties: input::PROP_POINTER,
    };
    let device = input::register("PS/2 Generic Mouse", [input::BUS_I8042, 2, 1, 0], capabilities);

    interrupts::set_handler(MOUSE_VECTOR, on_interrupt);
    apic::route_isa_irq(MOUSE_IRQ, MOUSE_VECTOR)?;
    set_config(|config| config | CONFIG_SECOND_PORT_INTERRUPT)?;
    task::spawn("mouse", move || decode_forever(&device, if wheel { 4 } else { 3 }));
    Ok(())
}

fn set_config(change: impl FnOnce(u8) -> u8) -> Result<(), &'static str> {
    send_command(READ_CONFIG)?;
    let config = read_data()?;
    send_command(WRITE_CONFIG)?;
    write_data(change(config))
}

/// Sends a byte to the mouse and waits for its acknowledgment.
fn command(byte: u8) -> Result<(), &'static str> {
    send_command(WRITE_SECOND_PORT)?;
    write_data(byte)?;
    match read_mouse_byte(100)? {
        ACK => Ok(()),
        _ => Err("the mouse didn't acknowledge a command"),
    }
}

/// Waits up to `ms` milliseconds for a byte from the mouse, skipping any
/// from the keyboard.
fn read_mouse_byte(ms: u64) -> Result<u8, &'static str> {
    let deadline = time::uptime_nanos() + ms * 1_000_000;
    while time::uptime_nanos() < deadline {
        // SAFETY: reading the status port has no side effects; the data
        // port is read only when it has a byte.
        unsafe {
            let status = inb(STATUS);
            if status & STATUS_OUTPUT_FULL != 0 && status & STATUS_FROM_MOUSE != 0 {
                return Ok(inb(DATA));
            }
        }
        core::hint::spin_loop();
    }
    Err("no mouse")
}

fn on_interrupt(_frame: &mut InterruptFrame) {
    // SAFETY: as in `read_mouse_byte`.
    unsafe {
        let status = inb(STATUS);
        if status & STATUS_OUTPUT_FULL != 0 && status & STATUS_FROM_MOUSE != 0 {
            BYTES.push(inb(DATA));
        }
    }
    INPUT.wake_all();
}

/// Assembles packets from the mouse's bytes and reports them.
fn decode_forever(device: &Arc<input::Device>, packet_size: usize) -> ! {
    let mut decoder = Decoder { packet: [0; 4], len: 0, packet_size, buttons: 0 };
    loop {
        let byte = INPUT.wait_until(|| BYTES.pop());
        let events = decoder.feed(byte);
        if !events.is_empty() {
            device.report(&events);
        }
    }
}

struct Decoder {
    packet: [u8; 4],
    len: usize,
    packet_size: usize,
    buttons: u8,
}

impl Decoder {
    /// Takes one byte; once a packet is complete, returns its events.
    fn feed(&mut self, byte: u8) -> Vec<(u16, u16, i32)> {
        // Bit 3 of a packet's first byte is always set; anything else
        // means a byte was lost, so wait for the next likely start.
        if self.len == 0 && byte & 0x08 == 0 {
            return Vec::new();
        }
        self.packet[self.len] = byte;
        self.len += 1;
        if self.len < self.packet_size {
            return Vec::new();
        }
        self.len = 0;

        let [flags, x, y, z] = self.packet;
        let mut events = Vec::new();
        let buttons = flags & 0x07;
        for (bit, code) in [(0, BTN_LEFT), (1, BTN_RIGHT), (2, BTN_MIDDLE)] {
            if (buttons ^ self.buttons) & 1 << bit != 0 {
                events.push((EV_KEY, code, i32::from(buttons >> bit & 1)));
            }
        }
        self.buttons = buttons;
        // Movement is 9-bit two's complement, the sign bits in the first
        // byte; overflowed movement is meaningless and dropped.
        if flags & 0xC0 == 0 {
            let dx = i32::from(x) - if flags & 0x10 != 0 { 256 } else { 0 };
            let dy = i32::from(y) - if flags & 0x20 != 0 { 256 } else { 0 };
            if dx != 0 {
                events.push((EV_REL, REL_X, dx));
            }
            // PS/2 counts up as positive; Linux counts down.
            if dy != 0 {
                events.push((EV_REL, REL_Y, -dy));
            }
        }
        // The wheel: negative is up (away from you), which Linux counts as
        // positive.
        if self.packet_size == 4 && z != 0 {
            events.push((EV_REL, REL_WHEEL, -i32::from(z as i8)));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn packets_become_events() {
            let mut decoder = Decoder { packet: [0; 4], len: 0, packet_size: 4, buttons: 0 };
            // A stray byte (bit 3 clear) is skipped. Then: left button, 5
            // right, 3 up, wheel one step up.
            assert!(decoder.feed(0x00).is_empty());
            assert!(decoder.feed(0x09).is_empty());
            assert!(decoder.feed(5).is_empty());
            assert!(decoder.feed(3).is_empty());
            let events = decoder.feed(0xFF);
            assert_eq!(events, [(EV_KEY, BTN_LEFT, 1), (EV_REL, REL_X, 5), (EV_REL, REL_Y, -3), (EV_REL, REL_WHEEL, 1)]);
            // Release, 2 left (sign bit), 1 down.
            for byte in [0x38, 0xFE, 0xFF] {
                assert!(decoder.feed(byte).is_empty());
            }
            assert_eq!(decoder.feed(0), [(EV_KEY, BTN_LEFT, 0), (EV_REL, REL_X, -2), (EV_REL, REL_Y, 1)]);
        }

        fn a_mouse_is_found_and_its_packets_arrive_by_interrupt() {
            // QEMU's PC has a PS/2 mouse (with a wheel); the controller can
            // also inject bytes as if the mouse sent them.
            let device = crate::input::find("PS/2 Generic Mouse").expect("no PS/2 mouse");
            let reader = crate::fs::Inode::open_instance(&*device, 0).unwrap().unwrap();
            for byte in [0x08, 7, 0, 0] {
                // One byte at a time: the controller holds only one.
                let start = time::ticks();
                // SAFETY: reading the status port has no side effects.
                while unsafe { inb(STATUS) } & STATUS_OUTPUT_FULL != 0 && time::ticks() < start + 100 {
                    task::sleep_ms(1);
                }
                send_command(WRITE_SECOND_PORT_OUTPUT).unwrap();
                write_data(byte).unwrap();
            }
            let deadline = time::ticks() + 1000;
            let mut buffer = [0u8; 48];
            let count = loop {
                match reader.read_stream(&mut buffer, true) {
                    Ok(count) => break count,
                    Err(_) if time::ticks() < deadline => task::sleep_ms(5),
                    Err(error) => panic!("no events from the mouse: {error:?}"),
                }
            };
            assert_eq!(count, 48);
            // REL_X 7, then SYN_REPORT.
            assert_eq!(&buffer[16..24], &[2, 0, 0, 0, 7, 0, 0, 0]);
        }
    }
}
