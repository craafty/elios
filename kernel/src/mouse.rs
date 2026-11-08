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
//! several fingers). A Synaptics touchpad is switched to its absolute mode
//! (`synaptics.rs`), which is what lets a ClickPad click and two fingers
//! scroll; `mouse=basic` on the kernel command line keeps the basic
//! protocol instead, in case some touchpad misbehaves in absolute mode.
//!
//! Losing a byte would shift every later packet by one, turning movement
//! into button presses and wild jumps, so the decoder resynchronizes: a
//! packet's bytes arrive within a millisecond or two of each other, so a
//! partial packet followed by a pause is thrown away, as is any packet
//! whose first byte doesn't look like one. `/proc/mouse` counts all this,
//! for checking a laptop's touchpad from its boot log.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::interrupts::{self, IRQ_BASE, InterruptFrame};
use crate::arch::{apic, inb};
use crate::cmdline;
use crate::input::{self, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, REL_WHEEL, REL_X, REL_Y};
use crate::keyboard::{READ_CONFIG, STATUS_FROM_MOUSE, STATUS_OUTPUT_FULL, WRITE_CONFIG, send_command, write_data};
use crate::task::{self, WaitQueue};

mod synaptics;
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
const SET_RESOLUTION: u8 = 0xE8;
const STATUS_REQUEST: u8 = 0xE9;
const GET_ID: u8 = 0xF2;
const ACK: u8 = 0xFA;
const SELF_TEST_PASSED: u8 = 0xAA;

const MOUSE_IRQ: u8 = 12;
const MOUSE_VECTOR: u8 = IRQ_BASE + MOUSE_IRQ;

/// Reports per second; 100 is the most every PS/2 mouse supports.
const SAMPLE_RATE: u8 = 100;
/// Resolution 3: 8 counts per millimetre, the finest standard one (the
/// default after a reset is 4 per millimetre, which makes slow, precise
/// movements jumpy once accelerated).
const RESOLUTION: u8 = 3;
/// A pause this long inside a packet means a byte was lost.
const PACKET_GAP_NANOS: u64 = 30_000_000;

static BYTES: TimedBytes = TimedBytes::new();
static INPUT: WaitQueue = WaitQueue::new();

/// What `/proc/mouse` reports.
static INFO: crate::sync::SpinLock<String> = crate::sync::SpinLock::new(String::new());
static PACKETS: AtomicU64 = AtomicU64::new(0);
static RESYNCS: AtomicU64 = AtomicU64::new(0);
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The bytes from the interrupt handler to the decoding thread, each with
/// the time it arrived (the thread may run much later), packed as
/// `nanos << 8 | byte`. One producer (the interrupt), one consumer.
struct TimedBytes {
    entries: [AtomicU64; 256],
    pushed: AtomicUsize,
    popped: AtomicUsize,
}

impl TimedBytes {
    const fn new() -> Self {
        Self { entries: [const { AtomicU64::new(0) }; 256], pushed: AtomicUsize::new(0), popped: AtomicUsize::new(0) }
    }

    fn push(&self, byte: u8, nanos: u64) -> bool {
        let pushed = self.pushed.load(Ordering::Relaxed);
        if pushed.wrapping_sub(self.popped.load(Ordering::Acquire)) == self.entries.len() {
            return false;
        }
        self.entries[pushed % self.entries.len()].store(nanos << 8 | u64::from(byte), Ordering::Relaxed);
        self.pushed.store(pushed.wrapping_add(1), Ordering::Release);
        true
    }

    fn pop(&self) -> Option<(u8, u64)> {
        let popped = self.popped.load(Ordering::Relaxed);
        if popped == self.pushed.load(Ordering::Acquire) {
            return None;
        }
        let entry = self.entries[popped % self.entries.len()].load(Ordering::Relaxed);
        self.popped.store(popped.wrapping_add(1), Ordering::Release);
        Some((entry as u8, entry >> 8))
    }
}

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
    let synaptics = synaptics_identity();
    if let Some([minor, major_model]) = synaptics
        && cmdline::get("mouse") != Some("basic")
    {
        match synaptics::start(major_model & 0x0F) {
            Ok((touchpad, description)) => return start_touchpad(touchpad, [minor, major_model], description),
            Err(error) => {
                warn!("mouse: Synaptics absolute mode: {error}; using the basic protocol");
                command(RESET)?;
                let _ = read_mouse_byte(1000);
                let _ = read_mouse_byte(100);
                command(SET_DEFAULTS)?;
            }
        }
    }
    for rate in [200, 100, 80] {
        command(SET_SAMPLE_RATE)?;
        command(rate)?;
    }
    command(GET_ID)?;
    let id = read_mouse_byte(100)?;
    let wheel = id == 3;
    command(SET_SAMPLE_RATE)?;
    command(SAMPLE_RATE)?;
    command(SET_RESOLUTION)?;
    command(RESOLUTION)?;
    let status = status_bytes();

    let mut info = alloc::format!("id: {id}\npacket bytes: {}\n", if wheel { 4 } else { 3 });
    if let Some([minor, major_model]) = synaptics {
        info += &alloc::format!("synaptics: version {}.{minor}, model {:#x}\n", major_model & 0x0F, major_model >> 4);
    }
    if let Some([flags, resolution, rate]) = status {
        info += &alloc::format!("status: flags {flags:#04x}, resolution {resolution}, rate {rate}\n");
    }
    info!("mouse: {}", info.trim_end().replace('\n', ", "));
    *INFO.lock() = info;
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

    let mut decoder = Decoder::new(if wheel { 4 } else { 3 });
    start_decoding(device, move |byte, nanos| decoder.feed(byte, nanos))
}

/// Registers a Synaptics touchpad in absolute mode (the TrackPoint's
/// packets come through it) and starts decoding its packets.
fn start_touchpad(touchpad: synaptics::Touchpad, identity: [u8; 2], description: String) -> Result<(), &'static str> {
    let [minor, major_model] = identity;
    let info = alloc::format!(
        "id: synaptics absolute
packet bytes: 6
synaptics: version {}.{minor}, model {:#x}
{description}",
        major_model & 0x0F,
        major_model >> 4
    );
    info!("mouse: {}", info.trim_end().replace('\n', ", "));
    *INFO.lock() = info;
    command(ENABLE_REPORTING)?;
    let capabilities = input::Capabilities {
        keys: alloc::vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
        relative: alloc::vec![REL_X, REL_Y, REL_WHEEL, input::REL_HWHEEL],
        leds: Vec::new(),
        properties: input::PROP_POINTER,
    };
    // Programs see a mouse: the pad's positions arrive as movement.
    let device = input::register("SynPS/2 Synaptics TouchPad", [input::BUS_I8042, 2, 7, 0], capabilities);
    let mut decoder = synaptics::Decoder::new(touchpad);
    start_decoding(device, move |byte, nanos| decoder.feed(byte, nanos))
}

/// Takes the mouse's interrupt and starts the thread that turns its bytes
/// into events with `decode`.
fn start_decoding(
    device: Arc<input::Device>,
    mut decode: impl FnMut(u8, u64) -> Vec<(u16, u16, i32)> + Send + 'static,
) -> Result<(), &'static str> {
    interrupts::set_handler(MOUSE_VECTOR, on_interrupt);
    apic::route_isa_irq(MOUSE_IRQ, MOUSE_VECTOR)?;
    set_config(|config| config | CONFIG_SECOND_PORT_INTERRUPT)?;
    task::spawn("mouse", move || {
        loop {
            let (byte, nanos) = INPUT.wait_until(|| BYTES.pop());
            let events = decode(byte, nanos);
            if !events.is_empty() {
                device.report(&events);
            }
        }
    });
    Ok(())
}

/// Changes the controller's configuration byte. The mouse may already be
/// sending (reporting is on before its interrupt is), so its bytes are
/// set aside for the decoder rather than taken for the controller's answer.
fn set_config(change: impl FnOnce(u8) -> u8) -> Result<(), &'static str> {
    send_command(READ_CONFIG)?;
    let deadline = time::uptime_nanos() + 100_000_000;
    let config = loop {
        if time::uptime_nanos() > deadline {
            return Err("the PS/2 controller didn't answer");
        }
        // SAFETY: as in `read_mouse_byte`; the status read says whose
        // byte the data port holds.
        let status = unsafe { inb(STATUS) };
        if status & STATUS_OUTPUT_FULL == 0 {
            core::hint::spin_loop();
            continue;
        }
        // SAFETY: as above.
        let byte = unsafe { inb(DATA) };
        if status & STATUS_FROM_MOUSE == 0 {
            break byte;
        }
        BYTES.push(byte, time::uptime_nanos());
    };
    send_command(WRITE_CONFIG)?;
    write_data(change(config))
}

/// Asks for a Synaptics touchpad's identity (its "special command"
/// encoding: resolution settings 0, 0, 0, 0 spell the argument, then a
/// status request). Returns the minor version and the major version and
/// model nibbles; `None` for anything else (whose middle byte isn't 0x47).
fn synaptics_identity() -> Option<[u8; 2]> {
    for _ in 0..4 {
        command(SET_RESOLUTION).ok()?;
        command(0).ok()?;
    }
    let [minor, magic, major_model] = status_bytes()?;
    (magic == 0x47).then_some([minor, major_model])
}

/// A status request: flags (mode and buttons), resolution, sample rate.
fn status_bytes() -> Option<[u8; 3]> {
    command(STATUS_REQUEST).ok()?;
    let mut bytes = [0; 3];
    for byte in &mut bytes {
        *byte = read_mouse_byte(100).ok()?;
    }
    Some(bytes)
}

/// `/proc/mouse`: what the mouse said at start-up and the decoder's counts.
pub fn status_text() -> String {
    let mut text = INFO.lock().clone();
    for (name, count) in
        [("packets", &PACKETS), ("resyncs", &RESYNCS), ("overflows", &OVERFLOWS), ("dropped", &DROPPED)]
    {
        text += &alloc::format!("{name}: {}\n", count.load(Ordering::Relaxed));
    }
    for (name, count) in [
        ("touch packets", &synaptics::TOUCH_PACKETS),
        ("trackpoint packets", &synaptics::STICK_PACKETS),
        ("button packets", &synaptics::BUTTON_PACKETS),
        ("pad clicks", &synaptics::PAD_CLICKS),
    ] {
        text += &alloc::format!("{name}: {}\n", count.load(Ordering::Relaxed));
    }
    text
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
            let byte = inb(DATA);
            if !BYTES.push(byte, time::uptime_nanos()) {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    INPUT.wake_all();
}

struct Decoder {
    packet: [u8; 4],
    len: usize,
    packet_size: usize,
    buttons: u8,
    /// When the previous byte arrived.
    last_nanos: u64,
}

impl Decoder {
    fn new(packet_size: usize) -> Self {
        Self { packet: [0; 4], len: 0, packet_size, buttons: 0, last_nanos: 0 }
    }

    /// Takes one byte (and when it arrived); once a packet is complete,
    /// returns its events.
    fn feed(&mut self, byte: u8, nanos: u64) -> Vec<(u16, u16, i32)> {
        let gap = nanos.saturating_sub(self.last_nanos);
        self.last_nanos = nanos;
        if self.len > 0 && gap > PACKET_GAP_NANOS {
            // The rest of that packet was lost: this byte starts a new one.
            RESYNCS.fetch_add(1, Ordering::Relaxed);
            self.len = 0;
        }
        // Bit 3 of a packet's first byte is always set; anything else
        // means a byte was lost, so wait for the next likely start.
        if self.len == 0 && byte & 0x08 == 0 {
            RESYNCS.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        }
        self.packet[self.len] = byte;
        self.len += 1;
        if self.len < self.packet_size {
            return Vec::new();
        }
        self.len = 0;
        PACKETS.fetch_add(1, Ordering::Relaxed);

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
        if flags & 0xC0 != 0 {
            OVERFLOWS.fetch_add(1, Ordering::Relaxed);
        } else {
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
            let mut decoder = Decoder::new(4);
            // A stray byte (bit 3 clear) is skipped. Then: left button, 5
            // right, 3 up, wheel one step up.
            assert!(decoder.feed(0x00, 0).is_empty());
            assert!(decoder.feed(0x09, 0).is_empty());
            assert!(decoder.feed(5, 0).is_empty());
            assert!(decoder.feed(3, 0).is_empty());
            let events = decoder.feed(0xFF, 0);
            assert_eq!(events, [(EV_KEY, BTN_LEFT, 1), (EV_REL, REL_X, 5), (EV_REL, REL_Y, -3), (EV_REL, REL_WHEEL, 1)]);
            // Release, 2 left (sign bit), 1 down.
            for byte in [0x38, 0xFE, 0xFF] {
                assert!(decoder.feed(byte, 0).is_empty());
            }
            assert_eq!(decoder.feed(0, 0), [(EV_KEY, BTN_LEFT, 0), (EV_REL, REL_X, -2), (EV_REL, REL_Y, 1)]);
        }

        fn a_pause_inside_a_packet_resynchronizes() {
            let mut decoder = Decoder::new(3);
            // Half a packet, then (after 50 ms) a whole one: the half is
            // dropped rather than merged into a shifted packet.
            assert!(decoder.feed(0x08, 1_000_000).is_empty());
            assert!(decoder.feed(0x40, 2_000_000).is_empty());
            let ms = 1_000_000;
            assert!(decoder.feed(0x08, 52 * ms).is_empty());
            assert!(decoder.feed(4, 53 * ms).is_empty());
            assert_eq!(decoder.feed(0, 53 * ms), [(EV_REL, REL_X, 4)]);
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
