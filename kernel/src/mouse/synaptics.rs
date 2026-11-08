//! Synaptics touchpads in absolute mode. In the basic mouse protocol a
//! touchpad reports only movement and its buttons; a ClickPad (a pad that
//! is itself the button, like the ThinkPad T440p's) then can't click at
//! all, and two-finger scrolling is impossible. Absolute mode reports where
//! the finger is, how hard it presses, how many fingers there are, and the
//! pad's own button; this module turns that back into what the rest of the
//! system expects from a mouse: relative movement (scaled to the basic
//! protocol's 8 counts per millimetre, so the desktop's acceleration feels
//! the same), buttons and wheel steps.
//!
//! The protocol is Synaptics' "PS/2 TouchPad Interfacing Guide": queries
//! and the mode byte are sent as a "special command" (an argument spelled
//! out two bits at a time in four set-resolution commands), then a status
//! request (a query, answered in three bytes) or a set-sample-rate 20 (set
//! the mode). In absolute mode with W mode on, every packet is six bytes:
//!
//! ```text
//! byte 0: 1 0 W3 W2 0 W1 R L        byte 3: 1 1 Y12 X12 0 W0 R^ L^
//! byte 1: Y11..Y8 X11..X8           byte 4: X7..X0
//! byte 2: Z (pressure)              byte 5: Y7..Y0
//! ```
//!
//! W says what touched: 0 two fingers, 1 three or more, 4 and up one
//! finger (its width), and 3 a packet passed through from the TrackPoint
//! (an ordinary 3-byte mouse packet in bytes 1, 4 and 5). A ClickPad's
//! button is the XOR of the two L bits. Touchpads with extra buttons wired
//! to them (the TrackPoint's buttons on Lenovo's 2015-era pads) report
//! those in the low bits of bytes 4 and 5 of packets whose two R bits
//! differ; firmware 8.1 and 8.2 report them only in those packets, so the
//! last state is kept in between.
//!
//! Nothing here can be tried in QEMU (its mouse isn't a Synaptics), so the
//! start-up logs everything the touchpad says about itself, and the first
//! packets that change a button are logged raw.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{SET_RESOLUTION, SET_SAMPLE_RATE, command, status_bytes};
use crate::input::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y};

// Queries.
const QUERY_MODES: u8 = 0x01;
const QUERY_CAPABILITIES: u8 = 0x02;
const QUERY_MODEL: u8 = 0x03;
const QUERY_RESOLUTION: u8 = 0x08;
const QUERY_EXTENDED_CAPABILITIES: u8 = 0x09;
const QUERY_CAPABILITIES_0C: u8 = 0x0C;
const QUERY_CAPABILITIES_10: u8 = 0x10;

/// Setting the sample rate to this after a special command sets the mode.
const SET_MODE_RATE: u8 = 0x14;

// Mode bits.
const MODE_ABSOLUTE: u8 = 1 << 7;
/// 80 packets a second instead of 40.
const MODE_HIGH_RATE: u8 = 1 << 6;
/// No tap-to-click from the firmware (it doesn't do that in absolute mode
/// anyway; this keeps it from holding packets back to look for taps).
const MODE_DISABLE_GESTURES: u8 = 1 << 2;
const MODE_W: u8 = 1 << 0;

// Capability bits.
const CAP_EXTENDED: u32 = 1 << 23;
const CAP_MIDDLE_BUTTON: u32 = 1 << 18;
const CAP_PASS_THROUGH: u32 = 1 << 7;
const CAP_MULTI_FINGER: u32 = 1 << 1;
const MODEL_NEW_ABSOLUTE: u32 = 1 << 7;
const CAP_0C_CLICKPAD: u32 = 1 << 20;
const CAP_0C_CLICKPAD_2_BUTTONS: u32 = 1 << 8;
const CAP_10_STICK_BUTTONS: u32 = 1 << 16;

/// Pressure from which a finger counts as touching (libinput's default
/// for Synaptics pads; resting fingers hover around 10-20).
const TOUCH_PRESSURE: u8 = 30;
/// Movement the basic protocol reports per millimetre (resolution 3).
const COUNTS_PER_MM: i32 = 8;
/// Units per millimetre if the touchpad doesn't say (typical of Lenovo's).
const DEFAULT_UNITS_PER_MM: i32 = 42;
/// Two fingers moving this many millimetres scroll one wheel step.
const SCROLL_STEP_TENTHS_MM: i32 = 25;
/// A finger "moving" more than this between packets (12 ms apart) is a
/// glitch, such as a second finger landing, not movement.
const MAX_JUMP_MM: i32 = 15;
/// Packets without movement after the pad's button goes down or up: the
/// finger rolls as it presses, which would move the pointer off its target.
const CLICK_SETTLE_PACKETS: u8 = 4;
/// How many button-changing packets are logged raw.
const LOGGED_PACKETS: u64 = 64;

pub static TOUCH_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static STICK_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static BUTTON_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static PAD_CLICKS: AtomicU64 = AtomicU64::new(0);
static LOGGED: AtomicU64 = AtomicU64::new(0);

/// What the touchpad says it can do.
#[derive(Clone, Copy)]
pub struct Touchpad {
    capabilities: u32,
    extended: u32,
    capabilities_0c: u32,
    capabilities_10: u32,
    /// Units per millimetre.
    x_resolution: i32,
    y_resolution: i32,
}

impl Touchpad {
    fn clickpad(&self) -> bool {
        self.capabilities_0c & (CAP_0C_CLICKPAD | CAP_0C_CLICKPAD_2_BUTTONS) != 0
    }

    fn w_mode(&self) -> bool {
        self.capabilities & CAP_EXTENDED != 0
    }

    /// How many extra buttons are wired to the touchpad.
    fn extra_buttons(&self) -> u32 {
        (self.extended >> 12) & 0x0F
    }
}

/// Sends a special command: `argument` two bits at a time.
fn special(argument: u8) -> Result<(), &'static str> {
    for shift in [6, 4, 2, 0] {
        command(SET_RESOLUTION)?;
        command(argument >> shift & 3)?;
    }
    Ok(())
}

/// A query's three answer bytes, as one number (first byte highest).
fn query(number: u8) -> Option<u32> {
    special(number).ok()?;
    let [a, b, c] = status_bytes()?;
    Some(u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c))
}

/// Asks the touchpad what it is and switches it to absolute mode.
/// `major` is the major version from its identity. Returns the touchpad and
/// a description for `/proc/mouse`.
pub fn start(major: u8) -> Result<(Touchpad, String), &'static str> {
    if major < 4 {
        return Err("touchpad too old for absolute mode");
    }
    let capabilities = query(QUERY_CAPABILITIES).ok_or("no capabilities")?;
    let model = query(QUERY_MODEL).ok_or("no model")?;
    if model & MODEL_NEW_ABSOLUTE == 0 {
        return Err("touchpad without the 6-byte absolute format");
    }
    let extended_queries = if capabilities & CAP_EXTENDED != 0 { (capabilities >> 20) & 7 } else { 0 };
    let extended = if extended_queries >= 1 { query(QUERY_EXTENDED_CAPABILITIES).unwrap_or(0) } else { 0 };
    let capabilities_0c = if extended_queries >= 4 { query(QUERY_CAPABILITIES_0C).unwrap_or(0) } else { 0 };
    let capabilities_10 = if extended_queries >= 5 { query(QUERY_CAPABILITIES_10).unwrap_or(0) } else { 0 };
    let resolution = query(QUERY_RESOLUTION).unwrap_or(0);
    let (x_resolution, y_resolution) = match resolution.to_be_bytes() {
        [_, x, flags, y] if x != 0 && y != 0 && flags & 0x80 != 0 => (i32::from(x), i32::from(y)),
        _ => (DEFAULT_UNITS_PER_MM, DEFAULT_UNITS_PER_MM),
    };
    let touchpad = Touchpad { capabilities, extended, capabilities_0c, capabilities_10, x_resolution, y_resolution };

    let mut mode = MODE_ABSOLUTE | MODE_HIGH_RATE | MODE_DISABLE_GESTURES;
    if touchpad.w_mode() {
        mode |= MODE_W;
    }
    special(mode)?;
    command(SET_SAMPLE_RATE)?;
    command(SET_MODE_RATE)?;
    let modes = query(QUERY_MODES).unwrap_or(0);

    let text = alloc::format!(
        "synaptics: capabilities {capabilities:#08x}, model {model:#08x}, extended {extended:#08x}, \
         0c {capabilities_0c:#08x}, 10 {capabilities_10:#08x}, resolution {resolution:#08x} \
         ({x_resolution}x{y_resolution} per mm)\n\
         synaptics: mode {mode:#04x} (reads back {modes:#08x}); clickpad {}, extra buttons {}, \
         stick buttons {}, pass-through {}, two fingers {}\n",
        touchpad.clickpad(),
        touchpad.extra_buttons(),
        capabilities_10 & CAP_10_STICK_BUTTONS != 0,
        capabilities & CAP_PASS_THROUGH != 0,
        capabilities & CAP_MULTI_FINGER != 0,
    );
    Ok((touchpad, text))
}

/// Assembles six-byte packets and turns them into mouse events.
pub struct Decoder {
    touchpad: Touchpad,
    packet: [u8; 6],
    len: usize,
    last_nanos: u64,
    /// Buttons as last reported: left, right, middle in bits 0-2.
    buttons: u8,
    /// The pad's own button, and which button it was reported as.
    pad_button: Option<u16>,
    /// Buttons wired to the touchpad (kept between packets: see above).
    extra: u8,
    /// Buttons from TrackPoint packets.
    stick: u8,
    /// Where the finger was, and how many fingers, in the last packet.
    last: Option<(i32, i32, u8)>,
    /// Fractions of counts and of scroll steps carried to the next packet.
    remainder: [i32; 2],
    scroll: [i32; 2],
    settle: u8,
}

impl Decoder {
    pub fn new(touchpad: Touchpad) -> Self {
        Self {
            touchpad,
            packet: [0; 6],
            len: 0,
            last_nanos: 0,
            buttons: 0,
            pad_button: None,
            extra: 0,
            stick: 0,
            last: None,
            remainder: [0; 2],
            scroll: [0; 2],
            settle: 0,
        }
    }

    /// Takes one byte (and when it arrived); once a packet is complete,
    /// returns its events.
    pub fn feed(&mut self, byte: u8, nanos: u64) -> Vec<(u16, u16, i32)> {
        let gap = nanos.saturating_sub(self.last_nanos);
        self.last_nanos = nanos;
        if self.len > 0 && gap > super::PACKET_GAP_NANOS {
            super::RESYNCS.fetch_add(1, Ordering::Relaxed);
            self.len = 0;
        }
        // Bytes 0 and 3 have fixed bits; anything else means a byte was
        // lost, so start again at the next likely first byte.
        let valid = match self.len {
            0 => byte & 0xC8 == 0x80,
            3 => byte & 0xC8 == 0xC0,
            _ => true,
        };
        if !valid {
            super::RESYNCS.fetch_add(1, Ordering::Relaxed);
            self.len = 0;
            return Vec::new();
        }
        self.packet[self.len] = byte;
        self.len += 1;
        if self.len < 6 {
            return Vec::new();
        }
        self.len = 0;
        super::PACKETS.fetch_add(1, Ordering::Relaxed);
        self.decode()
    }

    fn decode(&mut self) -> Vec<(u16, u16, i32)> {
        let p = self.packet;
        let w = if self.touchpad.w_mode() { (p[0] & 0x30) >> 2 | (p[0] & 0x04) >> 1 | (p[3] & 0x04) >> 2 } else { 4 };
        let mut events = Vec::new();
        let mut motion = None;
        if w == 3 {
            // A TrackPoint packet: buttons, sign bits, then X and Y.
            STICK_PACKETS.fetch_add(1, Ordering::Relaxed);
            let [flags, x, y] = [p[1], p[4], p[5]];
            // With its buttons wired to the touchpad, the TrackPoint's
            // own button bits aren't meaningful.
            if self.touchpad.capabilities_10 & CAP_10_STICK_BUTTONS == 0 {
                self.stick = flags & 0x07;
            }
            if flags & 0xC0 == 0 {
                let dx = i32::from(x) - if flags & 0x10 != 0 { 256 } else { 0 };
                let dy = i32::from(y) - if flags & 0x20 != 0 { 256 } else { 0 };
                motion = Some((dx, -dy));
            }
        }

        // The pad's button: a right click when two fingers press it. Found
        // before the movement, so that the press packet itself settles.
        let pad = if self.touchpad.clickpad() { (p[0] ^ p[3]) & 1 != 0 } else { false };
        if w != 3 && pad != self.pad_button.is_some() {
            self.settle = CLICK_SETTLE_PACKETS;
            if pad {
                PAD_CLICKS.fetch_add(1, Ordering::Relaxed);
                let two_fingers = w == 0 && self.touchpad.capabilities & CAP_MULTI_FINGER != 0;
                self.pad_button = Some(if two_fingers { BTN_RIGHT } else { BTN_LEFT });
            } else {
                self.pad_button = None;
            }
        }
        if w != 3 {
            TOUCH_PACKETS.fetch_add(1, Ordering::Relaxed);
            motion = self.touch(w, &mut events);
        }

        let mut buttons = self.stick | self.extra;
        if w != 3 {
            buttons |= p[0] & 0x02; // right
            if !self.touchpad.clickpad() {
                buttons |= p[0] & 0x01;
                if self.touchpad.capabilities & CAP_MIDDLE_BUTTON != 0 && (p[0] ^ p[3]) & 1 != 0 {
                    buttons |= 0x04;
                }
            }
        }
        match self.pad_button {
            Some(BTN_RIGHT) => buttons |= 0x02,
            Some(_) => buttons |= 0x01,
            None => {}
        }
        if buttons != self.buttons && LOGGED.fetch_add(1, Ordering::Relaxed) < LOGGED_PACKETS {
            debug!("mouse: packet {p:02x?} (W {w}): buttons {:#x} -> {buttons:#x}", self.buttons);
        }
        for (bit, code) in [(0, BTN_LEFT), (1, BTN_RIGHT), (2, BTN_MIDDLE)] {
            if (buttons ^ self.buttons) & 1 << bit != 0 {
                events.push((EV_KEY, code, i32::from(buttons >> bit & 1)));
            }
        }
        self.buttons = buttons;
        if let Some((dx, dy)) = motion {
            if dx != 0 {
                events.push((EV_REL, REL_X, dx));
            }
            if dy != 0 {
                events.push((EV_REL, REL_Y, dy));
            }
        }
        events
    }

    /// A touchpad packet's finger: returns pointer movement, or pushes
    /// scroll steps for two fingers.
    fn touch(&mut self, w: u8, events: &mut Vec<(u16, u16, i32)>) -> Option<(i32, i32)> {
        let p = self.packet;
        // Extra buttons replace the low bits of X and Y in these packets.
        if self.touchpad.extra_buttons() > 0 && (p[0] ^ p[3]) & 0x02 != 0 {
            BUTTON_PACKETS.fetch_add(1, Ordering::Relaxed);
            let bits = self.touchpad.extra_buttons().div_ceil(2);
            let mask = ((1u32 << bits) - 1) as u8;
            let (low, high) = (p[4] & mask, p[5] & mask);
            // Left and middle in byte 4, right in byte 5 (Lenovo's layout
            // for the TrackPoint's buttons).
            self.extra = (low & 1) | (high & 1) << 1 | (low >> 1 & 1) << 2;
            return None;
        }
        let x = i32::from(p[3] & 0x10) << 8 | i32::from(p[1] & 0x0F) << 8 | i32::from(p[4]);
        let y = i32::from(p[3] & 0x20) << 7 | i32::from(p[1] & 0xF0) << 4 | i32::from(p[5]);
        let multi = self.touchpad.capabilities & CAP_MULTI_FINGER != 0;
        let fingers = match w {
            0 if multi => 2,
            1 if multi => 3,
            2 => 0, // a pen or a packet format we didn't ask for
            _ => 1,
        };
        if p[2] < TOUCH_PRESSURE || fingers == 0 {
            self.last = None;
            self.scroll = [0; 2];
            return None;
        }
        let last = self.last.replace((x, y, fingers));
        // A finger landing or the finger count changing moves the reported
        // position (it's between fingers): start afresh.
        let (last_x, last_y, last_fingers) = last?;
        if last_fingers != fingers {
            self.scroll = [0; 2];
            return None;
        }
        let (dx, dy) = (x - last_x, y - last_y);
        let resolution = [self.touchpad.x_resolution, self.touchpad.y_resolution];
        if dx.abs() > MAX_JUMP_MM * resolution[0] || dy.abs() > MAX_JUMP_MM * resolution[1] {
            return None;
        }
        if self.settle > 0 {
            self.settle -= 1;
            return None;
        }
        if fingers >= 2 {
            // Content follows the fingers: moving them up scrolls down (a
            // negative wheel step), moving them right scrolls left.
            for (axis, (code, delta)) in [(REL_HWHEEL, -dx), (REL_WHEEL, -dy)].into_iter().enumerate() {
                let step = SCROLL_STEP_TENTHS_MM * resolution[axis] / 10;
                self.scroll[axis] += delta;
                let steps = self.scroll[axis] / step;
                if steps != 0 {
                    self.scroll[axis] -= steps * step;
                    events.push((EV_REL, code, steps));
                }
            }
            return None;
        }
        // Scale to counts, keeping the fractions. Y counts up on the pad
        // and down on the screen.
        let mut counts = [0; 2];
        for (axis, delta) in [dx, -dy].into_iter().enumerate() {
            let scaled = delta * COUNTS_PER_MM + self.remainder[axis];
            counts[axis] = scaled / resolution[axis];
            self.remainder[axis] = scaled % resolution[axis];
        }
        Some((counts[0], counts[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A T440p-like ClickPad: W mode, two fingers, pass-through, 3 extra
    /// buttons wired for the TrackPoint, 40 units per millimetre.
    fn touchpad() -> Touchpad {
        Touchpad {
            capabilities: CAP_EXTENDED | CAP_PASS_THROUGH | CAP_MULTI_FINGER,
            extended: 3 << 12,
            capabilities_0c: CAP_0C_CLICKPAD,
            capabilities_10: CAP_10_STICK_BUTTONS,
            x_resolution: 40,
            y_resolution: 40,
        }
    }

    /// A touch packet: position, pressure, W, and the pad's button.
    fn touch(x: i32, y: i32, z: u8, w: u8, pressed: bool) -> [u8; 6] {
        let mut p = [0x80, 0, z, 0xC0, x as u8, y as u8];
        p[1] = ((y >> 8 & 0x0F) << 4 | (x >> 8 & 0x0F)) as u8;
        p[3] |= ((x >> 12 & 1) << 4 | (y >> 12 & 1) << 5) as u8;
        p[0] |= (w >> 2 & 3) << 4 | (w >> 1 & 1) << 2;
        p[3] |= (w & 1) << 2;
        if pressed {
            p[3] |= 1; // L^ differs from L
        }
        p
    }

    fn feed(decoder: &mut Decoder, packet: [u8; 6]) -> Vec<(u16, u16, i32)> {
        let mut events = Vec::new();
        for byte in packet {
            events = decoder.feed(byte, 0);
        }
        events
    }

    crate::ktest! {
        fn finger_movement_becomes_counts() {
            let mut decoder = Decoder::new(touchpad());
            // Landing reports nothing; then 5 mm right and 2.5 mm up are 40
            // and 20 counts (up is negative on the screen).
            assert!(feed(&mut decoder, touch(3000, 3000, 60, 6, false)).is_empty());
            assert_eq!(feed(&mut decoder, touch(3200, 3100, 60, 6, false)), [(EV_REL, REL_X, 40), (EV_REL, REL_Y, -20)]);
            // Fractions carry over: 3 units are 0.6 counts each time.
            assert!(feed(&mut decoder, touch(3203, 3100, 60, 6, false)).is_empty());
            assert_eq!(feed(&mut decoder, touch(3206, 3100, 60, 6, false)), [(EV_REL, REL_X, 1)]);
            // Lifting the finger and landing elsewhere doesn't jump.
            assert!(feed(&mut decoder, touch(0, 0, 0, 6, false)).is_empty());
            assert!(feed(&mut decoder, touch(1500, 1500, 60, 6, false)).is_empty());
        }

        fn pressing_the_pad_clicks() {
            let mut decoder = Decoder::new(touchpad());
            feed(&mut decoder, touch(3000, 3000, 60, 6, false));
            // The press: left button, and the rolling finger doesn't move
            // the pointer.
            assert_eq!(feed(&mut decoder, touch(3010, 2990, 90, 6, true)), [(EV_KEY, BTN_LEFT, 1)]);
            assert!(feed(&mut decoder, touch(3020, 2980, 90, 6, true)).is_empty());
            assert_eq!(feed(&mut decoder, touch(3020, 2980, 60, 6, false)), [(EV_KEY, BTN_LEFT, 0)]);
            // With two fingers down it's a right click.
            feed(&mut decoder, touch(3000, 3000, 60, 0, false));
            assert_eq!(feed(&mut decoder, touch(3000, 3000, 60, 0, true)), [(EV_KEY, BTN_RIGHT, 1)]);
            assert_eq!(feed(&mut decoder, touch(3000, 3000, 60, 0, false)), [(EV_KEY, BTN_RIGHT, 0)]);
        }

        fn two_fingers_scroll() {
            let mut decoder = Decoder::new(touchpad());
            feed(&mut decoder, touch(3000, 3000, 60, 0, false));
            // 2.5 mm (100 units) up is one step down; less is kept.
            assert!(feed(&mut decoder, touch(3000, 3060, 60, 0, false)).is_empty());
            assert_eq!(feed(&mut decoder, touch(3000, 3100, 60, 0, false)), [(EV_REL, REL_WHEEL, -1)]);
        }

        fn trackpoint_packets_and_wired_buttons() {
            let mut decoder = Decoder::new(touchpad());
            // A TrackPoint packet (W 3): 3 right, 2 down.
            let w3 = [0x80 | 0x04, 0x08 | 0x20, 0, 0xC0 | 0x04, 3, 0xFE];
            assert_eq!(feed(&mut decoder, w3), [(EV_REL, REL_X, 3), (EV_REL, REL_Y, 2)]);
            // Its left button, reported in a touchpad packet whose R bits
            // differ (low bit of byte 4), stays down until a packet like
            // that says otherwise.
            let button = |low: u8, high: u8| [0x80, 0, 0, 0xC2, low, high];
            assert_eq!(feed(&mut decoder, button(1, 0)), [(EV_KEY, BTN_LEFT, 1)]);
            assert!(feed(&mut decoder, touch(0, 0, 0, 6, false)).is_empty());
            assert_eq!(feed(&mut decoder, button(0, 1)), [(EV_KEY, BTN_LEFT, 0), (EV_KEY, BTN_RIGHT, 1)]);
            assert_eq!(feed(&mut decoder, button(0, 0)), [(EV_KEY, BTN_RIGHT, 0)]);
        }
    }
}
