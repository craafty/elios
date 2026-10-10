//! AHCI: the standard interface to SATA disks, on real PCs and in QEMU's
//! q35 machine.
//!
//! The controller (HBA) has memory-mapped registers and up to 32 ports.
//! Each port with a disk gets a command list and a received-FIS area in
//! memory; a command is a table holding an ATA command (as a "frame
//! information structure") and a list of memory regions to transfer. The
//! driver uses one command slot per port and waits for completion by
//! polling.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;

use super::block::Disk;
use super::pci;
use crate::memory::{self, CacheType, PAGE_SIZE, frames, phys_to_virt};
use crate::sync::Mutex;
use crate::time;

// Controller registers.
const CAPABILITIES: usize = 0x00;
const GLOBAL_CONTROL: usize = 0x04;
const PORTS_IMPLEMENTED: usize = 0x0C;
const CAPABILITIES_2: usize = 0x24;
const HANDOFF: usize = 0x28;

const GLOBAL_AHCI_ENABLE: u32 = 1 << 31;
const CAPABILITY_64BIT: u32 = 1 << 31;
const HANDOFF_SUPPORTED: u32 = 1 << 0;
const HANDOFF_OS_OWNED: u32 = 1 << 1;
const HANDOFF_BIOS_OWNED: u32 = 1 << 0;

// Port registers (offsets within a port's block).
const PORT_COMMAND_LIST: usize = 0x00;
const PORT_FIS_BASE: usize = 0x08;
const PORT_INTERRUPT_STATUS: usize = 0x10;
const PORT_COMMAND: usize = 0x18;
const PORT_TASK_FILE: usize = 0x20;
const PORT_SIGNATURE: usize = 0x24;
const PORT_STATUS: usize = 0x28;
const PORT_ERROR: usize = 0x30;
const PORT_COMMAND_ISSUE: usize = 0x38;

const COMMAND_START: u32 = 1 << 0;
const COMMAND_FIS_RECEIVE: u32 = 1 << 4;
const COMMAND_FIS_RUNNING: u32 = 1 << 14;
const COMMAND_LIST_RUNNING: u32 = 1 << 15;
const TASK_FILE_ERROR: u32 = 1 << 0;
const TASK_FILE_BUSY: u32 = 1 << 7;
const TASK_FILE_DATA_REQUEST: u32 = 1 << 3;
const STATUS_TASK_FILE_ERROR: u32 = 1 << 30;
const SIGNATURE_SATA: u32 = 0x0000_0101;

// ATA commands.
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const FIS_HOST_TO_DEVICE: u8 = 0x27;

/// The bounce buffer for transfers: 64 KiB, contiguous in physical memory.
const BUFFER_PAGES: usize = 16;
const BUFFER_BYTES: usize = BUFFER_PAGES * PAGE_SIZE as usize;
const SECTOR: usize = 512;
const TIMEOUT_NS: u64 = 5_000_000_000;

struct Port {
    registers: usize,
    /// Physical addresses of the command list (with the FIS area after it),
    /// the command table, and the bounce buffer.
    command_list: u64,
    command_table: u64,
    buffer: u64,
}

pub struct AhciDisk {
    name: String,
    sectors: u64,
    port: Mutex<Port>,
}

fn read_register(address: usize) -> u32 {
    // SAFETY: an AHCI register, mapped uncached.
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn write_register(address: usize, value: u32) {
    // SAFETY: as for `read_register`.
    unsafe { ptr::write_volatile(address as *mut u32, value) };
}

/// Waits (up to the timeout) for `done` to hold.
fn wait_until(mut done: impl FnMut() -> bool) -> Result<(), &'static str> {
    let deadline = time::uptime_nanos() + TIMEOUT_NS;
    while !done() {
        if time::uptime_nanos() > deadline {
            return Err("the disk didn't respond");
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Finds AHCI controllers and the SATA disks on them.
pub fn probe(devices: &[pci::Device]) -> Vec<Arc<AhciDisk>> {
    let mut disks = Vec::new();
    for device in devices.iter().filter(|d| d.class == 0x01 && d.subclass == 0x06 && d.interface == 0x01) {
        let Some(base) = device.memory_bar(5) else { continue };
        device.enable_bus_mastering();
        let registers = memory::map_device(base, 0x1100, CacheType::Uncached) as usize;
        match init_controller(registers, disks.len()) {
            Ok(found) => disks.extend(found),
            Err(error) => warn!("ahci: controller at {base:#x}: {error}"),
        }
    }
    disks
}

fn init_controller(registers: usize, first_index: usize) -> Result<Vec<Arc<AhciDisk>>, &'static str> {
    // Take the controller over from the firmware, if it supports handing off.
    if read_register(registers + CAPABILITIES_2) & HANDOFF_SUPPORTED != 0 {
        let handoff = registers + HANDOFF;
        write_register(handoff, read_register(handoff) | HANDOFF_OS_OWNED);
        wait_until(|| read_register(handoff) & HANDOFF_BIOS_OWNED == 0)?;
    }
    let control = registers + GLOBAL_CONTROL;
    write_register(control, read_register(control) | GLOBAL_AHCI_ENABLE);
    let addressing_64 = read_register(registers + CAPABILITIES) & CAPABILITY_64BIT != 0;

    let implemented = read_register(registers + PORTS_IMPLEMENTED);
    let mut disks = Vec::new();
    for index in (0..32).filter(|index| implemented & (1 << index) != 0) {
        let port = registers + 0x100 + index * 0x80;
        // Detection: a device present with communication established.
        if read_register(port + PORT_STATUS) & 0xF != 3 || read_register(port + PORT_SIGNATURE) != SIGNATURE_SATA {
            continue;
        }
        match init_port(port, addressing_64) {
            Ok(port) => {
                let name = format!("sata{}", first_index + disks.len());
                match AhciDisk::identify(name, port) {
                    Ok(disk) => disks.push(Arc::new(disk)),
                    Err(error) => warn!("ahci: port {index}: {error}"),
                }
            }
            Err(error) => warn!("ahci: port {index}: {error}"),
        }
    }
    Ok(disks)
}

fn init_port(registers: usize, addressing_64: bool) -> Result<Port, &'static str> {
    // Stop the port before changing its memory areas.
    let command = registers + PORT_COMMAND;
    write_register(command, read_register(command) & !COMMAND_START);
    wait_until(|| read_register(command) & COMMAND_LIST_RUNNING == 0)?;
    write_register(command, read_register(command) & !COMMAND_FIS_RECEIVE);
    wait_until(|| read_register(command) & COMMAND_FIS_RUNNING == 0)?;

    // Without 64-bit addressing, DMA memory has to be below 4 GiB; the
    // allocator hands out low memory first, but check.
    let allocate = |pages: usize| -> Result<u64, &'static str> {
        let address = frames::allocate_contiguous(pages, pages).ok_or("out of memory")?;
        if !addressing_64 && address + (pages as u64 * PAGE_SIZE) > 1 << 32 {
            return Err("no DMA memory below 4 GiB");
        }
        // SAFETY: freshly allocated; all RAM is in the direct map.
        unsafe { ptr::write_bytes(phys_to_virt(address), 0, pages * PAGE_SIZE as usize) };
        Ok(address)
    };
    // One page: the command list (1 KiB, 32 headers) then the received-FIS
    // area (256 bytes).
    let command_list = allocate(1)?;
    let command_table = allocate(1)?;
    let buffer = allocate(BUFFER_PAGES)?;

    write_register(registers + PORT_COMMAND_LIST, command_list as u32);
    write_register(registers + PORT_COMMAND_LIST + 4, (command_list >> 32) as u32);
    write_register(registers + PORT_FIS_BASE, (command_list + 1024) as u32);
    write_register(registers + PORT_FIS_BASE + 4, ((command_list + 1024) >> 32) as u32);
    // Clear any stale errors and interrupts (write-1-to-clear).
    write_register(registers + PORT_ERROR, u32::MAX);
    write_register(registers + PORT_INTERRUPT_STATUS, u32::MAX);

    write_register(command, read_register(command) | COMMAND_FIS_RECEIVE);
    write_register(command, read_register(command) | COMMAND_START);
    Ok(Port { registers, command_list, command_table, buffer })
}

impl Port {
    /// Runs an ATA command that transfers `sectors` sectors through the
    /// bounce buffer.
    fn execute(&mut self, command: u8, lba: u64, sectors: usize, write: bool) -> Result<(), &'static str> {
        let task_file = self.registers + PORT_TASK_FILE;
        wait_until(|| read_register(task_file) & (TASK_FILE_BUSY | TASK_FILE_DATA_REQUEST) == 0)?;

        let bytes = sectors * SECTOR;
        // SAFETY: the command list and table are ours, in the direct map.
        unsafe {
            // Command header 0: FIS length 5 dwords, write flag, one PRDT
            // entry, and the table's address.
            let header = phys_to_virt(self.command_list).cast::<u32>();
            let flags = 5 | if write { 1 << 6 } else { 0 } | 1 << 16;
            header.write_volatile(flags);
            header.add(1).write_volatile(0);
            header.add(2).write_volatile(self.command_table as u32);
            header.add(3).write_volatile((self.command_table >> 32) as u32);

            let table = phys_to_virt(self.command_table);
            ptr::write_bytes(table, 0, 0x80 + 16);
            let fis = core::slice::from_raw_parts_mut(table, 20);
            fis[0] = FIS_HOST_TO_DEVICE;
            fis[1] = 1 << 7; // this is a command
            fis[2] = command;
            fis[4] = lba as u8;
            fis[5] = (lba >> 8) as u8;
            fis[6] = (lba >> 16) as u8;
            fis[7] = 1 << 6; // LBA addressing
            fis[8] = (lba >> 24) as u8;
            fis[9] = (lba >> 32) as u8;
            fis[10] = (lba >> 40) as u8;
            fis[12] = sectors as u8;
            fis[13] = (sectors >> 8) as u8;

            // One region: the bounce buffer.
            let entry = table.add(0x80).cast::<u32>();
            entry.write_volatile(self.buffer as u32);
            entry.add(1).write_volatile((self.buffer >> 32) as u32);
            entry.add(3).write_volatile((bytes - 1) as u32);
        }

        write_register(self.registers + PORT_INTERRUPT_STATUS, u32::MAX);
        write_register(self.registers + PORT_COMMAND_ISSUE, 1);
        let status = self.registers + PORT_INTERRUPT_STATUS;
        let issue = self.registers + PORT_COMMAND_ISSUE;
        wait_until(|| read_register(issue) & 1 == 0 || read_register(status) & STATUS_TASK_FILE_ERROR != 0)?;
        if read_register(status) & STATUS_TASK_FILE_ERROR != 0 || read_register(task_file) & TASK_FILE_ERROR != 0 {
            return Err("the disk reported an error");
        }
        Ok(())
    }

    fn buffer(&mut self) -> &mut [u8] {
        // SAFETY: the bounce buffer is ours, in the direct map.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(self.buffer), BUFFER_BYTES) }
    }
}

impl AhciDisk {
    fn identify(name: String, mut port: Port) -> Result<Self, &'static str> {
        port.execute(ATA_IDENTIFY, 0, 1, false)?;
        let identity = port.buffer();
        let word = |index: usize| u16::from_le_bytes([identity[index * 2], identity[index * 2 + 1]]);
        let lba48 = word(83) & (1 << 10) != 0;
        let sectors = if lba48 {
            (0..4).fold(0u64, |total, index| total | u64::from(word(100 + index)) << (16 * index))
        } else {
            u64::from(word(60)) | u64::from(word(61)) << 16
        };
        // The model name is stored with each pair of bytes swapped.
        let model: Vec<u8> = (27..47).flat_map(|index| word(index).to_be_bytes()).collect();
        let model = String::from(String::from_utf8_lossy(&model).trim());
        info!("ahci: {name}: {model}, {} MiB", sectors / 2048);
        Ok(Self { name, sectors, port: Mutex::new(port) })
    }
}

impl Disk for AhciDisk {
    fn name(&self) -> &str {
        &self.name
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let mut port = self.port.lock();
        for (index, chunk) in buffer.chunks_mut(BUFFER_BYTES).enumerate() {
            let sectors = chunk.len() / SECTOR;
            port.execute(ATA_READ_DMA_EXT, lba + (index * BUFFER_BYTES / SECTOR) as u64, sectors, false)?;
            chunk.copy_from_slice(&port.buffer()[..chunk.len()]);
        }
        Ok(())
    }

    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        let mut port = self.port.lock();
        for (index, chunk) in data.chunks(BUFFER_BYTES).enumerate() {
            let sectors = chunk.len() / SECTOR;
            port.buffer()[..chunk.len()].copy_from_slice(chunk);
            port.execute(ATA_WRITE_DMA_EXT, lba + (index * BUFFER_BYTES / SECTOR) as u64, sectors, true)?;
        }
        Ok(())
    }
}
