//! Intel's e1000 family of Ethernet controllers (the 8254x and the 82574).
//! QEMU, VirtualBox and VMware all emulate one, and the design lives on in
//! Intel's later chips: the I217 and I218 built into Haswell and Broadwell
//! chipsets (the ThinkPad T440p has an I217-LM) use the same rings and
//! registers, with a separate PHY chip that needs some care at startup (see
//! the `pch_` functions).
//!
//! Packets move through two rings of 16-byte descriptors in memory, one for
//! receiving and one for transmitting. Each descriptor points at a buffer.
//! The driver hands descriptors to the card by moving a ring's tail
//! register; the card hands them back by setting their "descriptor done"
//! bit.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::pci;
use crate::arch::{apic, interrupts};
use crate::memory::{self, CacheType, PAGE_SIZE, frames, phys_to_virt};
use crate::net::{self, NetDevice};
use crate::sync::SpinLock;
use crate::task::{self, WaitQueue};

const CTRL: u64 = 0x0000;
const STATUS: u64 = 0x0008;
const EERD: u64 = 0x0014;
const CTRL_EXT: u64 = 0x0018;
const FEXTNVM: u64 = 0x0028;
const MDIC: u64 = 0x0020;
const FEXTNVM3: u64 = 0x003C;
const EXTCNF_CTRL: u64 = 0x0F00;
const EXTCNF_SIZE: u64 = 0x0F08;
const PHY_CTRL: u64 = 0x0F10;
const PBECCSTS: u64 = 0x100C;
const KABGTXD: u64 = 0x3004;
const TXDCTL0: u64 = 0x3828;
const TARC0: u64 = 0x3840;
const TXDCTL1: u64 = 0x3928;
const TARC1: u64 = 0x3940;
const RFCTL: u64 = 0x5008;
const FWSM: u64 = 0x5B54;
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
/// Statistics counters (cleared when read): CRC errors, missed packets,
/// good packets received and transmitted.
const CRCERRS: u64 = 0x4000;
const MPC: u64 = 0x4010;
const GPRC: u64 = 0x4074;
const GPTC: u64 = 0x4080;

const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
const CTRL_SET_LINK_UP: u32 = 1 << 6;
const CTRL_FORCE_SPEED: u32 = 1 << 11;
const CTRL_FORCE_DUPLEX: u32 = 1 << 12;
const CTRL_LANPHYPC_OVERRIDE: u32 = 1 << 16;
const CTRL_LANPHYPC_VALUE: u32 = 1 << 17;
const CTRL_MEMORY_ERROR_HANDLING: u32 = 1 << 19;
const CTRL_RESET: u32 = 1 << 26;
const CTRL_PHY_RESET: u32 = 1 << 31;
const STATUS_FULL_DUPLEX: u32 = 1 << 0;
const STATUS_LINK_UP: u32 = 1 << 1;
const STATUS_LAN_INIT_DONE: u32 = 1 << 9;
const STATUS_PHY_RESET_ASSERTED: u32 = 1 << 10;
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;
const CTRL_EXT_PHY_POWER_CYCLE_DONE: u32 = 1 << 2;
const CTRL_EXT_FORCE_SMBUS: u32 = 1 << 11;
const CTRL_EXT_PHY_POWER_DOWN: u32 = 1 << 20;
const EXTCNF_CTRL_SOFTWARE_FLAG: u32 = 1 << 5;
/// Software, not the hardware, applies the PHY's configuration from the
/// NVM after a reset.
const FEXTNVM_SOFTWARE_CONFIG: u32 = 1 << 27;
const PHY_CTRL_D0_LOW_POWER_LINK: u32 = 1 << 1;
const PHY_CTRL_GIGABIT_DISABLE: u32 = 1 << 6;
/// The management engine's firmware is running (and owns some of the PHY).
const FWSM_FIRMWARE_VALID: u32 = 1 << 15;
/// Clear while the management engine forbids resetting the PHY.
const FWSM_PHY_RESET_ALLOWED: u32 = 1 << 6;
const MDIC_READ: u32 = 2 << 26;
const MDIC_WRITE: u32 = 1 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
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

/// The chipset (PCH) controllers: I217-LM and -V (Lynx Point), I218-LM and
/// -V (Lynx Point LP and Wildcat Point).
const PCH_DEVICE_IDS: [u16; 8] = [0x153A, 0x153B, 0x155A, 0x1559, 0x15A0, 0x15A1, 0x15A2, 0x15A3];

/// PHY registers (page, register). The PHY answers at MDIO address 2, or
/// 1 for pages from 768 up.
const PHY_CONTROL: (u16, u8) = (0, 0);
const PHY_ID1: (u16, u8) = (0, 2);
const PHY_ID2: (u16, u8) = (0, 3);
const PHY_AUTONEG_ADVERTISE: (u16, u8) = (0, 4);
const PHY_STATUS: (u16, u8) = (0, 1);
const PHY_PARTNER_ABILITY: (u16, u8) = (0, 5);
const PHY_1000T_CONTROL: (u16, u8) = (0, 9);
const PHY_1000T_STATUS: (u16, u8) = (0, 10);
/// The I217's own status register: speed, duplex and link as resolved.
const PHY_STATUS_2: (u16, u8) = (0, 26);
/// The OEM bits: gigabit disable (bit 6), low-power link up (bit 2) and
/// restart negotiation (bit 10).
const PHY_OEM_BITS: (u16, u8) = (768, 25);
/// Port general configuration; bit 4 is "host wakeup".
const PHY_PORT_GENERAL: (u16, u8) = (769, 17);
/// SMBus control; bit 0 forces the PHY's side of the link to SMBus.
const PHY_SMBUS_CONTROL: (u16, u8) = (769, 23);

static RECEIVED: WaitQueue = WaitQueue::new();
static USES_INTERRUPTS: AtomicBool = AtomicBool::new(false);
static INTERRUPTS_SEEN: AtomicU32 = AtomicU32::new(0);

/// How long the receive thread sleeps between checks when it relies on
/// interrupts (in case they never come) and when it polls, in ms.
const INTERRUPT_FALLBACK_MS: u64 = 100;
const POLL_MS: u64 = 2;
/// How often the link is checked, and for how long after startup the
/// driver logs its state, in ms (real hardware can't be debugged otherwise).
const LINK_CHECK_MS: u64 = 1000;
const POLLED_FRAMES_WARNING: u32 = 10;
const DIAGNOSTICS_MS: u64 = 120_000;
const DIAGNOSTICS_EVERY_MS: u64 = 5000;

pub struct E1000 {
    registers: u64,
    pch: bool,
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
    let supported =
        |d: &&pci::Device| d.vendor == 0x8086 && (DEVICE_IDS.contains(&d.device) || PCH_DEVICE_IDS.contains(&d.device));
    for device in devices.iter().filter(supported) {
        let Some(base) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        match E1000::new(base, PCH_DEVICE_IDS.contains(&device.device)) {
            Ok(nic) => {
                let nic = Arc::new(nic);
                if device.enable_msi(apic::local_id(), NIC_VECTOR) {
                    interrupts::set_handler(NIC_VECTOR, |_| {
                        INTERRUPTS_SEEN.fetch_add(1, Ordering::Relaxed);
                        RECEIVED.wake_all();
                    });
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
                net::add_interface("eth0", nic);
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
    fn new(base: u64, pch: bool) -> Result<Self, &'static str> {
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
            pch,
            mac: [0; 6],
            rx_ring: dma_pages(1)?,
            tx_ring: dma_pages(1)?,
            rx_buffers: buffers(RX_COUNT)?,
            tx_buffers: buffers(TX_COUNT)?,
            rx_next: SpinLock::new(0),
            tx_next: SpinLock::new(0),
        };

        nic.write(IMC, u32::MAX);
        if pch {
            nic.pch_reset();
        } else {
            nic.write(CTRL, nic.read(CTRL) | CTRL_RESET);
            task::sleep_ms(10);
        }
        nic.write(IMC, u32::MAX);
        nic.read(ICR);
        if pch {
            nic.pch_set_required_bits();
        }
        nic.write(CTRL, (nic.read(CTRL) | CTRL_SET_LINK_UP) & !(CTRL_FORCE_SPEED | CTRL_FORCE_DUPLEX));
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
        if pch {
            nic.pch_start_autonegotiation();
        }
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

    /// Hands received packets to the network stack, forever, and watches
    /// the link. Even with interrupts it wakes now and then, so a machine
    /// whose interrupts don't arrive still receives (slowly), and says so.
    fn receive_loop(&self) {
        let start = crate::time::ticks();
        let mut link = None;
        let (mut next_link_check, mut next_diagnostics) = (start, start);
        let (mut received, mut found_by_polling) = (0u32, 0u32);
        loop {
            let interrupts_before = INTERRUPTS_SEEN.load(Ordering::Relaxed);
            while let Some(frame) = self.next_frame() {
                received += 1;
                net::receive(self.mac, frame);
            }
            // Reading ICR acknowledges the interrupt.
            self.read(ICR);

            let now = crate::time::ticks();
            if now >= next_link_check {
                next_link_check = now + LINK_CHECK_MS;
                let up = self.link_up();
                if link != Some(up) {
                    self.log_link(up);
                    link = Some(up);
                    next_diagnostics = now;
                }
            }
            if self.pch && now >= next_diagnostics && now - start < DIAGNOSTICS_MS {
                next_diagnostics = now + DIAGNOSTICS_EVERY_MS;
                self.log_diagnostics(received, found_by_polling);
            }

            let interrupts = USES_INTERRUPTS.load(Ordering::Relaxed);
            let wait = if interrupts { INTERRUPT_FALLBACK_MS } else { POLL_MS };
            RECEIVED.wait_until_deadline(Some(crate::time::ticks() + wait), || self.frame_ready().then_some(()));
            if interrupts && INTERRUPTS_SEEN.load(Ordering::Relaxed) == interrupts_before && self.frame_ready() {
                // An interrupt can trail its packet a little, so only a
                // pattern means interrupts are lost.
                found_by_polling += 1;
                if found_by_polling == POLLED_FRAMES_WARNING {
                    warn!("net: e1000: packets are arriving without interrupts; polling for them");
                }
            }
        }
    }

    fn log_link(&self, up: bool) {
        if !up {
            info!("net: e1000: link down");
            return;
        }
        let status = self.read(STATUS);
        let speed = match status >> 6 & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        };
        let duplex = if status & STATUS_FULL_DUPLEX != 0 { "full" } else { "half" };
        info!("net: e1000: link up, {speed} Mbit/s, {duplex} duplex");
    }

    /// One line with everything that says why packets do or don't flow:
    /// the MAC's view of the link, the PHY's (status is read twice, since
    /// its link bit latches low), the packet counters and interrupts.
    fn log_diagnostics(&self, received: u32, found_by_polling: u32) {
        let phy = |register| self.pch_phy_read(register).map_or(-1, i32::from);
        let _ = self.pch_phy_read(PHY_STATUS);
        debug!(
            "net: e1000 PCH: STATUS {:#x} CTRL {:#x} | PHY control {:#x} status {:#x} partner {:#x} 1000T {:#x} \
             status2 {:#x} OEM {:#x} | received {} sent {} CRC errors {} missed {} | frames {received}, \
             interrupts {}, found by polling {found_by_polling}",
            self.read(STATUS),
            self.read(CTRL),
            phy(PHY_CONTROL),
            phy(PHY_STATUS),
            phy(PHY_PARTNER_ABILITY),
            phy(PHY_1000T_STATUS),
            phy(PHY_STATUS_2),
            phy(PHY_OEM_BITS),
            self.read(GPRC),
            self.read(GPTC),
            self.read(CRCERRS),
            self.read(MPC),
            INTERRUPTS_SEEN.load(Ordering::Relaxed),
        );
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

/// The chipset controllers. The MAC is in the chipset and the PHY (the part
/// that talks to the cable) is a separate chip, connected by a fast link
/// for packets and a slow one (SMBus) the management engine uses while the
/// computer sleeps. Firmware can leave the PHY on the slow link or powered
/// down, and it then ignores the MAC; these functions get it back, reset
/// both together, and set the bits Intel's documentation requires. The
/// steps follow Intel's description of the hardware as Linux's e1000e
/// driver uses it.
impl E1000 {
    fn pch_reset(&self) {
        debug!(
            "net: e1000 PCH: before reset: CTRL {:#x} CTRL_EXT {:#x} STATUS {:#x} FWSM {:#x} EXTCNF {:#x} \
             EXTCNF_SIZE {:#x} FEXTNVM {:#x} PHY_CTRL {:#x}",
            self.read(CTRL),
            self.read(CTRL_EXT),
            self.read(STATUS),
            self.read(FWSM),
            self.read(EXTCNF_CTRL),
            self.read(EXTCNF_SIZE),
            self.read(FEXTNVM),
            self.read(PHY_CTRL)
        );
        self.pch_make_phy_accessible();

        // Stop DMA, so the reset doesn't land in the middle of a transfer.
        self.write(CTRL, self.read(CTRL) | CTRL_GIO_MASTER_DISABLE);
        for _ in 0..800 {
            if self.read(STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                break;
            }
            crate::time::delay_us(100);
        }
        self.write(RCTL, 0);
        self.write(TCTL, TCTL_PAD_SHORT);
        task::sleep_ms(10);

        // The MAC and PHY are reset together, so the link between them is
        // too, unless the management engine forbids it.
        let reset_phy = !self.pch_phy_reset_blocked();
        let control = self.read(CTRL) | if reset_phy { CTRL_PHY_RESET } else { 0 };
        let flagged = self.pch_acquire_software_flag();
        self.write(CTRL, control | CTRL_RESET);
        task::sleep_ms(20);
        if flagged && self.read(EXTCNF_CTRL) & EXTCNF_CTRL_SOFTWARE_FLAG != 0 {
            self.pch_release_software_flag();
        }

        if reset_phy {
            task::sleep_ms(10);
            // The PHY loads its configuration from flash after a reset.
            let mut done = false;
            for _ in 0..1500 {
                if self.read(STATUS) & STATUS_LAN_INIT_DONE != 0 {
                    done = true;
                    break;
                }
                crate::time::delay_us(150);
            }
            if !done {
                warn!("net: e1000 PCH: the PHY didn't finish configuring itself");
            }
            let status = self.read(STATUS);
            self.write(STATUS, status & !(STATUS_LAN_INIT_DONE | STATUS_PHY_RESET_ASSERTED));
            task::sleep_ms(10);
            // A reset can leave the PHY's "wake the host" bit set.
            if let Some(value) = self.pch_phy_read(PHY_PORT_GENERAL) {
                self.pch_phy_write(PHY_PORT_GENERAL, value & !(1 << 4));
            }
            self.pch_configure_oem_bits();
        }
        // Analog front end bias, as Intel specifies.
        self.write(KABGTXD, self.read(KABGTXD) | 0x0005_0000);
        debug!(
            "net: e1000 PCH: after reset{}: CTRL {:#x} CTRL_EXT {:#x} STATUS {:#x}",
            if reset_phy { " (with PHY)" } else { " (PHY reset blocked)" },
            self.read(CTRL),
            self.read(CTRL_EXT),
            self.read(STATUS)
        );
    }

    /// Makes sure the PHY answers on the fast link: if it doesn't, tries
    /// the slow (SMBus) mode, then power-cycling it with the LANPHYPC pin.
    fn pch_make_phy_accessible(&self) {
        if self.pch_phy_accessible() {
            return;
        }
        debug!("net: e1000 PCH: PHY not answering; trying SMBus mode");
        self.write(CTRL_EXT, self.read(CTRL_EXT) | CTRL_EXT_FORCE_SMBUS);
        task::sleep_ms(50);
        if self.pch_phy_accessible() {
            return;
        }
        if self.pch_phy_reset_blocked() {
            warn!("net: e1000 PCH: the PHY isn't answering and the management engine blocks resetting it");
            return;
        }
        debug!("net: e1000 PCH: power-cycling the PHY");
        self.pch_toggle_lanphypc();
        if self.pch_phy_accessible() {
            return;
        }
        self.write(CTRL_EXT, self.read(CTRL_EXT) & !CTRL_EXT_FORCE_SMBUS);
        if !self.pch_phy_accessible() {
            warn!("net: e1000 PCH: the PHY isn't answering");
        }
    }

    /// Whether the PHY answers with an ID. If it does and the management
    /// engine isn't running, takes both sides out of SMBus mode.
    fn pch_phy_accessible(&self) -> bool {
        let id = match (self.pch_phy_read(PHY_ID1), self.pch_phy_read(PHY_ID2)) {
            (Some(high), Some(low)) if high != 0xFFFF && low != 0xFFFF => u32::from(high) << 16 | u32::from(low),
            _ => return false,
        };
        debug!("net: e1000 PCH: PHY ID {id:#010x}");
        if self.read(FWSM) & FWSM_FIRMWARE_VALID == 0 {
            if let Some(value) = self.pch_phy_read(PHY_SMBUS_CONTROL) {
                self.pch_phy_write(PHY_SMBUS_CONTROL, value & !1);
            }
            self.write(CTRL_EXT, self.read(CTRL_EXT) & !CTRL_EXT_FORCE_SMBUS);
        }
        true
    }

    /// Power-cycles the PHY by driving the LANPHYPC pin low briefly.
    fn pch_toggle_lanphypc(&self) {
        // Give the PHY 50 ms to configure itself afterwards.
        self.write(FEXTNVM3, self.read(FEXTNVM3) & !0x0C00_0000 | 0x0800_0000);
        let control = (self.read(CTRL) | CTRL_LANPHYPC_OVERRIDE) & !CTRL_LANPHYPC_VALUE;
        self.write(CTRL, control);
        self.read(STATUS);
        crate::time::delay_us(20);
        self.write(CTRL, control & !CTRL_LANPHYPC_OVERRIDE);
        self.read(STATUS);
        for _ in 0..20 {
            if self.read(CTRL_EXT) & CTRL_EXT_PHY_POWER_CYCLE_DONE != 0 {
                break;
            }
            task::sleep_ms(5);
        }
        task::sleep_ms(30);
    }

    /// Whether the management engine forbids a PHY reset (waiting up to
    /// 300 ms for it to allow one).
    fn pch_phy_reset_blocked(&self) -> bool {
        for _ in 0..30 {
            if self.read(FWSM) & FWSM_PHY_RESET_ALLOWED != 0 {
                return false;
            }
            task::sleep_ms(10);
        }
        true
    }

    /// Takes the semaphore that keeps the driver, the management engine
    /// and the hardware from using the PHY at the same time.
    fn pch_acquire_software_flag(&self) -> bool {
        for _ in 0..100 {
            if self.read(EXTCNF_CTRL) & EXTCNF_CTRL_SOFTWARE_FLAG == 0 {
                break;
            }
            crate::time::delay_us(1000);
        }
        self.write(EXTCNF_CTRL, self.read(EXTCNF_CTRL) | EXTCNF_CTRL_SOFTWARE_FLAG);
        for _ in 0..1000 {
            if self.read(EXTCNF_CTRL) & EXTCNF_CTRL_SOFTWARE_FLAG != 0 {
                return true;
            }
            crate::time::delay_us(1000);
        }
        debug!("net: e1000 PCH: can't take the PHY semaphore (FWSM {:#x})", self.read(FWSM));
        self.pch_release_software_flag();
        false
    }

    fn pch_release_software_flag(&self) {
        self.write(EXTCNF_CTRL, self.read(EXTCNF_CTRL) & !EXTCNF_CTRL_SOFTWARE_FLAG);
    }

    fn pch_phy_read(&self, (page, register): (u16, u8)) -> Option<u16> {
        self.pch_phy_access(page, register, None)
    }

    fn pch_phy_write(&self, (page, register): (u16, u8), value: u16) {
        let _ = self.pch_phy_access(page, register, Some(value));
    }

    /// Reads or writes a PHY register through MDIC, selecting its page
    /// first for registers above 15 (the ones that are paged).
    fn pch_phy_access(&self, page: u16, register: u8, value: Option<u16>) -> Option<u16> {
        if !self.pch_acquire_software_flag() {
            return None;
        }
        let address = if page >= 768 { 1 } else { 2 };
        let page = if page == 768 { 0 } else { page };
        let result = if register > 15 && self.mdic(address, 31, Some(page << 5)).is_none() {
            None
        } else {
            self.mdic(address, register, value)
        };
        self.pch_release_software_flag();
        result
    }

    /// One MDIO transaction with the PHY.
    fn mdic(&self, address: u32, register: u8, value: Option<u16>) -> Option<u16> {
        let operation = match value {
            Some(value) => MDIC_WRITE | u32::from(value),
            None => MDIC_READ,
        };
        self.write(MDIC, operation | u32::from(register) << 16 | address << 21);
        for _ in 0..2000 {
            crate::time::delay_us(50);
            let result = self.read(MDIC);
            if result & MDIC_READY != 0 {
                return (result & MDIC_ERROR == 0).then_some(result as u16);
            }
        }
        None
    }

    /// After a PHY reset, when the NVM says software configures the PHY,
    /// copies the MAC's "gigabit disable" and "low-power link" settings
    /// into the PHY's OEM bits, which a reset leaves however the PHY likes.
    fn pch_configure_oem_bits(&self) {
        if self.read(FEXTNVM) & FEXTNVM_SOFTWARE_CONFIG == 0 {
            return;
        }
        let Some(old) = self.pch_phy_read(PHY_OEM_BITS) else { return };
        let mac = self.read(PHY_CTRL);
        let mut value = old & !(1 << 6 | 1 << 2);
        if mac & PHY_CTRL_GIGABIT_DISABLE != 0 {
            value |= 1 << 6;
        }
        if mac & PHY_CTRL_D0_LOW_POWER_LINK != 0 {
            value |= 1 << 2;
        }
        self.pch_phy_write(PHY_OEM_BITS, value | 1 << 10);
        debug!("net: e1000 PCH: OEM bits {old:#x} -> {value:#x} (PHY_CTRL {mac:#x})");
    }

    /// Bits Intel's documentation says must be set after a reset.
    fn pch_set_required_bits(&self) {
        self.write(CTRL_EXT, self.read(CTRL_EXT) | 1 << 22 | CTRL_EXT_PHY_POWER_DOWN);
        self.write(TXDCTL0, self.read(TXDCTL0) | 1 << 22);
        self.write(TXDCTL1, self.read(TXDCTL1) | 1 << 22);
        self.write(TARC0, self.read(TARC0) | 1 << 23 | 1 << 24 | 1 << 26 | 1 << 27);
        // Bit 28 of TCTL is "multiple requests", which isn't used.
        self.write(TARC1, self.read(TARC1) | 1 << 24 | 1 << 26 | 1 << 28 | 1 << 30);
        // Turn off NFS filtering, which can corrupt descriptors.
        self.write(RFCTL, self.read(RFCTL) | 1 << 6 | 1 << 7);
        // Error correction for the packet buffer.
        self.write(PBECCSTS, self.read(PBECCSTS) | 1 << 16);
        self.write(CTRL, self.read(CTRL) | CTRL_MEMORY_ERROR_HANDLING);
    }

    /// Advertises 10, 100 and 1000 Mbit/s and (re)starts negotiating a
    /// speed with the other end of the cable.
    fn pch_start_autonegotiation(&self) {
        let (Some(advertise), Some(gigabit), Some(control)) = (
            self.pch_phy_read(PHY_AUTONEG_ADVERTISE),
            self.pch_phy_read(PHY_1000T_CONTROL),
            self.pch_phy_read(PHY_CONTROL),
        ) else {
            warn!("net: e1000 PCH: can't reach the PHY to start the link");
            return;
        };
        self.pch_phy_write(PHY_AUTONEG_ADVERTISE, advertise | 0x01E0);
        self.pch_phy_write(PHY_1000T_CONTROL, gigabit | 1 << 9);
        // Autonegotiation on (bit 12) and restart (bit 9).
        self.pch_phy_write(PHY_CONTROL, control | 1 << 12 | 1 << 9);
        debug!("net: e1000 PCH: negotiating the link (PHY control was {control:#x})");
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
