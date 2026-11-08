//! Bluetooth keyboards and mice: the host side of the HID profile.
//!
//! A Bluetooth HID device talks over two L2CAP channels: control (PSM
//! 0x11: requests and their handshakes) and interrupt (PSM 0x13: input
//! reports from the device, output reports such as keyboard LEDs to it).
//! Each message starts with a byte of transaction type and parameter.
//!
//! As for USB (`drivers/usb/hid.rs`, whose report decoding is reused),
//! the host asks for the boot protocol, which every keyboard and mouse
//! supports, so reports needn't be described by a report descriptor
//! (which would take an SDP query to fetch). Boot reports over Bluetooth
//! carry a report ID: 1 for a keyboard, 2 for a mouse.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::usb::hid::{BootKeyboard, BootMouse};
use crate::input;
use crate::keyboard::Key;
use crate::time;

/// The PSMs of HID's channels.
pub(super) const CONTROL: u16 = 0x11;
pub(super) const INTERRUPT: u16 = 0x13;

// Transaction types (high nibble) and parameters.
const HANDSHAKE: u8 = 0x00;
const HID_CONTROL: u8 = 0x10;
const VIRTUAL_CABLE_UNPLUG: u8 = 0x05;
const SET_PROTOCOL_BOOT: u8 = 0x70;
const DATA_INPUT: u8 = 0xA1;
const DATA_OUTPUT: u8 = 0xA2;

const KEYBOARD_REPORT: u8 = 1;
const MOUSE_REPORT: u8 = 2;

/// Classes of device: the peripheral major class, and its minor class
/// bits for a keyboard and a pointing device.
const MAJOR_PERIPHERAL: u32 = 0x05;
const MINOR_KEYBOARD: u32 = 0x40;
const MINOR_POINTING: u32 = 0x80;

/// Whether a class of device says it's a HID device (or says nothing).
pub(super) fn is_hid(class: u32) -> bool {
    class == 0 || (class >> 8 & 0x1F) == MAJOR_PERIPHERAL
}

/// What the stack should do after a control message.
pub(super) enum Control {
    Nothing,
    /// The device asked to be forgotten.
    Unplugged,
}

pub(super) struct Hid {
    name: String,
    keyboard: Option<(BootKeyboard, Arc<input::Device>)>,
    mouse: Option<(BootMouse, Arc<input::Device>)>,
    pub reports: u64,
}

impl Hid {
    /// A HID connection to device `name`, with input devices for what
    /// its class says it is (both if it doesn't say; a combined keyboard
    /// and touchpad says both). Returns the messages to send on the
    /// control and interrupt channels to start it.
    pub fn new(name: &str, class: u32) -> (Self, Vec<u8>, Vec<u8>) {
        let mut hid = Self { name: String::from(name), keyboard: None, mouse: None, reports: 0 };
        let pointing = class & MINOR_POINTING != 0;
        let keyboard = class & MINOR_KEYBOARD != 0;
        if keyboard || !pointing {
            hid.add_keyboard();
        }
        if pointing || !keyboard {
            hid.add_mouse();
        }
        // Boot protocol, and Num Lock on (as for USB keyboards).
        (hid, alloc::vec![SET_PROTOCOL_BOOT], alloc::vec![DATA_OUTPUT, KEYBOARD_REPORT, 0x01])
    }

    fn add_keyboard(&mut self) {
        let capabilities = input::Capabilities {
            keys: input::keyboard_keys(),
            relative: Vec::new(),
            leds: alloc::vec![0, 1, 2],
            properties: 0,
        };
        let device = input::register(&self.name, [input::BUS_BLUETOOTH, 0, 0, 0], capabilities);
        self.keyboard = Some((BootKeyboard::default(), device));
    }

    fn add_mouse(&mut self) {
        let capabilities = input::Capabilities {
            keys: alloc::vec![input::BTN_LEFT, input::BTN_RIGHT, input::BTN_MIDDLE],
            relative: alloc::vec![input::REL_X, input::REL_Y, input::REL_WHEEL],
            leds: Vec::new(),
            properties: input::PROP_POINTER,
        };
        let device = input::register(&self.name, [input::BUS_BLUETOOTH, 0, 0, 0], capabilities);
        self.mouse = Some((BootMouse::default(), device));
    }

    /// What the device has: for `/proc/bluetooth`.
    pub fn kinds(&self) -> &'static str {
        match (self.keyboard.is_some(), self.mouse.is_some()) {
            (true, true) => "keyboard and mouse",
            (true, false) => "keyboard",
            (false, true) => "mouse",
            (false, false) => "no input",
        }
    }

    /// A message on the control channel.
    pub fn control(&mut self, data: &[u8]) -> Control {
        match data {
            [byte, ..] if byte & 0xF0 == HANDSHAKE => {
                if *byte != HANDSHAKE {
                    // Some devices only do the report protocol, whose
                    // keyboard and mouse reports are mostly the boot ones.
                    debug!("bluetooth: {}: handshake {byte:#04x}", self.name);
                }
                Control::Nothing
            }
            [byte, ..] if *byte == HID_CONTROL | VIRTUAL_CABLE_UNPLUG => Control::Unplugged,
            _ => Control::Nothing,
        }
    }

    /// A message on the interrupt channel: input reports. Keys typed go
    /// to `keys` (for the terminal); returns a message for the interrupt
    /// channel if the keyboard's LEDs should change.
    pub fn interrupt(&mut self, data: &[u8], keys: &mut Vec<Key>) -> Option<Vec<u8>> {
        let [DATA_INPUT, id, report @ ..] = data else { return None };
        self.reports += 1;
        let mut events = Vec::new();
        let mut leds = None;
        match *id {
            KEYBOARD_REPORT => {
                if self.keyboard.is_none() {
                    self.add_keyboard();
                }
                let (keyboard, device) = self.keyboard.as_mut().unwrap();
                if keyboard.report(report, time::ticks(), keys, &mut events) {
                    leds = Some(alloc::vec![DATA_OUTPUT, KEYBOARD_REPORT, keyboard.leds()]);
                }
                if !events.is_empty() {
                    device.report(&events);
                }
            }
            MOUSE_REPORT => {
                if self.mouse.is_none() {
                    self.add_mouse();
                }
                let (mouse, device) = self.mouse.as_mut().unwrap();
                mouse.report(report, &mut events);
                if !events.is_empty() {
                    device.report(&events);
                }
            }
            _ => {}
        }
        leds
    }

    /// A held key again, if it's time.
    pub fn repeat(&mut self, now: u64) -> Option<Key> {
        self.keyboard.as_mut()?.0.repeat(now)
    }

    pub fn next_repeat(&self) -> Option<u64> {
        self.keyboard.as_ref()?.0.next_repeat()
    }
}

impl Drop for Hid {
    fn drop(&mut self) {
        let devices = self.keyboard.iter().map(|(_, device)| device).chain(self.mouse.iter().map(|(_, device)| device));
        for device in devices {
            input::unregister(device);
        }
    }
}
