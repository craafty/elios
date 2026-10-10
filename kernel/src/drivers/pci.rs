//! PCI: finding devices and configuring them.
//!
//! Devices are found by reading each possible bus/device/function's
//! configuration space through the legacy I/O ports (0xCF8 to select, 0xCFC
//! for data), which every PC chipset supports. Both are 32-bit registers and
//! must be accessed whole: neighboring ports (0xCF9 is often the reset
//! control) do other things.

use alloc::vec::Vec;

use crate::arch::{inl, outl};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

#[derive(Debug, Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
}

fn select(bus: u8, slot: u8, function: u8, offset: u8) {
    let address =
        1 << 31 | u32::from(bus) << 16 | u32::from(slot) << 11 | u32::from(function) << 8 | u32::from(offset & 0xFC);
    // SAFETY: selecting a configuration register has no other effect.
    unsafe { outl(CONFIG_ADDRESS, address) };
}

fn read32(bus: u8, slot: u8, function: u8, offset: u8) -> u32 {
    select(bus, slot, function, offset);
    // SAFETY: reads the selected configuration register.
    unsafe { inl(CONFIG_DATA) }
}

fn write32(bus: u8, slot: u8, function: u8, offset: u8, value: u32) {
    select(bus, slot, function, offset);
    // SAFETY: writes the selected configuration register; callers choose
    // what to write.
    unsafe { outl(CONFIG_DATA, value) };
}

impl Device {
    fn read(&self, offset: u8) -> u32 {
        read32(self.bus, self.slot, self.function, offset)
    }

    fn write(&self, offset: u8, value: u32) {
        write32(self.bus, self.slot, self.function, offset, value);
    }

    /// The physical address a memory BAR points at (handling 64-bit BARs).
    pub fn memory_bar(&self, index: u8) -> Option<u64> {
        let offset = 0x10 + index * 4;
        let low = self.read(offset);
        if low & 1 != 0 {
            // An I/O port BAR.
            return None;
        }
        let address = u64::from(low & !0xF);
        let high = if (low >> 1) & 3 == 2 { u64::from(self.read(offset + 4)) } else { 0 };
        Some(address | high << 32)
    }

    /// Lets the device access memory itself (DMA) and respond to memory
    /// accesses.
    pub fn enable_bus_mastering(&self) {
        let command = self.read(0x04);
        self.write(0x04, command | 0x2 | 0x4);
    }
}

/// Every PCI function present.
pub fn scan() -> Vec<Device> {
    let mut devices = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            for function in 0..8u8 {
                let id = read32(bus, slot, function, 0);
                if id & 0xFFFF == 0xFFFF {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                let class = read32(bus, slot, function, 0x08);
                devices.push(Device {
                    bus,
                    slot,
                    function,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                    interface: (class >> 8) as u8,
                });
                // Only multi-function devices have functions 1-7.
                let header_type = (read32(bus, slot, function, 0x0C) >> 16) as u8;
                if function == 0 && header_type & 0x80 == 0 {
                    break;
                }
            }
        }
    }
    devices
}
