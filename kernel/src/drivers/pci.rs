//! PCI: finding devices and configuring them.
//!
//! Devices are found by reading each possible bus/device/function's
//! configuration space through the legacy I/O ports (0xCF8 to select, 0xCFC
//! for data), which every PC chipset supports. Both are 32-bit registers and
//! must be accessed whole: neighboring ports (0xCF9 is often the reset
//! control) do other things.

use alloc::vec::Vec;

use core::ptr;

use crate::arch::{inl, outl};
use crate::memory::{self, CacheType};

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
    /// Reads 32-bit configuration register `offset` (a multiple of 4).
    pub fn read(&self, offset: u8) -> u32 {
        read32(self.bus, self.slot, self.function, offset)
    }

    /// Writes 32-bit configuration register `offset` (a multiple of 4).
    pub fn write(&self, offset: u8, value: u32) {
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

    /// The offset of capability `id` in configuration space, if the device
    /// has it.
    fn capability(&self, id: u8) -> Option<u8> {
        // Status bit 4: there's a capability list.
        if self.read(0x04) >> 16 & 0x10 == 0 {
            return None;
        }
        let mut offset = self.read(0x34) as u8 & !3;
        // A bound, in case of a looping list.
        for _ in 0..48 {
            if offset == 0 {
                return None;
            }
            let header = self.read(offset);
            if header as u8 == id {
                return Some(offset);
            }
            offset = (header >> 8) as u8 & !3;
        }
        None
    }

    /// Makes the device signal interrupts as messages: a write the local
    /// APIC of CPU `apic_id` sees as interrupt `vector`. This needs no
    /// knowledge of how the board wires interrupt lines (which only ACPI's
    /// AML code describes). Uses MSI, or else MSI-X; returns false if the
    /// device has neither.
    pub fn enable_msi(&self, apic_id: u8, vector: u8) -> bool {
        const MSI: u8 = 0x05;
        let Some(offset) = self.capability(MSI) else { return self.enable_msi_x(apic_id, vector) };
        let control = self.read(offset) >> 16;
        let wide = control & 0x80 != 0;
        self.write(offset + 4, 0xFEE0_0000 | u32::from(apic_id) << 12);
        if wide {
            self.write(offset + 8, 0);
            self.write(offset + 12, u32::from(vector));
        } else {
            self.write(offset + 8, u32::from(vector));
        }
        // Enable with one message (multiple-message enable = 0), and turn
        // off the legacy interrupt line.
        let header = self.read(offset) & !(0x70 << 16);
        self.write(offset, header | 1 << 16);
        let command = self.read(0x04);
        self.write(0x04, command | 1 << 10);
        true
    }

    /// MSI-X: like MSI, but the messages are set in a table in one of the
    /// device's memory BARs, one entry per interrupt. Only the first entry
    /// is used; the others stay masked.
    fn enable_msi_x(&self, apic_id: u8, vector: u8) -> bool {
        const MSI_X: u8 = 0x11;
        let Some(offset) = self.capability(MSI_X) else { return false };
        let control = self.read(offset) >> 16;
        let entries = u64::from(control & 0x7FF) + 1;
        let table = self.read(offset + 4);
        let Some(bar) = self.memory_bar((table & 0x7) as u8) else { return false };
        let address = bar + u64::from(table & !0x7);
        let base = memory::map_device(address, entries * 16, CacheType::Uncached);
        // SAFETY: the table is mapped; each entry is four 32-bit words:
        // address low, address high, data, and vector control (bit 0 masks).
        unsafe {
            let entry = |index: u64, word: u64| base.add((index * 16 + word * 4) as usize).cast::<u32>();
            for index in 0..entries {
                ptr::write_volatile(entry(index, 3), 1);
            }
            ptr::write_volatile(entry(0, 0), 0xFEE0_0000 | u32::from(apic_id) << 12);
            ptr::write_volatile(entry(0, 1), 0);
            ptr::write_volatile(entry(0, 2), u32::from(vector));
            ptr::write_volatile(entry(0, 3), 0);
        }
        // Enable (bit 15), not masked as a whole (bit 14).
        let header = self.read(offset) & !(1 << 30);
        self.write(offset, header | 1 << 31);
        let command = self.read(0x04);
        self.write(0x04, command | 1 << 10);
        true
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
