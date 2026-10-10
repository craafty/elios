//! USB: the host controller driver (xHCI), and drivers for the devices on
//! it (keyboards, for now).
//!
//! Every USB device is set up the same way, through its endpoint 0 with
//! "control transfers": a setup packet saying what's wanted, optionally some
//! data, and a status acknowledgment. The device describes itself with
//! descriptors: a device descriptor, then a configuration with interfaces
//! (what the device does: a keyboard, a disk...) and their endpoints (the
//! channels data flows through).

mod hid;
mod xhci;

use core::sync::atomic::{AtomicUsize, Ordering};

use super::pci;

/// How many USB keyboards are working.
static KEYBOARDS: AtomicUsize = AtomicUsize::new(0);
/// How many controllers have looked at the devices plugged in at boot.
static SETTLED: AtomicUsize = AtomicUsize::new(0);

/// Starts drivers for the USB controllers among `devices`. They find
/// devices in the background; this waits (up to two seconds) for them to
/// find the ones plugged in at boot, so a USB keyboard works as soon as
/// the shell starts.
pub fn probe(devices: &[pci::Device]) {
    let controllers = xhci::probe(devices);
    let deadline = crate::time::ticks() + 2000;
    while SETTLED.load(Ordering::Acquire) < controllers && crate::time::ticks() < deadline {
        crate::task::sleep_ms(5);
    }
}

#[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
pub fn keyboard_count() -> usize {
    KEYBOARDS.load(Ordering::Relaxed)
}

/// The 8-byte packet that starts a control transfer.
#[derive(Debug, Clone, Copy)]
pub struct SetupPacket {
    /// Direction (bit 7: device to host), type (bits 5-6: standard, class,
    /// vendor) and recipient (bits 0-4: device, interface, endpoint).
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    /// GET_DESCRIPTOR: descriptor `kind` (1 device, 2 configuration, 3
    /// string) number `index`.
    pub fn get_descriptor(kind: u8, index: u8, length: u16) -> Self {
        Self { request_type: 0x80, request: 6, value: u16::from(kind) << 8 | u16::from(index), index: 0, length }
    }

    /// For string descriptors: which language.
    pub fn language(self, language: u16) -> Self {
        Self { index: language, ..self }
    }

    pub fn set_configuration(configuration: u8) -> Self {
        Self { request_type: 0, request: 9, value: u16::from(configuration), index: 0, length: 0 }
    }

    /// A class-specific request to an interface, sending `length` bytes.
    pub fn class_interface_out(request: u8, value: u16, interface: u8, length: u16) -> Self {
        Self { request_type: 0x21, request, value, index: u16::from(interface), length }
    }

    pub fn as_u64(self) -> u64 {
        u64::from(self.request_type)
            | u64::from(self.request) << 8
            | u64::from(self.value) << 16
            | u64::from(self.index) << 32
            | u64::from(self.length) << 48
    }
}

/// What a configuration descriptor says, as far as drivers here care.
pub struct Descriptors {
    /// The value to select this configuration with.
    pub configuration: u8,
    interfaces: alloc::vec::Vec<Interface>,
}

struct Interface {
    number: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: alloc::vec::Vec<Endpoint>,
}

#[derive(Clone, Copy)]
struct Endpoint {
    address: u8,
    attributes: u8,
    packet_size: u16,
    interval: u8,
}

/// A keyboard interface's interrupt endpoint.
pub struct KeyboardInterface {
    pub interface: u8,
    pub address: u8,
    pub packet_size: u16,
    pub interval: u8,
}

const DESCRIPTOR_CONFIGURATION: u8 = 2;
const DESCRIPTOR_INTERFACE: u8 = 4;
const DESCRIPTOR_ENDPOINT: u8 = 5;

impl Descriptors {
    /// Parses a configuration descriptor and everything after it.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut descriptors = Self { configuration: 1, interfaces: alloc::vec::Vec::new() };
        let mut offset = 0;
        while offset + 2 <= bytes.len() {
            let length = usize::from(bytes[offset]);
            if length < 2 || offset + length > bytes.len() {
                break;
            }
            let d = &bytes[offset..offset + length];
            match d[1] {
                DESCRIPTOR_CONFIGURATION if length >= 9 => descriptors.configuration = d[5],
                DESCRIPTOR_INTERFACE if length >= 9 => descriptors.interfaces.push(Interface {
                    number: d[2],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                    endpoints: alloc::vec::Vec::new(),
                }),
                DESCRIPTOR_ENDPOINT if length >= 7 => {
                    if let Some(interface) = descriptors.interfaces.last_mut() {
                        interface.endpoints.push(Endpoint {
                            address: d[2],
                            attributes: d[3],
                            packet_size: u16::from_le_bytes([d[4], d[5]]) & 0x7FF,
                            interval: d[6],
                        });
                    }
                }
                _ => {}
            }
            offset += length;
        }
        descriptors
    }

    /// The first HID boot-protocol keyboard interface (class 3, subclass 1,
    /// protocol 1) with an interrupt IN endpoint.
    pub fn boot_keyboard(&self) -> Option<KeyboardInterface> {
        self.interfaces.iter().filter(|i| i.class == 3 && i.subclass == 1 && i.protocol == 1).find_map(|interface| {
            let endpoint = interface.endpoints.iter().find(|e| e.address & 0x80 != 0 && e.attributes & 0x3 == 3)?;
            Some(KeyboardInterface {
                interface: interface.number,
                address: endpoint.address,
                packet_size: endpoint.packet_size,
                interval: endpoint.interval,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn setup_packets_are_laid_out_little_endian() {
            let packet = SetupPacket::get_descriptor(2, 0, 9);
            assert_eq!(packet.as_u64().to_le_bytes(), [0x80, 6, 0, 2, 0, 0, 9, 0]);
        }

        fn keyboards_are_found_in_configuration_descriptors() {
            // A configuration with a mass storage interface, then a boot
            // keyboard with an interrupt IN endpoint 1.
            let bytes = [
                9, 2, 50, 0, 2, 7, 0, 0xA0, 50,
                9, 4, 0, 0, 1, 8, 6, 0x50, 0,
                9, 4, 1, 0, 1, 3, 1, 1, 0,
                9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0,
                7, 5, 0x81, 3, 8, 0, 10,
            ];
            let descriptors = Descriptors::parse(&bytes);
            assert_eq!(descriptors.configuration, 7);
            let keyboard = descriptors.boot_keyboard().expect("no keyboard found");
            assert_eq!((keyboard.interface, keyboard.address, keyboard.packet_size, keyboard.interval), (1, 0x81, 8, 10));
        }

        fn usb_keyboards_come_up() {
            // QEMU gets a USB keyboard (see xtask), found during boot.
            if probe_count() > 0 {
                assert!(keyboard_count() > 0, "no USB keyboard came up");
            }
        }
    }

    fn probe_count() -> usize {
        pci::scan().iter().filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.interface == 0x30).count()
    }
}
