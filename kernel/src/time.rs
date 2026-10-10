//! Timekeeping.
//!
//! Uptime comes from the CPU's timestamp counter (TSC), which is cheap to read
//! and has nanosecond-level resolution. The local APIC timer provides a
//! periodic tick for things that need to happen regularly (preemption, from
//! Phase 4). Neither runs at a frequency we know in advance, so both are
//! measured against the PIT, whose frequency is fixed.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::interrupts::{self, InterruptFrame};
use crate::arch::{apic, cpuid, pit, rdtsc};

/// Periodic timer interrupts per second.
pub const TICK_HZ: u64 = 1000;

/// About 50 ms of PIT ticks: long enough for an accurate measurement.
const CALIBRATION_PIT_TICKS: u16 = 59_659;

static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);
/// Wall-clock time at boot, in seconds since the Unix epoch.
static BOOT_UNIX_TIME: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

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

    interrupts::set_handler(interrupts::TIMER, on_tick);
    apic::timer_start_periodic(interrupts::TIMER, (apic_hz / TICK_HZ) as u32);

    info!(
        "time: TSC at {} MHz{}, APIC timer at {} MHz, ticking at {TICK_HZ} Hz",
        tsc_hz / 1_000_000,
        if invariant_tsc() { "" } else { " (not invariant: may drift with power states)" },
        apic_hz / 1_000_000,
    );
}

fn on_tick(_frame: &mut InterruptFrame) {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::task::on_tick(now);
}

/// Timer ticks (milliseconds) since the periodic timer started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Nanoseconds since boot, or 0 before the TSC has been measured.
pub fn uptime_nanos() -> u64 {
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        return 0;
    }
    let elapsed = rdtsc().saturating_sub(TSC_AT_BOOT.load(Ordering::Relaxed));
    (u128::from(elapsed) * 1_000_000_000 / u128::from(hz)) as u64
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
            let start_ticks = ticks();
            let start = uptime_nanos();
            while ticks() < start_ticks + 50 {
                core::hint::spin_loop();
            }
            let elapsed_ms = (uptime_nanos() - start) / 1_000_000;
            // 50 ticks is 50 ms. Emulators can be sluggish, so be generous;
            // this catches a wildly wrong calibration, not small drift.
            assert!((25..=200).contains(&elapsed_ms), "50 ticks took {elapsed_ms} ms");
        }
    }
}
