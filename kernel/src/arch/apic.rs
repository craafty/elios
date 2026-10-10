//! The Advanced Programmable Interrupt Controllers.
//!
//! Each CPU has a local APIC, which delivers interrupts to it and has a
//! built-in timer. Device interrupts arrive through one or more I/O APICs,
//! which route each input line (a "global system interrupt", GSI) to a vector
//! on a chosen CPU.
//!
//! Both are accessed through memory-mapped registers, mapped uncached.

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use super::interrupts::{APIC_ERROR, SPURIOUS};
use super::{rdmsr, wrmsr};
use crate::acpi::{InterruptOverride, Madt};
use crate::collections::ArrayVec;
use crate::memory::{self, CacheType};
use crate::sync::SpinLock;

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

// Local APIC registers (byte offsets).
const ID: usize = 0x20;
const TASK_PRIORITY: usize = 0x80;
const END_OF_INTERRUPT: usize = 0xB0;
const SPURIOUS_VECTOR: usize = 0xF0;
const ERROR_STATUS: usize = 0x280;
const LVT_TIMER: usize = 0x320;
const LVT_LINT0: usize = 0x350;
const LVT_LINT1: usize = 0x360;
const LVT_ERROR: usize = 0x370;
const TIMER_INITIAL_COUNT: usize = 0x380;
const TIMER_CURRENT_COUNT: usize = 0x390;
const TIMER_DIVIDE: usize = 0x3E0;

const SOFTWARE_ENABLE: u32 = 1 << 8;
// Local vector table entry bits.
const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const LVT_DELIVERY_NMI: u32 = 0b100 << 8;
const LVT_ACTIVE_LOW: u32 = 1 << 13;
/// Timer divide configuration value for dividing the bus clock by 16.
const TIMER_DIVIDE_BY_16: u32 = 0b0011;

/// Virtual address of the local APIC's registers.
static LOCAL_APIC: AtomicU64 = AtomicU64::new(0);

fn local_read(register: usize) -> u32 {
    // SAFETY: the register block is mapped, and registers are 32-bit aligned.
    unsafe { ptr::read_volatile((LOCAL_APIC.load(Ordering::Relaxed) as usize + register) as *const u32) }
}

fn local_write(register: usize, value: u32) {
    // SAFETY: as for `local_read`.
    unsafe { ptr::write_volatile((LOCAL_APIC.load(Ordering::Relaxed) as usize + register) as *mut u32, value) };
}

/// Enables this CPU's local APIC with everything masked except error and
/// NMI delivery.
pub fn init_local(madt: &Madt) {
    // SAFETY: IA32_APIC_BASE exists on every x86_64 CPU; setting the enable
    // bit keeps the APIC at its current address.
    let base = unsafe {
        let msr = rdmsr(IA32_APIC_BASE);
        wrmsr(IA32_APIC_BASE, msr | APIC_BASE_ENABLE);
        msr & APIC_BASE_ADDRESS_MASK
    };
    LOCAL_APIC.store(memory::map_device(base, 0x400, CacheType::Uncached) as u64, Ordering::Relaxed);

    // LINT0 carries the legacy PIC's output ("virtual wire" mode); that path
    // is off. The firmware tables say which input is the NMI line.
    local_write(LVT_LINT0, LVT_MASKED);
    local_write(LVT_LINT1, LVT_MASKED);
    for nmi in madt.local_nmis.iter() {
        let register = if nmi.lint == 0 { LVT_LINT0 } else { LVT_LINT1 };
        local_write(register, LVT_DELIVERY_NMI | if nmi.active_low { LVT_ACTIVE_LOW } else { 0 });
    }

    local_write(LVT_TIMER, LVT_MASKED);
    local_write(LVT_ERROR, u32::from(APIC_ERROR));
    // The error status register must be written before it's read.
    local_write(ERROR_STATUS, 0);
    local_write(ERROR_STATUS, 0);
    local_write(TASK_PRIORITY, 0);
    local_write(SPURIOUS_VECTOR, SOFTWARE_ENABLE | u32::from(SPURIOUS));
    end_of_interrupt();
}

/// This CPU's local APIC ID.
pub fn local_id() -> u8 {
    (local_read(ID) >> 24) as u8
}

/// Tells the local APIC the current interrupt has been handled.
pub fn end_of_interrupt() {
    local_write(END_OF_INTERRUPT, 0);
}

/// Starts the timer counting down from its maximum, with no interrupt, so
/// its rate can be measured with [`timer_count`].
pub fn timer_start_free_running() {
    local_write(TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
    local_write(LVT_TIMER, LVT_MASKED);
    local_write(TIMER_INITIAL_COUNT, u32::MAX);
}

pub fn timer_count() -> u32 {
    local_read(TIMER_CURRENT_COUNT)
}

/// Fires `vector` every `period` timer ticks (bus clock / 16).
pub fn timer_start_periodic(vector: u8, period: u32) {
    local_write(TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
    local_write(LVT_TIMER, u32::from(vector) | LVT_TIMER_PERIODIC);
    local_write(TIMER_INITIAL_COUNT, period.max(1));
}

#[derive(Clone, Copy, Default)]
struct IoApic {
    /// Virtual address of the register-select register; the data window is
    /// 0x10 bytes after it.
    base: u64,
    gsi_base: u32,
    inputs: u32,
}

impl IoApic {
    fn read(&self, register: u32) -> u32 {
        // SAFETY: the registers are mapped; callers hold the I/O APIC lock,
        // so the select/data pair isn't interleaved.
        unsafe {
            ptr::write_volatile(self.base as *mut u32, register);
            ptr::read_volatile((self.base + 0x10) as *const u32)
        }
    }

    fn write(&self, register: u32, value: u32) {
        // SAFETY: as for `read`.
        unsafe {
            ptr::write_volatile(self.base as *mut u32, register);
            ptr::write_volatile((self.base + 0x10) as *mut u32, value);
        }
    }

    fn set_redirection(&self, input: u32, low: u32, high: u32) {
        // Write the high half (destination) first, then the low half, which
        // holds the mask bit.
        self.write(0x10 + 2 * input + 1, high);
        self.write(0x10 + 2 * input, low);
    }
}

struct IoApics {
    controllers: ArrayVec<IoApic, 8>,
    overrides: ArrayVec<InterruptOverride, 16>,
}

static IO_APICS: SpinLock<Option<IoApics>> = SpinLock::new(None);

// Redirection entry bits.
const REDIRECT_ACTIVE_LOW: u32 = 1 << 13;
const REDIRECT_LEVEL_TRIGGERED: u32 = 1 << 15;
const REDIRECT_MASKED: u32 = 1 << 16;

/// Finds the I/O APICs and masks all their inputs.
pub fn init_io(madt: &Madt) {
    let mut controllers = ArrayVec::new();
    for info in madt.io_apics.iter() {
        let registers = memory::map_device(u64::from(info.address), 0x20, CacheType::Uncached);
        let mut io_apic = IoApic { base: registers as u64, gsi_base: info.gsi_base, inputs: 0 };
        // Version register: bits 16-23 hold the index of the last input.
        io_apic.inputs = (io_apic.read(0x01) >> 16 & 0xFF) + 1;
        for input in 0..io_apic.inputs {
            io_apic.set_redirection(input, REDIRECT_MASKED, 0);
        }
        let _ = controllers.push(io_apic);
    }
    let mut overrides = ArrayVec::new();
    for entry in madt.overrides.iter() {
        let _ = overrides.push(*entry);
    }
    *IO_APICS.lock() = Some(IoApics { controllers, overrides });
}

/// Delivers legacy ISA interrupt `irq` to this CPU on `vector`, following
/// any rewiring the firmware reported.
pub fn route_isa_irq(irq: u8, vector: u8) -> Result<(), &'static str> {
    let guard = IO_APICS.lock();
    let io_apics = guard.as_ref().ok_or("I/O APICs not initialized")?;

    let wiring = io_apics.overrides.iter().find(|o| o.irq == irq).copied().unwrap_or(InterruptOverride {
        irq,
        gsi: u32::from(irq),
        active_low: false,
        level_triggered: false,
    });
    let io_apic = io_apics
        .controllers
        .iter()
        .find(|io| (io.gsi_base..io.gsi_base + io.inputs).contains(&wiring.gsi))
        .ok_or("no I/O APIC handles this interrupt")?;

    let mut low = u32::from(vector);
    if wiring.active_low {
        low |= REDIRECT_ACTIVE_LOW;
    }
    if wiring.level_triggered {
        low |= REDIRECT_LEVEL_TRIGGERED;
    }
    let high = u32::from(local_id()) << 24;
    io_apic.set_redirection(wiring.gsi - io_apic.gsi_base, low, high);
    Ok(())
}
