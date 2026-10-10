//! NVMe: the interface to PCI Express SSDs, found in most modern laptops.
//!
//! The host talks to the controller through queues in memory: it writes
//! 64-byte commands into a submission queue and rings a doorbell register;
//! the controller writes 16-byte completions into a completion queue,
//! flipping a "phase" bit each time it wraps around, so new entries can be
//! told from old ones. There's an admin queue pair (for setup) and one I/O
//! queue pair here; completions are polled.

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
const CONFIGURATION: usize = 0x14;
const STATUS: usize = 0x1C;
const ADMIN_QUEUE_ATTRIBUTES: usize = 0x24;
const ADMIN_SUBMISSION_QUEUE: usize = 0x28;
const ADMIN_COMPLETION_QUEUE: usize = 0x30;
const DOORBELLS: usize = 0x1000;

const CONFIGURATION_ENABLE: u32 = 1 << 0;
/// 64-byte submission and 16-byte completion entries (as powers of two).
const CONFIGURATION_ENTRY_SIZES: u32 = 6 << 16 | 4 << 20;
const STATUS_READY: u32 = 1 << 0;
const STATUS_FATAL: u32 = 1 << 1;

// Admin commands.
const ADMIN_CREATE_SUBMISSION_QUEUE: u8 = 0x01;
const ADMIN_CREATE_COMPLETION_QUEUE: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
// I/O commands.
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

const QUEUE_ENTRIES: u16 = 64;
const BUFFER_PAGES: usize = 16;
const BUFFER_BYTES: usize = BUFFER_PAGES * PAGE_SIZE as usize;
const SECTOR: usize = 512;
const NAMESPACE: u32 = 1;

struct Queue {
    submission: u64,
    completion: u64,
    tail: u16,
    head: u16,
    phase: bool,
    submission_doorbell: usize,
    completion_doorbell: usize,
    next_id: u16,
}

struct Controller {
    admin: Queue,
    io: Queue,
    /// The bounce buffer, and a page holding the list of its pages (a "PRP
    /// list") for transfers longer than two pages.
    buffer: u64,
    page_list: u64,
    timeout_ns: u64,
}

pub struct NvmeDisk {
    name: String,
    sectors: u64,
    controller: Mutex<Controller>,
}

fn read32(address: usize) -> u32 {
    // SAFETY: an NVMe register, mapped uncached.
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // SAFETY: as for `read32`.
    unsafe { ptr::write_volatile(address as *mut u32, value) };
}

fn write64(address: usize, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

fn zeroed_pages(pages: usize) -> Result<u64, &'static str> {
    let address = frames::allocate_contiguous(pages, pages).ok_or("out of memory")?;
    // SAFETY: freshly allocated; all RAM is in the direct map.
    unsafe { ptr::write_bytes(phys_to_virt(address), 0, pages * PAGE_SIZE as usize) };
    Ok(address)
}

fn wait_until(timeout_ns: u64, mut done: impl FnMut() -> bool) -> Result<(), &'static str> {
    let deadline = time::uptime_nanos() + timeout_ns;
    while !done() {
        if time::uptime_nanos() > deadline {
            return Err("the controller didn't respond");
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Finds NVMe controllers and their first namespace.
pub fn probe(devices: &[pci::Device]) -> Vec<Arc<NvmeDisk>> {
    let mut disks = Vec::new();
    for device in devices.iter().filter(|d| d.class == 0x01 && d.subclass == 0x08 && d.interface == 0x02) {
        let Some(base) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        let registers = memory::map_device(base, 0x2000, CacheType::Uncached) as usize;
        let name = format!("nvme{}", disks.len());
        match NvmeDisk::start(name, registers) {
            Ok(disk) => disks.push(Arc::new(disk)),
            Err(error) => warn!("nvme: controller at {base:#x}: {error}"),
        }
    }
    disks
}

impl Queue {
    fn new(registers: usize, stride: usize, index: usize) -> Result<Self, &'static str> {
        Ok(Self {
            submission: zeroed_pages(1)?,
            completion: zeroed_pages(1)?,
            tail: 0,
            head: 0,
            phase: true,
            submission_doorbell: registers + DOORBELLS + (2 * index) * stride,
            completion_doorbell: registers + DOORBELLS + (2 * index + 1) * stride,
            next_id: 0,
        })
    }

    /// Submits a command (16 dwords, with the id filled in here) and waits
    /// for its completion. Returns completion dword 0.
    fn run(&mut self, mut command: [u32; 16], timeout_ns: u64) -> Result<u32, &'static str> {
        self.next_id = self.next_id.wrapping_add(1);
        command[0] |= u32::from(self.next_id) << 16;
        // SAFETY: the queues are ours, in the direct map.
        unsafe {
            let slot = phys_to_virt(self.submission).cast::<u32>().add(usize::from(self.tail) * 16);
            for (index, value) in command.iter().enumerate() {
                slot.add(index).write_volatile(*value);
            }
        }
        self.tail = (self.tail + 1) % QUEUE_ENTRIES;
        write32(self.submission_doorbell, u32::from(self.tail));

        // SAFETY: as above.
        let entry = unsafe { phys_to_virt(self.completion).cast::<u32>().add(usize::from(self.head) * 4) };
        let phase = self.phase;
        // SAFETY: as above.
        wait_until(timeout_ns, || unsafe { (entry.add(3).read_volatile() >> 16) & 1 == u32::from(phase) })?;
        // SAFETY: as above.
        let (result, status) = unsafe { (entry.read_volatile(), entry.add(3).read_volatile() >> 17) };
        self.head = (self.head + 1) % QUEUE_ENTRIES;
        if self.head == 0 {
            self.phase = !self.phase;
        }
        write32(self.completion_doorbell, u32::from(self.head));
        if status & 0x7FF != 0 {
            return Err("the controller reported an error");
        }
        Ok(result)
    }
}

impl NvmeDisk {
    fn start(name: String, registers: usize) -> Result<Self, &'static str> {
        let capabilities =
            read32(registers + CAPABILITIES) as u64 | (read32(registers + CAPABILITIES + 4) as u64) << 32;
        let stride = 4 << ((capabilities >> 32) & 0xF);
        // The controller's own worst case for enabling, in 500 ms units.
        let timeout_ns = ((capabilities >> 24) & 0xFF).max(2) * 500_000_000;

        // Reset, set up the admin queues, enable.
        let configuration = registers + CONFIGURATION;
        write32(configuration, read32(configuration) & !CONFIGURATION_ENABLE);
        wait_until(timeout_ns, || read32(registers + STATUS) & STATUS_READY == 0)?;
        let admin = Queue::new(registers, stride, 0)?;
        let entries = u32::from(QUEUE_ENTRIES - 1);
        write32(registers + ADMIN_QUEUE_ATTRIBUTES, entries << 16 | entries);
        write64(registers + ADMIN_SUBMISSION_QUEUE, admin.submission);
        write64(registers + ADMIN_COMPLETION_QUEUE, admin.completion);
        write32(configuration, CONFIGURATION_ENTRY_SIZES | CONFIGURATION_ENABLE);
        wait_until(timeout_ns, || read32(registers + STATUS) & (STATUS_READY | STATUS_FATAL) != 0)?;
        if read32(registers + STATUS) & STATUS_FATAL != 0 {
            return Err("the controller failed to start");
        }

        let buffer = zeroed_pages(BUFFER_PAGES)?;
        let page_list = zeroed_pages(1)?;
        // SAFETY: our page; entries for the buffer's pages after the first.
        unsafe {
            let list = phys_to_virt(page_list).cast::<u64>();
            for page in 1..BUFFER_PAGES {
                list.add(page - 1).write(buffer + page as u64 * PAGE_SIZE);
            }
        }
        let io = Queue::new(registers, stride, 1)?;
        let mut controller = Controller { admin, io, buffer, page_list, timeout_ns };

        // Identify the controller (for its model name) and namespace 1.
        let mut identify = [0u32; 16];
        identify[0] = u32::from(ADMIN_IDENTIFY);
        identify[6] = buffer as u32;
        identify[7] = (buffer >> 32) as u32;
        identify[10] = 1;
        controller.admin.run(identify, timeout_ns)?;
        let model = String::from(String::from_utf8_lossy(&controller.buffer()[24..64]).trim());
        identify[1] = NAMESPACE;
        identify[10] = 0;
        controller.admin.run(identify, timeout_ns)?;
        let data = controller.buffer();
        let sectors = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let format = usize::from(data[26] & 0xF);
        let sector_shift = data[128 + format * 4 + 2];
        if sector_shift != 9 {
            return Err("only 512-byte sectors are supported");
        }

        // The I/O queue pair: completion queue first.
        let entries = u32::from(QUEUE_ENTRIES - 1) << 16 | 1;
        let mut create = [0u32; 16];
        create[0] = u32::from(ADMIN_CREATE_COMPLETION_QUEUE);
        create[6] = controller.io.completion as u32;
        create[7] = (controller.io.completion >> 32) as u32;
        create[10] = entries;
        create[11] = 1; // physically contiguous, no interrupts
        controller.admin.run(create, timeout_ns)?;
        create[0] = u32::from(ADMIN_CREATE_SUBMISSION_QUEUE);
        create[6] = controller.io.submission as u32;
        create[7] = (controller.io.submission >> 32) as u32;
        create[11] = 1 << 16 | 1; // completion queue 1, contiguous
        controller.admin.run(create, timeout_ns)?;

        info!("nvme: {name}: {model}, {} MiB", sectors / 2048);
        Ok(Self { name, sectors, controller: Mutex::new(controller) })
    }
}

impl Controller {
    fn buffer(&mut self) -> &mut [u8] {
        // SAFETY: the bounce buffer is ours, in the direct map.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(self.buffer), BUFFER_BYTES) }
    }

    fn transfer(&mut self, opcode: u8, lba: u64, bytes: usize) -> Result<(), &'static str> {
        let mut command = [0u32; 16];
        command[0] = u32::from(opcode);
        command[1] = NAMESPACE;
        command[6] = self.buffer as u32;
        command[7] = (self.buffer >> 32) as u32;
        // The second pointer: the next page, or a list of the rest.
        let second = match bytes.div_ceil(PAGE_SIZE as usize) {
            0 | 1 => 0,
            2 => self.buffer + PAGE_SIZE,
            _ => self.page_list,
        };
        command[8] = second as u32;
        command[9] = (second >> 32) as u32;
        command[10] = lba as u32;
        command[11] = (lba >> 32) as u32;
        command[12] = (bytes / SECTOR - 1) as u32;
        self.io.run(command, self.timeout_ns).map(|_| ())
    }
}

impl Disk for NvmeDisk {
    fn name(&self) -> &str {
        &self.name
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let mut controller = self.controller.lock();
        for (index, chunk) in buffer.chunks_mut(BUFFER_BYTES).enumerate() {
            controller.transfer(IO_READ, lba + (index * BUFFER_BYTES / SECTOR) as u64, chunk.len())?;
            chunk.copy_from_slice(&controller.buffer()[..chunk.len()]);
        }
        Ok(())
    }

    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        let mut controller = self.controller.lock();
        for (index, chunk) in data.chunks(BUFFER_BYTES).enumerate() {
            controller.buffer()[..chunk.len()].copy_from_slice(chunk);
            controller.transfer(IO_WRITE, lba + (index * BUFFER_BYTES / SECTOR) as u64, chunk.len())?;
        }
        Ok(())
    }
}
