//! Timekeeping.
//!
//! Uptime comes from the CPU's timestamp counter (TSC), which is cheap to read
//! and has nanosecond-level resolution. The local APIC timer provides a
//! periodic tick for things that need to happen regularly (preemption, from
//! Phase 4). Neither runs at a frequency we know in advance, so both are
//! measured against the PIT, whose frequency is fixed.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::interrupts::{self, InterruptFrame};
use crate::arch::{apic, cpu, cpuid, pit, rdtsc};

/// Periodic timer interrupts per second.
pub const TICK_HZ: u64 = 1000;

/// About 50 ms of PIT ticks: long enough for an accurate measurement.
const CALIBRATION_PIT_TICKS: u16 = 59_659;

static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);
/// Wall-clock time at boot, in seconds since the Unix epoch.
static BOOT_UNIX_TIME: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC count, as a 32.32 fixed-point number: uptime is
/// `(counts * TSC_MULT) >> 32`. A multiply instead of a division, and the
/// exact formula the vDSO uses, so the system call and vDSO clocks agree to
/// the nanosecond.
static TSC_MULT: AtomicU64 = AtomicU64::new(0);
/// Timer interrupts on the boot CPU (for checking the timer's rate).
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
/// The APIC timer count between ticks, for starting other CPUs' timers.
static TIMER_PERIOD: AtomicU64 = AtomicU64::new(0);

/// Marks the moment uptime counts from. Called first thing at boot.
pub fn mark_boot() {
    TSC_AT_BOOT.store(rdtsc(), Ordering::Relaxed);
}

/// Measures the TSC and APIC timer and starts the periodic tick. Needs the
/// local APIC initialized.
pub fn init() {
    apic::timer_start_free_running();
    let tsc_start = rdtsc();
    let apic_start = apic::timer_count();
    pit::busy_wait(CALIBRATION_PIT_TICKS);
    let tsc_end = rdtsc();
    let apic_end = apic::timer_count();

    BOOT_UNIX_TIME.store(crate::rtc::read_unix_time(), Ordering::Relaxed);
    let per_second = |elapsed: u64| elapsed * pit::FREQUENCY_HZ / u64::from(CALIBRATION_PIT_TICKS);
    let tsc_hz = per_second(tsc_end - tsc_start);
    let apic_hz = per_second(u64::from(apic_start - apic_end));
    TSC_HZ.store(tsc_hz, Ordering::Relaxed);
    TSC_MULT.store(((1_000_000_000u128 << 32) / u128::from(tsc_hz)) as u64, Ordering::Relaxed);

    interrupts::set_handler(interrupts::TIMER, on_tick);
    TIMER_PERIOD.store(apic_hz / TICK_HZ, Ordering::Relaxed);
    start_timer();

    info!(
        "time: TSC at {} MHz{}, APIC timer at {} MHz, ticking at {TICK_HZ} Hz",
        tsc_hz / 1_000_000,
        if invariant_tsc() { "" } else { " (not invariant: may drift with power states)" },
        apic_hz / 1_000_000,
    );
}

/// Starts this CPU's periodic tick. Every CPU's APIC timer runs at the
/// rate measured on the boot CPU (they share the bus clock).
pub fn start_timer() {
    apic::timer_start_periodic(interrupts::TIMER, TIMER_PERIOD.load(Ordering::Relaxed) as u32);
}

/// Every CPU's timer fires every millisecond; the boot CPU's also wakes
/// threads whose time has come.
fn on_tick(frame: &mut InterruptFrame) {
    let now = (cpu::current().index() == 0).then(|| {
        INTERRUPTS.fetch_add(1, Ordering::Relaxed);
        ticks()
    });
    crate::process::sample_tick(frame);
    crate::task::on_tick(now);
}

/// Milliseconds since boot: the unit of the kernel's deadlines ("ticks",
/// for the timer interrupt that checks them). They're read from the TSC,
/// not counted interrupts, so deadlines keep to real time even when
/// interrupts arrive late (in an emulator, or after a long stretch with
/// interrupts off): a late tick finds everything that came due meanwhile.
pub fn ticks() -> u64 {
    uptime_nanos() / 1_000_000
}

/// The timestamp counter's frequency, as measured at boot.
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Nanoseconds since boot, or 0 before the TSC has been measured.
pub fn uptime_nanos() -> u64 {
    let elapsed = rdtsc().wrapping_sub(TSC_AT_BOOT.load(Ordering::Relaxed));
    ((u128::from(elapsed) * u128::from(TSC_MULT.load(Ordering::Relaxed))) >> 32) as u64
}

/// What the vDSO needs to tell the time without the kernel: the TSC value
/// uptime counts from, [`TSC_MULT`], and the wall-clock time at boot in
/// nanoseconds. None of them change after boot.
pub fn clock_parameters() -> [u64; 3] {
    [
        TSC_AT_BOOT.load(Ordering::Relaxed),
        TSC_MULT.load(Ordering::Relaxed),
        BOOT_UNIX_TIME.load(Ordering::Relaxed) * 1_000_000_000,
    ]
}

/// Waits `micros` microseconds without sleeping, for hardware that needs
/// a short pause between register accesses.
pub fn delay_us(micros: u64) {
    let end = uptime_nanos() + micros * 1000;
    while uptime_nanos() < end {
        core::hint::spin_loop();
    }
}

/// Wall-clock time in nanoseconds since the Unix epoch.
pub fn unix_nanos() -> u64 {
    BOOT_UNIX_TIME.load(Ordering::Relaxed) * 1_000_000_000 + uptime_nanos()
}

/// Wall-clock time in seconds since the Unix epoch.
pub fn unix_time() -> u64 {
    unix_nanos() / 1_000_000_000
}

/// Whether the TSC runs at a constant rate regardless of CPU power states.
fn invariant_tsc() -> bool {
    let [max_extended_leaf, ..] = cpuid(0x8000_0000, 0);
    max_extended_leaf >= 0x8000_0007 && cpuid(0x8000_0007, 0)[3] & (1 << 8) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn uptime_moves_forward() {
            let first = uptime_nanos();
            let second = uptime_nanos();
            assert!(first > 0 && second >= first);
        }

        fn the_timer_ticks_at_its_rate() {
            // Interrupts are on while tests run, so the tick handler fires.
            let start_ticks = INTERRUPTS.load(Ordering::Relaxed);
            let start = uptime_nanos();
            while INTERRUPTS.load(Ordering::Relaxed) < start_ticks + 50 {
                core::hint::spin_loop();
            }
            let elapsed_ms = (uptime_nanos() - start) / 1_000_000;
            // 50 ticks is 50 ms. Emulators can be sluggish, so be generous;
            // this catches a wildly wrong calibration, not small drift.
            assert!((25..=200).contains(&elapsed_ms), "50 ticks took {elapsed_ms} ms");
        }
    }
}
