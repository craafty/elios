//! ACPI table discovery. For now only the MADT ("APIC" table) is read: it
//! lists the processors and interrupt controllers, and how legacy IRQs are
//! wired to them.

use core::ptr;

use crate::collections::ArrayVec;
use crate::memory;

#[derive(Debug, Clone, Copy, Default)]
pub struct IoApicInfo {
    pub address: u32,
    /// The first global system interrupt this I/O APIC handles.
    pub gsi_base: u32,
}

/// A legacy ISA IRQ that isn't wired to the GSI of the same number, or not
/// with ISA's default signaling (edge-triggered, active high).
#[derive(Debug, Clone, Copy, Default)]
pub struct InterruptOverride {
    pub irq: u8,
    pub gsi: u32,
    pub active_low: bool,
    pub level_triggered: bool,
}

/// A local APIC input wired to the NMI line. (The table also says which
/// processors this applies to; with one CPU running, it's always ours.)
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalNmi {
    pub lint: u8,
    pub active_low: bool,
}

pub struct Madt {
    pub local_apic_address: u64,
    /// Whether the machine also has the legacy 8259 PICs.
    pub legacy_pics: bool,
    /// APIC IDs of the enabled processors.
    pub processors: ArrayVec<u32, 256>,
    pub io_apics: ArrayVec<IoApicInfo, 8>,
    pub overrides: ArrayVec<InterruptOverride, 16>,
    pub local_nmis: ArrayVec<LocalNmi, 8>,
}

const SDT_HEADER_SIZE: u64 = 36;

/// Finds and parses the MADT, starting from the RSDP the bootloader found.
pub fn find_madt(rsdp: u64) -> Result<Madt, &'static str> {
    if rsdp == 0 {
        return Err("the firmware provided no ACPI tables");
    }
    if read::<[u8; 8]>(rsdp) != *b"RSD PTR " || !checksum_ok(rsdp, 20) {
        return Err("the ACPI RSDP is corrupt");
    }

    // ACPI 2.0+ has the 64-bit XSDT; ACPI 1.0 only the 32-bit RSDT.
    let revision = read::<u8>(rsdp + 15);
    let xsdt = if revision >= 2 && checksum_ok(rsdp, read::<u32>(rsdp + 20)) { read::<u64>(rsdp + 24) } else { 0 };
    let (root, entry_size) = if xsdt != 0 { (xsdt, 8) } else { (u64::from(read::<u32>(rsdp + 16)), 4) };

    let root_length = read::<u32>(root + 4);
    if !checksum_ok(root, root_length) {
        return Err("the ACPI root table is corrupt");
    }
    let entries = (u64::from(root_length) - SDT_HEADER_SIZE) / entry_size;
    for index in 0..entries {
        let entry = root + SDT_HEADER_SIZE + index * entry_size;
        let table = if entry_size == 8 { read::<u64>(entry) } else { u64::from(read::<u32>(entry)) };
        if read::<[u8; 4]>(table) == *b"APIC" {
            let length = read::<u32>(table + 4);
            if !checksum_ok(table, length) {
                return Err("the ACPI MADT is corrupt");
            }
            return Ok(parse_madt(table, length));
        }
    }
    Err("the ACPI tables have no MADT")
}

fn parse_madt(table: u64, length: u32) -> Madt {
    let mut madt = Madt {
        local_apic_address: u64::from(read::<u32>(table + 36)),
        legacy_pics: read::<u32>(table + 40) & 1 != 0,
        processors: ArrayVec::new(),
        io_apics: ArrayVec::new(),
        overrides: ArrayVec::new(),
        local_nmis: ArrayVec::new(),
    };

    // Entries that don't fit are dropped: these limits are far beyond the
    // machines eliOS runs on.
    let mut offset = SDT_HEADER_SIZE + 8;
    while offset + 2 <= u64::from(length) {
        let entry = table + offset;
        let entry_length = read::<u8>(entry + 1);
        if entry_length < 2 {
            break;
        }
        match read::<u8>(entry) {
            // Processor local APIC, if enabled.
            0 if read::<u32>(entry + 4) & 1 != 0 => {
                let _ = madt.processors.push(u32::from(read::<u8>(entry + 3)));
            }
            1 => {
                let _ = madt.io_apics.push(IoApicInfo { address: read(entry + 4), gsi_base: read(entry + 8) });
            }
            // Interrupt source override; bus 0 is ISA.
            2 if read::<u8>(entry + 2) == 0 => {
                let flags = read::<u16>(entry + 8);
                let _ = madt.overrides.push(InterruptOverride {
                    irq: read(entry + 3),
                    gsi: read(entry + 4),
                    active_low: flags & 0b11 == 0b11,
                    level_triggered: flags >> 2 & 0b11 == 0b11,
                });
            }
            4 => {
                let flags = read::<u16>(entry + 3);
                let _ = madt.local_nmis.push(LocalNmi { lint: read(entry + 5), active_low: flags & 0b11 == 0b11 });
            }
            // Local APIC address override (64-bit).
            5 => madt.local_apic_address = read(entry + 4),
            // Processor local x2APIC (for APIC IDs above 255), if enabled.
            9 if read::<u32>(entry + 8) & 1 != 0 => {
                let _ = madt.processors.push(read(entry + 4));
            }
            _ => {}
        }
        offset += u64::from(entry_length);
    }
    madt
}

/// Reads a value from physical memory. ACPI tables are byte-packed, so
/// fields are often misaligned.
fn read<T: Copy>(phys: u64) -> T {
    // SAFETY: ACPI tables live in RAM covered by the direct map.
    unsafe { ptr::read_unaligned(memory::phys_to_virt(phys).cast::<T>()) }
}

/// ACPI structures are valid when all their bytes sum to zero.
fn checksum_ok(phys: u64, length: u32) -> bool {
    // SAFETY: as for `read`.
    let bytes = unsafe { core::slice::from_raw_parts(memory::phys_to_virt(phys), length as usize) };
    bytes.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn madt_lists_processors_and_io_apics() {
            let madt = find_madt(crate::test::boot_info().rsdp).unwrap();
            assert!(!madt.processors.is_empty());
            assert!(!madt.io_apics.is_empty());
            assert_ne!(madt.local_apic_address, 0);
        }
    }
}
