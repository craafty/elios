//! ACPI table discovery. For now only the MADT ("APIC" table) is read: it
//! lists the processors and interrupt controllers, and how legacy IRQs are
//! wired to them.

use core::ptr;

use crate::collections::ArrayVec;
use crate::memory;
use crate::sync::Once;

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
/// processors this applies to; in practice it's all of them, so it's
/// applied to every CPU.)
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

static MADT: Once<Madt> = Once::new();

/// Reads the ACPI tables the kernel needs. Must run while the bootloader's
/// mappings are active: the tables can be anywhere in the low 4 GiB, which
/// those cover.
pub fn init(rsdp: u64) -> Result<&'static Madt, &'static str> {
    let tables = root_tables(rsdp)?;
    if let Some(fadt) = find_table(&tables, b"FACP") {
        let _ = MAKER.set(read::<[u8; 6]>(fadt + 10));
        if let Some(power) = parse_fadt(fadt) {
            let _ = POWER.set(power);
        }
    }
    if let Some(ecdt) = find_table(&tables, b"ECDT") {
        // Two generic addresses (12 bytes each, the address 4 bytes in)
        // after the header: the command/status port, then the data port.
        let (command, data) = (read::<u64>(ecdt + SDT_HEADER_SIZE + 4), read::<u64>(ecdt + SDT_HEADER_SIZE + 16));
        let in_io_space = read::<u8>(ecdt + SDT_HEADER_SIZE) == 1 && read::<u8>(ecdt + SDT_HEADER_SIZE + 12) == 1;
        if in_io_space && command <= 0xFFFF && data <= 0xFFFF {
            let _ = EMBEDDED_CONTROLLER.set((command as u16, data as u16));
        }
    }
    let madt = find_table(&tables, b"APIC").ok_or("the ACPI tables have no MADT")?;
    MADT.set(parse_madt(madt, read::<u32>(madt + 4))).map_err(|_| "ACPI initialized twice")
}

/// The MADT read by [`init`].
pub fn madt() -> Option<&'static Madt> {
    MADT.get()
}

/// How to power off and reset the machine, from the FADT and DSDT.
#[derive(Debug, Clone, Copy)]
pub struct Power {
    /// The PM1 control ports (b may be absent, 0).
    pub pm1a_control: u16,
    pub pm1b_control: u16,
    /// The S5 ("soft off") sleep type values for each.
    pub sleep_type_a: u16,
    pub sleep_type_b: u16,
    /// For switching the chipset into ACPI mode, if the firmware hasn't.
    pub smi_command: u16,
    pub acpi_enable: u8,
    /// The reset register, if it's an I/O port.
    pub reset: Option<(u16, u8)>,
}

static POWER: Once<Power> = Once::new();

pub fn power() -> Option<&'static Power> {
    POWER.get()
}

static MAKER: Once<[u8; 6]> = Once::new();
static EMBEDDED_CONTROLLER: Once<(u16, u16)> = Once::new();

/// The OEM ID in the FADT: who made the firmware ("LENOVO", "BOCHS ").
pub fn maker() -> &'static str {
    MAKER.get().and_then(|id| core::str::from_utf8(id).ok()).map_or("", str::trim_end)
}

/// The embedded controller's command/status and data ports, from the
/// ECDT, if the firmware has one.
pub fn embedded_controller() -> Option<(u16, u16)> {
    EMBEDDED_CONTROLLER.get().copied()
}

/// The addresses of every table the RSDT or XSDT lists. (This runs before
/// the heap exists, hence the fixed-size list.)
fn root_tables(rsdp: u64) -> Result<ArrayVec<u64, 64>, &'static str> {
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
    let mut tables = ArrayVec::new();
    for index in 0..entries {
        let entry = root + SDT_HEADER_SIZE + index * entry_size;
        let _ = tables.push(if entry_size == 8 { read::<u64>(entry) } else { u64::from(read::<u32>(entry)) });
    }
    Ok(tables)
}

/// The table with this signature, if present and intact.
fn find_table(tables: &[u64], signature: &[u8; 4]) -> Option<u64> {
    tables
        .iter()
        .copied()
        .find(|&table| read::<[u8; 4]>(table) == *signature && checksum_ok(table, read::<u32>(table + 4)))
}

/// The power management details from the FADT ("FACP") and the DSDT it
/// points to.
fn parse_fadt(fadt: u64) -> Option<Power> {
    let length = read::<u32>(fadt + 4);
    let dsdt = if length >= 148 && read::<u64>(fadt + 140) != 0 {
        read::<u64>(fadt + 140)
    } else {
        u64::from(read::<u32>(fadt + 40))
    };
    let (sleep_type_a, sleep_type_b) = find_s5(dsdt)?;
    // The reset register (ACPI 2.0+): a generic address; only I/O port
    // ones (address space 1) are used.
    let flags = if length >= 116 { read::<u32>(fadt + 112) } else { 0 };
    let reset = (length >= 129 && flags & (1 << 10) != 0 && read::<u8>(fadt + 116) == 1)
        .then(|| (read::<u64>(fadt + 120) as u16, read::<u8>(fadt + 128)));
    Some(Power {
        pm1a_control: read::<u32>(fadt + 64) as u16,
        pm1b_control: read::<u32>(fadt + 68) as u16,
        sleep_type_a,
        sleep_type_b,
        smi_command: read::<u32>(fadt + 48) as u16,
        acpi_enable: read::<u8>(fadt + 52),
        reset,
    })
}

/// Finds the `\_S5` object in the DSDT's AML bytecode and reads its sleep
/// type values. A full AML interpreter isn't needed for this one object: it
/// is always `Name (_S5, Package () { a, b, ... })`.
fn find_s5(dsdt: u64) -> Option<(u16, u16)> {
    if read::<[u8; 4]>(dsdt) != *b"DSDT" {
        return None;
    }
    let length = read::<u32>(dsdt + 4) as u64;
    // SAFETY: the DSDT is ACPI memory, in the direct map.
    let aml = unsafe {
        core::slice::from_raw_parts(memory::phys_to_virt(dsdt + SDT_HEADER_SIZE), (length - SDT_HEADER_SIZE) as usize)
    };
    let at = aml.windows(5).position(|w| &w[1..] == b"_S5_" && (w[0] == 0x08 || w[0] == b'\\'))?;
    let mut cursor = at + 5;
    if aml.get(cursor) != Some(&0x12) {
        // PackageOp.
        return None;
    }
    cursor += 1;
    // PkgLength: the top two bits of the first byte count extra bytes.
    cursor += 1 + usize::from(aml.get(cursor)? >> 6);
    cursor += 1; // NumElements
    let mut value = || -> Option<u16> {
        let byte = *aml.get(cursor)?;
        cursor += 1;
        Some(match byte {
            // BytePrefix, then the value.
            0x0A => {
                cursor += 1;
                u16::from(*aml.get(cursor - 1)?)
            }
            // ZeroOp, OneOp, or a small constant written directly.
            other => u16::from(other),
        })
    };
    let a = value()?;
    let b = value()?;
    Some((a, b))
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
            let madt = madt().unwrap();
            assert!(!madt.processors.is_empty());
            assert!(!madt.io_apics.is_empty());
            assert_ne!(madt.local_apic_address, 0);
        }
    }
}
