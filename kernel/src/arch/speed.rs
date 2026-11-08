//! CPU speed (P-states). Intel CPUs start at whatever ratio the firmware
//! left them: often the base clock, sometimes the slowest, and never turbo,
//! which only runs when the operating system asks for a ratio above base.
//! An operating system without a speed driver therefore wastes much of an
//! older laptop's single-thread speed, which is what software video
//! rendering in Firefox needs most. Asking for the top ratio is what
//! Linux's "performance" setting does: the CPU still drops its clock and
//! voltage when a core halts (idle), and it throttles by itself when hot.
//!
//! Everything here is read and written through MSRs that fault on CPUs
//! without them, so each access is the fault-tolerant kind, and every step
//! is logged: this only runs on real hardware (virtual machines don't
//! offer Enhanced SpeedStep).

use core::sync::atomic::{AtomicU64, Ordering};

use super::{cpuid, rdtsc, try_rdmsr, try_wrmsr};
use crate::time;

const IA32_MPERF: u32 = 0xE7;
const IA32_APERF: u32 = 0xE8;
const MSR_PLATFORM_INFO: u32 = 0xCE;
const IA32_PERF_STATUS: u32 = 0x198;
const IA32_PERF_CTL: u32 = 0x199;
const IA32_MISC_ENABLE: u32 = 0x1A0;
const MSR_TURBO_RATIO_LIMIT: u32 = 0x1AD;

/// IA32_MISC_ENABLE: Enhanced SpeedStep on, and turbo forbidden.
const SPEEDSTEP_ENABLE: u64 = 1 << 16;
const TURBO_DISABLE: u64 = 1 << 38;

/// The IA32_PERF_CTL value every CPU is given (0: leave them alone).
static REQUEST: AtomicU64 = AtomicU64::new(0);

/// Chooses the fastest ratio on the boot CPU and asks for it. Call once
/// the timestamp counter has been measured (for the log's MHz figures).
pub fn init() {
    let [_, ebx, ecx, edx] = cpuid(0, 0);
    let intel = [ebx, edx, ecx] == [0x756E_6547, 0x4965_6E69, 0x6C65_746E]; // "GenuineIntel"
    let features = cpuid(1, 0)[2];
    if features & (1 << 31) != 0 {
        info!("speed: virtual machine, CPU speed is the host's business");
        return;
    }
    if !intel || features & (1 << 7) == 0 {
        info!("speed: no Enhanced SpeedStep (intel {intel}), CPU speed left as the firmware set it");
        return;
    }
    let Some(platform) = try_rdmsr(MSR_PLATFORM_INFO) else {
        warn!("speed: MSR_PLATFORM_INFO unreadable, CPU speed left alone");
        return;
    };
    let Some(mut misc) = try_rdmsr(IA32_MISC_ENABLE) else {
        warn!("speed: IA32_MISC_ENABLE unreadable, CPU speed left alone");
        return;
    };
    let base = (platform >> 8) & 0xFF;
    let lowest = (platform >> 40) & 0xFF;
    let bus_mhz = 100; // Sandy Bridge and later.
    info!(
        "speed: platform info {platform:#x} (base ratio {base}, lowest {lowest}), misc enable {misc:#x}, perf ctl {:#x}, \
         perf status {:#x}",
        try_rdmsr(IA32_PERF_CTL).unwrap_or(0),
        try_rdmsr(IA32_PERF_STATUS).unwrap_or(0)
    );
    if base == 0 {
        warn!("speed: no base ratio, CPU speed left alone");
        return;
    }
    if misc & SPEEDSTEP_ENABLE == 0 {
        let enabled = try_wrmsr(IA32_MISC_ENABLE, misc | SPEEDSTEP_ENABLE);
        misc = try_rdmsr(IA32_MISC_ENABLE).unwrap_or(misc);
        info!("speed: turning Enhanced SpeedStep on: {}", if enabled { "done" } else { "refused" });
    }
    // CPUID leaf 6 reports turbo only while IA32_MISC_ENABLE allows it
    // (the firmware's setup screen can forbid it; that choice stands).
    let turbo = misc & TURBO_DISABLE == 0 && cpuid(6, 0)[0] & (1 << 1) != 0;
    let top = if turbo { try_rdmsr(MSR_TURBO_RATIO_LIMIT).map_or(0, |limits| limits & 0xFF) } else { 0 };
    let ratio = top.max(base);
    info!(
        "speed: turbo {}, asking for ratio {ratio} ({} MHz; base {} MHz)",
        if turbo { "available" } else { "off" },
        ratio * bus_mhz,
        base * bus_mhz
    );
    let before = measure_mhz();
    let request = ratio << 8;
    REQUEST.store(request, Ordering::Relaxed);
    if !try_wrmsr(IA32_PERF_CTL, request) {
        REQUEST.store(0, Ordering::Relaxed);
        warn!("speed: IA32_PERF_CTL write refused");
        return;
    }
    let after = measure_mhz();
    info!(
        "speed: running at {} MHz (was {}), perf status {:#x}",
        after.map_or_else(|| "?".into(), |mhz| alloc::format!("{mhz}")),
        before.map_or_else(|| "?".into(), |mhz| alloc::format!("{mhz}")),
        try_rdmsr(IA32_PERF_STATUS).unwrap_or(0)
    );
}

/// Gives a CPU that just started the boot CPU's choice.
pub fn init_other() {
    let request = REQUEST.load(Ordering::Relaxed);
    if request != 0 {
        try_wrmsr(IA32_PERF_CTL, request);
    }
}

/// The clock this CPU actually runs at while busy, in MHz, from how fast
/// APERF (actual cycles) ticks against MPERF (cycles at the base clock,
/// which the timestamp counter also counts) over 20 ms of spinning.
fn measure_mhz() -> Option<u64> {
    let tsc_hz = time::tsc_hz();
    if tsc_hz == 0 {
        return None;
    }
    let (aperf, mperf) = (try_rdmsr(IA32_APERF)?, try_rdmsr(IA32_MPERF)?);
    let end = rdtsc() + tsc_hz / 50;
    while rdtsc() < end {
        core::hint::spin_loop();
    }
    let actual = try_rdmsr(IA32_APERF)?.wrapping_sub(aperf);
    let reference = try_rdmsr(IA32_MPERF)?.wrapping_sub(mperf);
    (reference != 0).then(|| (tsc_hz as u128 * actual as u128 / reference as u128 / 1_000_000) as u64)
}

#[cfg(test)]
mod tests {
    use super::super::{try_rdmsr, try_wrmsr};

    crate::ktest! {
        fn missing_msrs_fail_instead_of_crashing() {
            const IA32_EFER: u32 = 0xC000_0080;
            // Long mode is active, so EFER exists and has LMA (bit 10) set.
            assert!(try_rdmsr(IA32_EFER).is_some_and(|efer| efer & (1 << 10) != 0));
            // No CPU has this one: it faults (or reads 0 where a hypervisor
            // ignores unknown MSRs); either way the kernel carries on.
            let missing = 0x0BAD_0000;
            assert!(try_rdmsr(missing).is_none_or(|value| value == 0));
            let _ = try_wrmsr(missing, 1);
        }
    }
}
