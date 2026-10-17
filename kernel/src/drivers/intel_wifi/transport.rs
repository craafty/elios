//! The PCIe side of Intel's 7000-series Wi-Fi cards: registers, power-up,
//! loading firmware, and the queues commands and received packets travel
//! through.
//!
//! - Registers ("CSR") are memory-mapped through BAR 0. Registers deeper in
//!   the card ("periphery", PRPH) and its memory are reached indirectly,
//!   through address/data register pairs, and only while the driver holds
//!   the card awake ("NIC access").
//! - Firmware is copied into the card's memory by its DMA engine's
//!   "service channel", one chunk (up to 128 KiB) at a time.
//! - The card puts what it receives (command responses, notifications,
//!   and later network packets) into 4 KiB buffers from a ring of 256
//!   descriptors, and writes its progress to a status block.
//! - Commands go through transmit queue 0: a ring of 256 "TFDs", each
//!   pointing at up to 20 buffers. The card's scheduler picks them up when
//!   the write pointer moves.
//!
//! This driver polls instead of using interrupts while the card is brought
//! up: the interrupt status register is read and acknowledged directly.
//! The register names and steps follow Intel's hardware as Linux's iwlwifi
//! driver documents it.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ptr;

use super::commands::TxFrame;
use super::firmware::{SEPARATOR_CPU1_CPU2, SEPARATOR_PAGING, Section};
use crate::drivers::dma::DmaBuffer;
use crate::memory::{self, CacheType, PAGE_SIZE};
use crate::{task, time};

// Registers.
const CSR_HW_IF_CONFIG: u32 = 0x000;
const CSR_INT_COALESCING: u32 = 0x004;
const CSR_INT: u32 = 0x008;
const CSR_INT_MASK: u32 = 0x00C;
const CSR_FH_INT_STATUS: u32 = 0x010;
const CSR_RESET: u32 = 0x020;
const CSR_GP_CNTRL: u32 = 0x024;
const CSR_HW_REV: u32 = 0x028;
const CSR_GIO: u32 = 0x03C;
const CSR_UCODE_DRV_GP1_CLR: u32 = 0x05C;
const CSR_MBOX_SET: u32 = 0x088;
const CSR_MAC_SHADOW_REG_CTRL: u32 = 0x0A8;
const CSR_GIO_CHICKEN_BITS: u32 = 0x100;
const CSR_DBG_HPET_MEM: u32 = 0x240;
const CSR_DBG_LINK_PWR_MGMT: u32 = 0x250;
const HBUS_TARG_MEM_RADDR: u32 = 0x40C;
const HBUS_TARG_MEM_WADDR: u32 = 0x410;
const HBUS_TARG_MEM_WDAT: u32 = 0x418;
const HBUS_TARG_MEM_RDAT: u32 = 0x41C;
const HBUS_TARG_PRPH_WADDR: u32 = 0x444;
const HBUS_TARG_PRPH_RADDR: u32 = 0x448;
const HBUS_TARG_PRPH_WDAT: u32 = 0x44C;
const HBUS_TARG_PRPH_RDAT: u32 = 0x450;
const HBUS_TARG_WRPTR: u32 = 0x460;

// The DMA engine ("flow handler").
const FH_TFDIB_CTRL0_SERVICE: u32 = 0x1900 + 0x8 * 9;
const FH_TFDIB_CTRL1_SERVICE: u32 = 0x1900 + 0x8 * 9 + 4;
const FH_KW_MEM_ADDR: u32 = 0x197C;
const FH_SRVC_CHNL_SRAM_ADDR: u32 = 0x19C8;
const FH_RSCSR_CHNL0_STTS_WPTR: u32 = 0x1BC0;
const FH_RSCSR_CHNL0_RBDCB_BASE: u32 = 0x1BC4;
const FH_RSCSR_CHNL0_WPTR: u32 = 0x1BC8;
const FH_RSCSR_CHNL0_RDPTR: u32 = 0x1BCC;
const FH_MEM_RCSR_CHNL0_CONFIG: u32 = 0x1C00;
const FH_MEM_RCSR_CHNL0_RBDCB_WPTR: u32 = 0x1C08;
const FH_MEM_RCSR_CHNL0_FLUSH_RB_REQ: u32 = 0x1C10;
const FH_MEM_RSSR_RX_STATUS: u32 = 0x1C44;
const FH_TX_CHICKEN_BITS: u32 = 0x1E98;
const FH_TSSR_TX_STATUS: u32 = 0x1EB0;

const fn fh_tcsr_config(channel: u32) -> u32 {
    0x1D00 + 0x20 * channel
}

const fn fh_tcsr_buffer_status(channel: u32) -> u32 {
    0x1D00 + 0x20 * channel + 0x8
}

/// Where queue `queue`'s TFD ring address goes.
const fn fh_queue_base(queue: u32) -> u32 {
    match queue {
        0..16 => 0x19D0 + 4 * queue,
        16..20 => 0x1BF0 + 4 * (queue - 16),
        _ => 0x1B20 + 4 * (queue - 20),
    }
}

// Periphery registers.
const APMG_CLK_EN: u32 = 0x3004;
const APMG_CLK_DIS: u32 = 0x3008;
const APMG_PS_CTRL: u32 = 0x300C;
const APMG_PCIDEV_STT: u32 = 0x3010;
const APMG_RTC_INT_STT: u32 = 0x301C;
const OSC_CLK: u32 = 0xA0_4068;
const LMPM_CHICK: u32 = 0xA0_1FF8;
const SCD_BASE: u32 = 0xA0_2C00;
const SCD_SRAM_BASE_ADDR: u32 = SCD_BASE;
const SCD_DRAM_BASE_ADDR: u32 = SCD_BASE + 0x8;
const SCD_TXFACT: u32 = SCD_BASE + 0x10;
const SCD_AGGR_SEL: u32 = SCD_BASE + 0x248;
const SCD_GP_CTRL: u32 = SCD_BASE + 0x1A8;
const SCD_EN_CTRL: u32 = SCD_BASE + 0x254;

const fn scd_queue_read_pointer(queue: u32) -> u32 {
    if queue < 20 { SCD_BASE + 0x68 + queue * 4 } else { SCD_BASE + 0x2B4 + queue * 4 }
}

const fn scd_queue_status(queue: u32) -> u32 {
    if queue < 20 { SCD_BASE + 0x10C + queue * 4 } else { SCD_BASE + 0x334 + (queue - 20) * 4 }
}

// Offsets in the scheduler's part of the card's memory.
const SCD_CONTEXT_MEM: u32 = 0x600;
const SCD_TRANS_TBL_END: u32 = (0x7E0 + QUEUE_COUNT * 2) & 0xFFFC;

// Bits.
const HW_IF_CONFIG_PCI_OWN_SET: u32 = 0x0040_0000;
const HW_IF_CONFIG_WAKE_ME: u32 = 0x0800_0000;
const HW_IF_CONFIG_HAP_WAKE: u32 = 0x0008_0000;
const HW_IF_CONFIG_RADIO_SI: u32 = 0x0000_0200;
const GP_CNTRL_MAC_CLOCK_READY: u32 = 0x1;
const GP_CNTRL_INIT_DONE: u32 = 0x4;
const GP_CNTRL_MAC_ACCESS_REQ: u32 = 0x8;
const GP_CNTRL_GOING_TO_SLEEP: u32 = 0x10;
/// Set while the radio is allowed on (no hardware kill switch engaged).
pub const GP_CNTRL_RF_KILL_OFF: u32 = 0x0800_0000;
const RESET_SW_RESET: u32 = 0x80;
const RESET_STOP_MASTER: u32 = 0x100;
const RESET_MASTER_DISABLED: u32 = 0x200;
const INT_FH_RX: u32 = 1 << 31;
const INT_HW_ERROR: u32 = 1 << 29;
const INT_FH_TX: u32 = 1 << 27;
const INT_SW_ERROR: u32 = 1 << 25;
const INT_RF_KILL: u32 = 1 << 7;
const INT_SW_RX: u32 = 1 << 3;
const INT_ALIVE: u32 = 1 << 0;
/// Interrupts the card may raise (enabled so their status bits are set;
/// PCI interrupts themselves stay off while polling).
const INT_MASK: u32 = INT_FH_RX | INT_HW_ERROR | INT_FH_TX | INT_SW_ERROR | INT_RF_KILL | INT_SW_RX | INT_ALIVE;
const FH_INT_TX_MASK: u32 = 0x3;

/// Transmit queues the card has, and the one commands go through.
const QUEUE_COUNT: u32 = 31;
pub const COMMAND_QUEUE: u32 = 0;
/// The transmit FIFO commands use.
const COMMAND_FIFO: u32 = 7;
const TFD_COUNT: usize = 256;
const TFD_SIZE: usize = 128;
/// The first buffer of a command TFD is at most this long (the card may
/// write to it).
const FIRST_BUFFER_LEN: usize = 20;
const COMMAND_HEADER_LEN: usize = 4;
const TX_PARAMETERS_LEN: usize = 56;
/// Byte count tables: 256 entries plus 64 duplicated, 2 bytes each.
const BYTE_COUNT_ENTRIES: usize = 320;

const RX_COUNT: usize = 256;
const RX_BUFFER_SIZE: usize = 4096;
const FRAME_INVALID: u32 = 0x5555_0000;

const FIRMWARE_CHUNK: usize = 0x2_0000;

/// A packet from the card: a command response or notification.
pub struct Packet {
    pub command: u8,
    pub group: u8,
    pub sequence: u16,
    pub data: Vec<u8>,
}

impl Packet {
    /// Whether the card sent it on its own rather than answering a
    /// command.
    pub fn is_notification(&self) -> bool {
        self.sequence & 0x8000 != 0
    }
}

#[derive(Debug)]
pub enum Error {
    /// The card didn't do something in time.
    Timeout(&'static str),
    /// The firmware reported an error or crashed.
    Firmware(&'static str),
    NoMemory,
    /// A transmit queue is full (the frame is dropped, as a busy network
    /// would).
    QueueFull,
}

/// A transmit queue: its TFD ring and a buffer per slot for what the TFDs
/// point at.
struct TxQueue {
    tfds: DmaBuffer,
    buffers: Vec<Option<DmaBuffer>>,
    write: usize,
    /// The oldest slot the card hasn't finished with (data queues only:
    /// commands are answered in order, one at a time).
    read: usize,
}

pub struct Transport {
    registers: u64,
    pub hw_revision: u32,
    keep_warm: DmaBuffer,
    byte_counts: DmaBuffer,
    queues: Vec<TxQueue>,
    rx_descriptors: DmaBuffer,
    rx_status: DmaBuffer,
    rx_buffers: Vec<DmaBuffer>,
    /// Which buffer each receive descriptor holds.
    rx_slot_buffer: [usize; RX_COUNT],
    rx_read: usize,
    rx_write: usize,
    /// Packets received but not yet asked for.
    pending: VecDeque<Packet>,
    scheduler_base: u32,
}

impl Transport {
    pub fn new(bar: u64) -> Result<Self, Error> {
        let registers = memory::map_device(bar, 0x2000, CacheType::Uncached) as u64;
        let dma = |pages, align| DmaBuffer::new(pages, align).ok_or(Error::NoMemory);
        let mut queues = Vec::new();
        for _ in 0..QUEUE_COUNT {
            let tfds = dma(TFD_COUNT * TFD_SIZE / PAGE_SIZE as usize, 1)?;
            queues.push(TxQueue { tfds, buffers: (0..TFD_COUNT).map(|_| None).collect(), write: 0, read: 0 });
        }
        let mut rx_buffers = Vec::new();
        for _ in 0..RX_COUNT {
            rx_buffers.push(dma(1, 1)?);
        }
        let byte_count_bytes = QUEUE_COUNT as usize * BYTE_COUNT_ENTRIES * 2;
        let mut transport = Self {
            registers,
            hw_revision: 0,
            keep_warm: dma(1, 1)?,
            byte_counts: dma(byte_count_bytes.div_ceil(PAGE_SIZE as usize), 1)?,
            queues,
            rx_descriptors: dma(1, 1)?,
            rx_status: dma(1, 1)?,
            rx_buffers,
            rx_slot_buffer: core::array::from_fn(|index| index),
            rx_read: 0,
            rx_write: 0,
            pending: VecDeque::new(),
            scheduler_base: 0,
        };
        transport.hw_revision = transport.read(CSR_HW_REV);
        Ok(transport)
    }

    pub fn read(&self, register: u32) -> u32 {
        // SAFETY: a register in the mapped BAR.
        unsafe { ptr::read_volatile((self.registers + u64::from(register)) as *const u32) }
    }

    pub fn write(&self, register: u32, value: u32) {
        // SAFETY: as for `read`.
        unsafe { ptr::write_volatile((self.registers + u64::from(register)) as *mut u32, value) };
    }

    fn set_bits(&self, register: u32, bits: u32) {
        self.write(register, self.read(register) | bits);
    }

    /// Waits up to `micros` for `register & mask == value`.
    fn poll(&self, register: u32, value: u32, mask: u32, micros: u64) -> bool {
        let deadline = time::uptime_nanos() + micros * 1000;
        loop {
            if self.read(register) & mask == value {
                return true;
            }
            if time::uptime_nanos() > deadline {
                return false;
            }
            time::delay_us(10);
        }
    }

    /// Keeps the card awake so its periphery and memory can be reached.
    /// The driver holds this the whole time while bringing the card up.
    fn grab_nic_access(&self) -> Result<(), Error> {
        self.set_bits(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_REQ);
        let mask = GP_CNTRL_MAC_CLOCK_READY | GP_CNTRL_GOING_TO_SLEEP;
        if self.poll(CSR_GP_CNTRL, GP_CNTRL_MAC_CLOCK_READY, mask, 15_000) {
            Ok(())
        } else {
            Err(Error::Timeout("waking the card"))
        }
    }

    pub fn read_prph(&self, address: u32) -> u32 {
        self.write(HBUS_TARG_PRPH_RADDR, (address & 0xF_FFFF) | 3 << 24);
        self.read(HBUS_TARG_PRPH_RDAT)
    }

    pub fn write_prph(&self, address: u32, value: u32) {
        self.write(HBUS_TARG_PRPH_WADDR, (address & 0xF_FFFF) | 3 << 24);
        self.write(HBUS_TARG_PRPH_WDAT, value);
    }

    fn set_bits_prph(&self, address: u32, bits: u32) {
        self.write_prph(address, self.read_prph(address) | bits);
    }

    fn clear_bits_prph(&self, address: u32, bits: u32) {
        self.write_prph(address, self.read_prph(address) & !bits);
    }

    /// Writes words to the card's memory (the address auto-increments).
    fn write_memory(&self, address: u32, words: &[u32]) {
        self.write(HBUS_TARG_MEM_WADDR, address);
        for &word in words {
            self.write(HBUS_TARG_MEM_WDAT, word);
        }
    }

    /// Reads words from the card's memory (for error logs).
    pub fn read_memory(&self, address: u32, count: usize) -> Vec<u32> {
        self.write(HBUS_TARG_MEM_RADDR, address);
        (0..count).map(|_| self.read(HBUS_TARG_MEM_RDAT)).collect()
    }

    /// Whether the radio may be on (no hardware kill switch engaged).
    pub fn radio_allowed(&self) -> bool {
        self.read(CSR_GP_CNTRL) & GP_CNTRL_RF_KILL_OFF != 0
    }

    /// Takes ownership of the card from the firmware (and, on business
    /// laptops, from Intel's management engine, which can use the card
    /// too).
    pub fn prepare(&self) -> Result<(), Error> {
        let ready = || {
            self.set_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_PCI_OWN_SET);
            let owned = self.poll(CSR_HW_IF_CONFIG, HW_IF_CONFIG_PCI_OWN_SET, HW_IF_CONFIG_PCI_OWN_SET, 50);
            if owned {
                // Tell the management engine the operating system is here.
                self.set_bits(CSR_MBOX_SET, 1 << 5);
            }
            owned
        };
        if ready() {
            return Ok(());
        }
        self.set_bits(CSR_DBG_LINK_PWR_MGMT, 1 << 31);
        time::delay_us(1000);
        for _ in 0..10 {
            self.set_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_WAKE_ME);
            let deadline = time::uptime_nanos() + 150_000_000;
            while time::uptime_nanos() < deadline {
                if ready() {
                    return Ok(());
                }
                time::delay_us(200);
            }
            task::sleep_ms(25);
        }
        Err(Error::Timeout("taking ownership of the card"))
    }

    /// Resets the card and powers up its basic functions ("APM").
    pub fn power_up(&self) -> Result<(), Error> {
        self.set_bits(CSR_RESET, RESET_SW_RESET);
        task::sleep_ms(6);
        self.prepare()?;

        // Link power management workarounds, as Intel specifies for this
        // family.
        self.set_bits(CSR_GIO_CHICKEN_BITS, 0x2000_0000 | 0x0080_0000);
        self.set_bits(CSR_DBG_HPET_MEM, 0xFFFF_0000);
        self.set_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_HAP_WAKE);
        // L0s is unreliable on these cards.
        self.set_bits(CSR_GIO, 0x2);

        self.set_bits(CSR_GP_CNTRL, GP_CNTRL_INIT_DONE);
        if !self.poll(CSR_GP_CNTRL, GP_CNTRL_MAC_CLOCK_READY, GP_CNTRL_MAC_CLOCK_READY, 25_000) {
            return Err(Error::Timeout("the card's clock"));
        }
        self.grab_nic_access()?;
        // 7260 and 3160: keep the oscillator counting wake-up time from L1
        // (the reads before and after are what the hardware wants).
        self.read_prph(OSC_CLK);
        self.read_prph(OSC_CLK);
        self.set_bits_prph(OSC_CLK, 0x8);
        self.read_prph(OSC_CLK);
        self.read_prph(OSC_CLK);
        // The DMA clock.
        self.write_prph(APMG_CLK_EN, 0x200);
        time::delay_us(20);
        // No L1-Active while setting up, and clear a pending kill switch
        // interrupt.
        self.set_bits_prph(APMG_PCIDEV_STT, 0x800);
        self.write_prph(APMG_RTC_INT_STT, 0x1000_0000);
        Ok(())
    }

    /// Tells the card about its radio, sets up the receive and transmit
    /// queues, and turns on the registers' shadow copies.
    pub fn initialize(&mut self, phy_config: u32) -> Result<(), Error> {
        // Power from the main supply.
        let power = self.read_prph(APMG_PS_CTRL) & !0x0300_0000;
        self.write_prph(APMG_PS_CTRL, power);

        // The radio's type, step and dash (from the firmware file), and the
        // MAC's step and dash (from the revision register).
        let radio = (phy_config & 0x3) << 10 | (phy_config >> 2 & 0x3) << 14 | (phy_config >> 4 & 0x3) << 12;
        let value = (self.hw_revision & 0xF) | radio | HW_IF_CONFIG_RADIO_SI;
        let mask = 0xF | 0xC00 | 0xC000 | 0x3000 | HW_IF_CONFIG_RADIO_SI | 0x100;
        let config = self.read(CSR_HW_IF_CONFIG);
        self.write(CSR_HW_IF_CONFIG, config & !mask | value);
        // Keep the card from getting stuck in reset after an early power
        // loss.
        self.set_bits_prph(APMG_PS_CTRL, 0x0040_0000);

        self.initialize_rx();
        self.initialize_tx();
        self.set_bits(CSR_MAC_SHADOW_REG_CTRL, 0x800F_FFFF);
        Ok(())
    }

    fn initialize_rx(&mut self) {
        self.write(FH_MEM_RCSR_CHNL0_CONFIG, 0);
        self.write(FH_MEM_RCSR_CHNL0_RBDCB_WPTR, 0);
        self.write(FH_MEM_RCSR_CHNL0_FLUSH_RB_REQ, 0);
        self.write(FH_RSCSR_CHNL0_RDPTR, 0);
        self.write(FH_RSCSR_CHNL0_WPTR, 0);
        self.write(FH_RSCSR_CHNL0_RBDCB_BASE, (self.rx_descriptors.phys >> 8) as u32);
        self.write(FH_RSCSR_CHNL0_STTS_WPTR, (self.rx_status.phys >> 4) as u32);
        // Enabled, ignoring "FIFO empty" (a hardware bug), interrupting the
        // host, 4 KiB buffers, a timeout of 0x11, 256 descriptors (2^8).
        self.write(FH_MEM_RCSR_CHNL0_CONFIG, 0x8000_0000 | 0x4 | 0x1000 | 0x11 << 4 | 8 << 20);
        self.write(CSR_INT_COALESCING, 0x40);
        // A workaround for interrupt coalescing on the 7260 and 3160.
        self.set_bits(CSR_INT_COALESCING, 1 << 31);

        // Hand the card every buffer but one (a full ring would look
        // empty), telling it in steps of 8.
        self.rx_read = 0;
        self.rx_write = 0;
        for slot in 0..RX_COUNT - 1 {
            self.rx_slot_buffer[slot] = slot;
            self.rx_descriptors.write32(slot * 4, (self.rx_buffers[slot].phys >> 8) as u32);
            self.rx_write = slot + 1;
        }
        self.rx_slot_buffer[RX_COUNT - 1] = RX_COUNT - 1;
        self.write(FH_RSCSR_CHNL0_WPTR, (self.rx_write & !7) as u32);
    }

    fn initialize_tx(&mut self) {
        self.write_prph(SCD_TXFACT, 0);
        self.write(FH_KW_MEM_ADDR, (self.keep_warm.phys >> 4) as u32);
        for index in 0..self.queues.len() {
            self.queues[index].write = 0;
            self.queues[index].read = 0;
            self.write(fh_queue_base(index as u32), (self.queues[index].tfds.phys >> 8) as u32);
        }
        // Automatic queue activation, and all 31 queues.
        self.set_bits_prph(SCD_GP_CTRL, 1 << 18 | 1 << 0);
    }

    /// Copies a firmware image's sections into the card and starts it. The
    /// firmware announces itself with an "alive" notification.
    pub fn load_firmware(&mut self, sections: &[Section]) -> Result<(), Error> {
        self.write(CSR_INT, 0xFFFF_FFFF);
        // Only "DMA done" while loading (status bits may need their
        // interrupt enabled to be set).
        self.write(CSR_INT_MASK, INT_FH_TX);
        self.write(CSR_UCODE_DRV_GP1_CLR, 0x2 | 0x4);
        let buffer = DmaBuffer::new(FIRMWARE_CHUNK / PAGE_SIZE as usize, 1).ok_or(Error::NoMemory)?;
        for section in sections {
            if section.address == SEPARATOR_CPU1_CPU2 || section.address == SEPARATOR_PAGING {
                break;
            }
            for (index, chunk) in section.data.chunks(FIRMWARE_CHUNK).enumerate() {
                let address = section.address + (index * FIRMWARE_CHUNK) as u32;
                // Addresses from 0x40000 to 0x57FFF need the extended
                // address space switched on while they're written.
                let extended = (0x4_0000..=0x5_7FFF).contains(&address);
                if extended {
                    self.set_bits_prph(LMPM_CHICK, 1);
                }
                buffer.write_bytes(0, chunk);
                let result = self.load_chunk(address, buffer.phys, chunk.len() as u32);
                if extended {
                    self.clear_bits_prph(LMPM_CHICK, 1);
                }
                result?;
            }
            debug!("wifi: loaded {} bytes at {:#x}", section.data.len(), section.address);
        }
        self.write(CSR_INT_MASK, INT_MASK);
        // Release the card's processor.
        self.write(CSR_RESET, 0);
        Ok(())
    }

    /// One DMA transfer through the service channel, waiting for it.
    fn load_chunk(&self, address: u32, phys: u64, len: u32) -> Result<(), Error> {
        const SERVICE: u32 = 9;
        self.write(fh_tcsr_config(SERVICE), 0);
        self.write(FH_SRVC_CHNL_SRAM_ADDR, address);
        self.write(FH_TFDIB_CTRL0_SERVICE, phys as u32);
        self.write(FH_TFDIB_CTRL1_SERVICE, ((phys >> 32) as u32 & 0xF) << 28 | len);
        // One buffer, valid.
        self.write(fh_tcsr_buffer_status(SERVICE), 1 << 20 | 1 << 12 | 0x3);
        // Enable, no credits, interrupt the host at the end.
        self.write(fh_tcsr_config(SERVICE), 0x8000_0000 | 0x0010_0000);
        let deadline = time::uptime_nanos() + 5_000_000_000;
        loop {
            let cause = self.read(CSR_INT);
            if cause & INT_FH_TX != 0 {
                self.write(CSR_INT, INT_FH_TX);
                self.write(CSR_FH_INT_STATUS, FH_INT_TX_MASK);
                return Ok(());
            }
            if cause & (INT_HW_ERROR | INT_SW_ERROR) != 0 {
                return Err(Error::Firmware("error while loading firmware"));
            }
            if time::uptime_nanos() > deadline {
                return Err(Error::Timeout("a firmware chunk"));
            }
            time::delay_us(20);
        }
    }

    /// After the firmware is alive: sets up the scheduler and the command
    /// queue, and starts the DMA channels.
    pub fn start_scheduler(&mut self) {
        self.scheduler_base = self.read_prph(SCD_SRAM_BASE_ADDR);
        let words = ((SCD_TRANS_TBL_END - SCD_CONTEXT_MEM) / 4) as usize;
        self.write_memory(self.scheduler_base + SCD_CONTEXT_MEM, &alloc::vec![0; words]);
        self.write_prph(SCD_DRAM_BASE_ADDR, (self.byte_counts.phys >> 10) as u32);

        // The command queue, on its FIFO, by itself in the scheduler.
        let queue = COMMAND_QUEUE;
        self.write_prph(SCD_EN_CTRL, 0);
        self.write_prph(scd_queue_status(queue), 1 << 19);
        self.clear_bits_prph(SCD_AGGR_SEL, 1 << queue);
        self.queues[queue as usize].write = 0;
        self.write(HBUS_TARG_WRPTR, queue << 8);
        self.write_prph(scd_queue_read_pointer(queue), 0);
        let context = self.scheduler_base + SCD_CONTEXT_MEM + queue * 8;
        // Window size and frame limit 64.
        self.write_memory(context, &[0, 64 | 64 << 16]);
        // Active, on its FIFO, "window size limit" on.
        self.write_prph(scd_queue_status(queue), 1 << 3 | COMMAND_FIFO | 1 << 4 | 0x017F_0000);
        self.write_prph(SCD_EN_CTRL, 1 << queue);

        self.write_prph(SCD_TXFACT, 0xFF);
        for channel in 0..8 {
            self.write(fh_tcsr_config(channel), 0x8000_0000 | 0x8);
        }
        self.set_bits(FH_TX_CHICKEN_BITS, 0x2);
        self.clear_bits_prph(APMG_PCIDEV_STT, 0x800);
    }

    /// Queues a command (legacy header for group 0, wide header otherwise)
    /// and returns its sequence number.
    pub fn send_command(&mut self, command: u8, group: u8, version: u8, payload: &[u8]) -> Result<u16, Error> {
        let queue_index = COMMAND_QUEUE as usize;
        let slot = self.queues[queue_index].write;
        let sequence = (COMMAND_QUEUE as u16) << 8 | slot as u16;
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        if group == 0 {
            bytes.extend_from_slice(&[command, 0]);
            bytes.extend_from_slice(&sequence.to_le_bytes());
        } else {
            bytes.extend_from_slice(&[command, group]);
            bytes.extend_from_slice(&sequence.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&[0, version]);
        }
        bytes.extend_from_slice(payload);
        if bytes.len() > PAGE_SIZE as usize {
            return Err(Error::Firmware("command too long"));
        }

        let queue = &mut self.queues[queue_index];
        if queue.buffers[slot].is_none() {
            queue.buffers[slot] = Some(DmaBuffer::new(1, 1).ok_or(Error::NoMemory)?);
        }
        let buffer = queue.buffers[slot].as_ref().expect("allocated above");
        buffer.write_bytes(0, &bytes);
        let first = bytes.len().min(FIRST_BUFFER_LEN);
        let mut pieces = alloc::vec![(buffer.phys, first)];
        if bytes.len() > first {
            pieces.push((buffer.phys + first as u64, bytes.len() - first));
        }
        write_tfd(&queue.tfds, slot, &pieces);
        queue.write = (slot + 1) % TFD_COUNT;
        let write = queue.write as u32;
        self.write(HBUS_TARG_WRPTR, write | COMMAND_QUEUE << 8);
        Ok(sequence)
    }

    /// Readies data queue `queue` for frames starting at sequence number
    /// `ssn`, and returns the sequence number to tell the firmware (with
    /// the SCD_QUEUE_CFG command, which connects it to a station).
    pub fn enable_queue(&mut self, queue: u32, ssn: u16) -> u16 {
        let state = &mut self.queues[queue as usize];
        let mut ssn = ssn;
        // The 7000 family's scheduler gets stuck if its write pointer
        // jumps by a multiple of 64; Linux steps over this the same way.
        let write = state.write as u16;
        if ssn.wrapping_sub(write) & 0x3F == 0 && ssn != write {
            ssn += 1;
        }
        state.write = usize::from(ssn & 0xFF);
        state.read = state.write;
        self.write(HBUS_TARG_WRPTR, u32::from(ssn & 0xFF) | queue << 8);
        ssn
    }

    /// Queues an 802.11 frame (`header` then `body`) on a data queue,
    /// behind a TX command built from `frame` (its length and scratch
    /// address are filled in here). Returns the slot it took.
    pub fn transmit(&mut self, queue: u32, mut frame: TxFrame, header: &[u8], body: &[u8]) -> Result<usize, Error> {
        let queue_index = queue as usize;
        let slot = self.queues[queue_index].write;
        if (slot + 1) % TFD_COUNT == self.queues[queue_index].read {
            return Err(Error::QueueFull);
        }
        // The command header and TX parameters, then the 802.11 header
        // padded to 4 bytes, then the body, all in one page.
        let header_end = (COMMAND_HEADER_LEN + TX_PARAMETERS_LEN + header.len()).next_multiple_of(4);
        if header_end + body.len() > PAGE_SIZE as usize {
            return Err(Error::Firmware("frame too long"));
        }
        let state = &mut self.queues[queue_index];
        if state.buffers[slot].is_none() {
            state.buffers[slot] = Some(DmaBuffer::new(1, 1).ok_or(Error::NoMemory)?);
        }
        let buffer = state.buffers[slot].as_ref().expect("allocated above");
        frame.len = (header.len() + body.len()) as u16;
        frame.scratch = buffer.phys + COMMAND_HEADER_LEN as u64 + 8;
        if header_end != COMMAND_HEADER_LEN + TX_PARAMETERS_LEN + header.len() {
            frame.flags |= super::commands::TX_PADDED_HEADER;
        }
        let station = frame.station;
        let sequence = (queue as u16) << 8 | slot as u16;
        let mut bytes = Vec::with_capacity(header_end);
        bytes.extend_from_slice(&[super::commands::TX, 0]);
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&super::commands::tx_parameters(&frame));
        bytes.extend_from_slice(header);
        bytes.resize(header_end, 0);
        buffer.write_bytes(0, &bytes);
        buffer.write_bytes(header_end, body);
        let mut pieces = alloc::vec![
            (buffer.phys, FIRST_BUFFER_LEN),
            (buffer.phys + FIRST_BUFFER_LEN as u64, header_end - FIRST_BUFFER_LEN),
        ];
        if !body.is_empty() {
            pieces.push((buffer.phys + header_end as u64, body.len()));
        }
        write_tfd(&state.tfds, slot, &pieces);

        // The scheduler's byte count for the slot: the frame, its checksum
        // and delimiter, in 4-byte units, and the station. The first 64
        // entries are repeated after the 256 (the scheduler reads past the
        // end of the ring).
        let count = (usize::from(frame.len) + 8).div_ceil(4) as u16 | u16::from(station) << 12;
        let entry = (queue_index * BYTE_COUNT_ENTRIES + slot) * 2;
        self.byte_counts.write16(entry, count);
        if slot < BYTE_COUNT_ENTRIES - TFD_COUNT {
            self.byte_counts.write16(entry + TFD_COUNT * 2, count);
        }
        let write = (slot + 1) % TFD_COUNT;
        self.queues[queue_index].write = write;
        self.write(HBUS_TARG_WRPTR, write as u32 | queue << 8);
        Ok(slot)
    }

    /// Frees slots of a data queue up to and including `slot` (the card
    /// reported it sent them).
    pub fn reclaim(&mut self, queue: u32, slot: usize) {
        if let Some(state) = self.queues.get_mut(queue as usize) {
            state.read = (slot + 1) % TFD_COUNT;
        }
    }

    /// Stops the card (its DMA, then its processor) so another firmware
    /// image can be loaded, and forgets everything queued.
    pub fn stop(&mut self) {
        self.write(CSR_INT_MASK, 0);
        self.write(CSR_INT, 0xFFFF_FFFF);
        if self.grab_nic_access().is_ok() {
            self.write_prph(SCD_TXFACT, 0);
            for channel in 0..8 {
                self.write(fh_tcsr_config(channel), 0);
            }
            let idle = 0xFF << 16;
            if !self.poll(FH_TSSR_TX_STATUS, idle, idle, 5000) {
                debug!("wifi: transmit DMA didn't stop: {:#x}", self.read(FH_TSSR_TX_STATUS));
            }
            self.write(FH_MEM_RCSR_CHNL0_CONFIG, 0);
            if !self.poll(FH_MEM_RSSR_RX_STATUS, 1 << 24, 1 << 24, 1000) {
                debug!("wifi: receive DMA didn't stop: {:#x}", self.read(FH_MEM_RSSR_RX_STATUS));
            }
            self.write_prph(APMG_CLK_DIS, 0x200);
            time::delay_us(5);
        }
        self.write(CSR_GP_CNTRL, self.read(CSR_GP_CNTRL) & !GP_CNTRL_MAC_ACCESS_REQ);
        self.set_bits(CSR_RESET, RESET_STOP_MASTER);
        if !self.poll(CSR_RESET, RESET_MASTER_DISABLED, RESET_MASTER_DISABLED, 100) {
            debug!("wifi: the card's bus master didn't stop");
        }
        self.set_bits(CSR_RESET, RESET_SW_RESET);
        time::delay_us(10);
        self.write(CSR_GP_CNTRL, self.read(CSR_GP_CNTRL) & !GP_CNTRL_INIT_DONE);
        self.pending.clear();
        self.rx_status.zero();
        self.byte_counts.zero();
        for buffer in &self.rx_buffers {
            buffer.zero();
        }
    }

    /// The next packet from the card, if any (call [`Self::poll_events`]
    /// first).
    pub fn next_packet(&mut self) -> Option<Packet> {
        self.pending.pop_front()
    }

    /// Checks the interrupt status and the receive ring, keeping any
    /// packets that arrived. Fails if the firmware or hardware reported an
    /// error.
    pub fn poll_events(&mut self) -> Result<(), Error> {
        let cause = self.read(CSR_INT);
        if cause != 0 && cause != 0xFFFF_FFFF {
            self.write(CSR_INT, cause);
            if cause & INT_FH_RX != 0 {
                self.write(CSR_FH_INT_STATUS, 0xC003_0000);
            }
            if cause & INT_RF_KILL != 0 {
                info!("wifi: the radio kill switch is {}", if self.radio_allowed() { "off" } else { "on" });
            }
            if cause & INT_SW_ERROR != 0 {
                return Err(Error::Firmware("the firmware crashed"));
            }
            if cause & INT_HW_ERROR != 0 {
                return Err(Error::Firmware("hardware error"));
            }
        }
        self.receive();
        Ok(())
    }

    /// Takes packets out of the buffers the card has filled.
    fn receive(&mut self) {
        let closed = usize::from(self.rx_status.read16(0) & 0xFFF) % RX_COUNT;
        while self.rx_read != closed {
            let buffer = &self.rx_buffers[self.rx_slot_buffer[self.rx_read]];
            let mut offset = 0;
            while offset + 8 < RX_BUFFER_SIZE {
                let length_and_flags = buffer.read32(offset);
                if length_and_flags == FRAME_INVALID {
                    break;
                }
                let len = (length_and_flags & 0x3FFF) as usize;
                if len < 4 || offset + 4 + len > RX_BUFFER_SIZE {
                    break;
                }
                let header = buffer.read32(offset + 4);
                self.pending.push_back(Packet {
                    command: header as u8,
                    group: (header >> 8) as u8,
                    sequence: (header >> 16) as u16,
                    data: buffer.read_bytes(offset + 8, len - 4),
                });
                offset += (len + 4).next_multiple_of(0x40);
            }
            // Give the buffer back at the write end, cleared so nothing in
            // it is ever read twice.
            let index = self.rx_slot_buffer[self.rx_read];
            self.rx_buffers[index].zero();
            self.rx_slot_buffer[self.rx_write] = index;
            self.rx_descriptors.write32(self.rx_write * 4, (self.rx_buffers[index].phys >> 8) as u32);
            self.rx_write = (self.rx_write + 1) % RX_COUNT;
            self.rx_read = (self.rx_read + 1) % RX_COUNT;
        }
        self.write(FH_RSCSR_CHNL0_WPTR, (self.rx_write & !7) as u32);
    }

    /// Waits up to `ms` for a packet `wanted` accepts, polling the card;
    /// other packets are passed to `other`.
    pub fn wait_for(
        &mut self,
        ms: u64,
        mut wanted: impl FnMut(&Packet) -> bool,
        mut other: impl FnMut(&Packet),
    ) -> Result<Packet, Error> {
        let deadline = time::uptime_nanos() + ms * 1_000_000;
        loop {
            self.poll_events()?;
            while let Some(packet) = self.pending.pop_front() {
                if wanted(&packet) {
                    return Ok(packet);
                }
                other(&packet);
            }
            if time::uptime_nanos() > deadline {
                return Err(Error::Timeout("a reply from the firmware"));
            }
            task::sleep_ms(1);
        }
    }
}

/// Fills in a TFD: the buffers it points at (address and length).
fn write_tfd(tfds: &DmaBuffer, slot: usize, pieces: &[(u64, usize)]) {
    let tfd = slot * TFD_SIZE;
    for offset in (0..TFD_SIZE).step_by(4) {
        tfds.write32(tfd + offset, 0);
    }
    for (index, &(phys, len)) in pieces.iter().enumerate() {
        let entry = tfd + 4 + index * 6;
        tfds.write32(entry, phys as u32);
        tfds.write16(entry + 4, ((phys >> 32) as u16 & 0xF) | (len as u16) << 4);
    }
    tfds.write8(tfd + 3, pieces.len() as u8);
}
