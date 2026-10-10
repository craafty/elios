//! Intel's e1000 family of Ethernet controllers (the 8254x and the 82574).
//! QEMU, VirtualBox and VMware all emulate one, and the design lives on in
//! Intel's later chips.
//!
//! Packets move through two rings of 16-byte descriptors in memory, one for
//! receiving and one for transmitting. Each descriptor points at a buffer.
//! The driver hands descriptors to the card by moving a ring's tail
//! register; the card hands them back by setting their "descriptor done"
//! bit.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::pci;
use crate::arch::{apic, interrupts};
use crate::memory::{self, CacheType, PAGE_SIZE, frames, phys_to_virt};
use crate::net::{self, NetDevice};
use crate::sync::SpinLock;
use crate::task::{self, WaitQueue};

const CTRL: u64 = 0x0000;
const STATUS: u64 = 0x0008;
const EERD: u64 = 0x0014;
const ICR: u64 = 0x00C0;
const IMS: u64 = 0x00D0;
const IMC: u64 = 0x00D8;
const RCTL: u64 = 0x0100;
const TCTL: u64 = 0x0400;
const TIPG: u64 = 0x0410;
const RDBAL: u64 = 0x2800;
const RDBAH: u64 = 0x2804;
const RDLEN: u64 = 0x2808;
const RDH: u64 = 0x2810;
const RDT: u64 = 0x2818;
const TDBAL: u64 = 0x3800;
const TDBAH: u64 = 0x3804;
const TDLEN: u64 = 0x3808;
const TDH: u64 = 0x3810;
const TDT: u64 = 0x3818;
const MTA: u64 = 0x5200;
const RAL0: u64 = 0x5400;
const RAH0: u64 = 0x5404;

const CTRL_SET_LINK_UP: u32 = 1 << 6;
const CTRL_RESET: u32 = 1 << 26;
const STATUS_LINK_UP: u32 = 1 << 1;
const RCTL_ENABLE: u32 = 1 << 1;
const RCTL_BROADCAST: u32 = 1 << 15;
const RCTL_STRIP_CRC: u32 = 1 << 26;
const TCTL_ENABLE: u32 = 1 << 1;
const TCTL_PAD_SHORT: u32 = 1 << 3;
/// Interrupts: receive timer, receive overrun, link change.
const INTERRUPTS: u32 = 1 << 7 | 1 << 6 | 1 << 2;

const DESCRIPTOR_DONE: u8 = 1 << 0;
const END_OF_PACKET: u8 = 1 << 1;
const COMMAND_END_OF_PACKET: u8 = 1 << 0;
const COMMAND_INSERT_CRC: u8 = 1 << 1;
const COMMAND_REPORT_STATUS: u8 = 1 << 3;

const RX_COUNT: usize = 64;
const TX_COUNT: usize = 64;
const BUFFER_SIZE: usize = 2048;

const NIC_VECTOR: u8 = interrupts::IRQ_BASE + 0x21;

/// The controllers this driver knows: 82540EM (QEMU's and VirtualBox's
/// e1000), 82545EM (VMware's), several other 8254x, and the 82574L (QEMU's
/// e1000e).
const DEVICE_IDS: [u16; 9] = [0x100E, 0x100F, 0x1004, 0x100C, 0x1010, 0x1011, 0x101E, 0x1026, 0x10D3];

static RECEIVED: WaitQueue = WaitQueue::new();
static USES_INTERRUPTS: AtomicBool = AtomicBool::new(false);

pub struct E1000 {
    registers: u64,
    mac: [u8; 6],
    /// Descriptor rings (one page each) and the packet buffers they point
    /// at (two per page).
    rx_ring: u64,
    tx_ring: u64,
    rx_buffers: Vec<u64>,
    tx_buffers: Vec<u64>,
    rx_next: SpinLock<usize>,
    tx_next: SpinLock<usize>,
}

/// Starts the driver for each supported controller.
pub fn probe(devices: &[pci::Device]) {
    for device in devices.iter().filter(|d| d.vendor == 0x8086 && DEVICE_IDS.contains(&d.device)) {
        let Some(base) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        match E1000::new(base) {
            Ok(nic) => {
                let nic = Arc::new(nic);
                if device.enable_msi(apic::local_id(), NIC_VECTOR) {
                    interrupts::set_handler(NIC_VECTOR, |_| RECEIVED.wake_all());
                    USES_INTERRUPTS.store(true, Ordering::Relaxed);
                    nic.write(IMS, INTERRUPTS);
                }
                let mac = nic.mac;
                info!(
                    "net: e1000 {:04x}: {}{}",
                    device.device,
                    net::format_mac(&mac),
                    if USES_INTERRUPTS.load(Ordering::Relaxed) { "" } else { ", polled" }
                );
                let receiver = nic.clone();
                net::add_interface(nic);
                task::spawn("e1000", move || receiver.receive_loop());
            }
            Err(error) => warn!("net: e1000 {:04x}: {error}", device.device),
        }
    }
}

fn dma_pages(count: usize) -> Result<u64, &'static str> {
    let phys = frames::allocate_contiguous(count, 1).ok_or("out of memory")?;
    // SAFETY: fresh RAM in the direct map.
    unsafe { ptr::write_bytes(phys_to_virt(phys), 0, count * PAGE_SIZE as usize) };
    Ok(phys)
}

impl E1000 {
    fn new(base: u64) -> Result<Self, &'static str> {
        let registers = memory::map_device(base, 0x2_0000, CacheType::Uncached) as u64;
        let buffers = |count: usize| -> Result<Vec<u64>, &'static str> {
            let mut list = Vec::new();
            for _ in 0..count / 2 {
                let page = dma_pages(1)?;
                list.extend([page, page + BUFFER_SIZE as u64]);
            }
            Ok(list)
        };
        let mut nic = Self {
            registers,
            mac: [0; 6],
            rx_ring: dma_pages(1)?,
            tx_ring: dma_pages(1)?,
            rx_buffers: buffers(RX_COUNT)?,
            tx_buffers: buffers(TX_COUNT)?,
            rx_next: SpinLock::new(0),
            tx_next: SpinLock::new(0),
        };

        nic.write(IMC, u32::MAX);
        nic.write(CTRL, nic.read(CTRL) | CTRL_RESET);
        task::sleep_ms(10);
        nic.write(IMC, u32::MAX);
        nic.read(ICR);
        nic.write(CTRL, nic.read(CTRL) | CTRL_SET_LINK_UP);
        nic.mac = nic.read_mac().ok_or("no MAC address")?;
        for index in 0..128 {
            nic.write(MTA + index * 4, 0);
        }

        for (index, &buffer) in nic.rx_buffers.iter().enumerate() {
            // SAFETY: a receive descriptor starts with its buffer's address.
            unsafe { ptr::write_volatile(nic.descriptor(nic.rx_ring, index).cast::<u64>(), buffer) };
        }
        nic.write(RDBAL, nic.rx_ring as u32);
        nic.write(RDBAH, (nic.rx_ring >> 32) as u32);
        nic.write(RDLEN, (RX_COUNT * 16) as u32);
        nic.write(RDH, 0);
        nic.write(RDT, (RX_COUNT - 1) as u32);
        nic.write(RCTL, RCTL_ENABLE | RCTL_BROADCAST | RCTL_STRIP_CRC);

        nic.write(TDBAL, nic.tx_ring as u32);
        nic.write(TDBAH, (nic.tx_ring >> 32) as u32);
        nic.write(TDLEN, (TX_COUNT * 16) as u32);
        nic.write(TDH, 0);
        nic.write(TDT, 0);
        // Collision threshold and distance: the standard values.
        nic.write(TCTL, TCTL_ENABLE | TCTL_PAD_SHORT | 0x10 << 4 | 0x40 << 12);
        nic.write(TIPG, 0x0060_200A);
        Ok(nic)
    }

    fn read(&self, register: u64) -> u32 {
        // SAFETY: a register in the mapped block.
        unsafe { ptr::read_volatile((self.registers + register) as *const u32) }
    }

    fn write(&self, register: u64, value: u32) {
        // SAFETY: as for `read`.
        unsafe { ptr::write_volatile((self.registers + register) as *mut u32, value) };
    }

    fn descriptor(&self, ring: u64, index: usize) -> *mut u8 {
        // SAFETY: rings are a page of 16-byte descriptors.
        unsafe { phys_to_virt(ring).add(index * 16) }
    }

    /// The MAC address: in the receive address registers, where the card
    /// loads it from its EEPROM at reset, or else from the EEPROM directly.
    fn read_mac(&self) -> Option<[u8; 6]> {
        let (low, high) = (self.read(RAL0), self.read(RAH0));
        if high & 1 << 31 != 0 {
            let [a, b, c, d] = low.to_le_bytes();
            let [e, f, ..] = high.to_le_bytes();
            return Some([a, b, c, d, e, f]);
        }
        let mut mac = [0; 6];
        for word in 0..3u32 {
            let value = self.read_eeprom(word)?;
            mac[word as usize * 2..][..2].copy_from_slice(&value.to_le_bytes());
        }
        // Make the card accept frames for it.
        self.write(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        self.write(RAH0, u32::from(mac[4]) | u32::from(mac[5]) << 8 | 1 << 31);
        Some(mac)
    }

    /// Reads a 16-bit EEPROM word. The 8254x and the 82574 put the address
    /// and the done bit in different places, so both are tried.
    fn read_eeprom(&self, word: u32) -> Option<u16> {
        for (shift, done) in [(8, 1 << 4), (2, 1 << 1)] {
            self.write(EERD, word << shift | 1);
            for _ in 0..10_000 {
                let value = self.read(EERD);
                if value & done != 0 {
                    return Some((value >> 16) as u16);
                }
                core::hint::spin_loop();
            }
        }
        None
    }

    /// Hands received packets to the network stack, forever.
    fn receive_loop(&self) {
        loop {
            while let Some(frame) = self.next_frame() {
                net::receive(frame);
            }
            // Reading ICR acknowledges the interrupt.
            self.read(ICR);
            let deadline = (!USES_INTERRUPTS.load(Ordering::Relaxed)).then(|| crate::time::ticks() + 2);
            RECEIVED.wait_until_deadline(deadline, || self.frame_ready().then_some(()));
        }
    }

    fn frame_ready(&self) -> bool {
        let index = *self.rx_next.lock();
        // SAFETY: byte 12 of a receive descriptor is its status.
        unsafe { ptr::read_volatile(self.descriptor(self.rx_ring, index).add(12)) & DESCRIPTOR_DONE != 0 }
    }

    fn next_frame(&self) -> Option<Vec<u8>> {
        let mut next = self.rx_next.lock();
        let descriptor = self.descriptor(self.rx_ring, *next);
        // SAFETY: a receive descriptor: length at 8, status at 12.
        let (status, length) =
            unsafe { (ptr::read_volatile(descriptor.add(12)), ptr::read_volatile(descriptor.add(8).cast::<u16>())) };
        if status & DESCRIPTOR_DONE == 0 {
            return None;
        }
        let length = usize::from(length).min(BUFFER_SIZE);
        let frame = if status & END_OF_PACKET != 0 {
            // SAFETY: the card wrote `length` bytes to the buffer.
            Some(unsafe { core::slice::from_raw_parts(phys_to_virt(self.rx_buffers[*next]), length) }.to_vec())
        } else {
            // Frames that span buffers are too big for us anyway.
            Some(Vec::new())
        };
        // Give the descriptor back: clear its status, then move the tail
        // to it.
        // SAFETY: as above.
        unsafe { ptr::write_volatile(descriptor.add(12), 0) };
        self.write(RDT, *next as u32);
        *next = (*next + 1) % RX_COUNT;
        frame
    }
}

impl NetDevice for E1000 {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&self) -> bool {
        self.read(STATUS) & STATUS_LINK_UP != 0
    }

    fn transmit(&self, frame: &[u8]) {
        if frame.len() > BUFFER_SIZE {
            return;
        }
        let mut next = self.tx_next.lock();
        let descriptor = self.descriptor(self.tx_ring, *next);
        // SAFETY: a transmit descriptor: buffer at 0, length at 8, command
        // at 11, status at 12. A descriptor is free once the card has set
        // its done bit, or if it was never used.
        unsafe {
            let status = ptr::read_volatile(descriptor.add(12));
            let used = ptr::read_volatile(descriptor.cast::<u64>()) != 0;
            if used && status & DESCRIPTOR_DONE == 0 {
                // The ring is full: drop the frame, as a busy network would.
                return;
            }
            let buffer = self.tx_buffers[*next];
            ptr::copy_nonoverlapping(frame.as_ptr(), phys_to_virt(buffer), frame.len());
            ptr::write_volatile(descriptor.cast::<u64>(), buffer);
            ptr::write_volatile(descriptor.add(8).cast::<u16>(), frame.len() as u16);
            ptr::write_volatile(descriptor.add(11), COMMAND_END_OF_PACKET | COMMAND_INSERT_CRC | COMMAND_REPORT_STATUS);
            ptr::write_volatile(descriptor.add(12), 0);
        }
        *next = (*next + 1) % TX_COUNT;
        self.write(TDT, *next as u32);
    }
}
