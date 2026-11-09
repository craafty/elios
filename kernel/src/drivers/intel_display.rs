//! Intel integrated graphics, read-only: just enough to know where the
//! screen's scanout is, so the desktop can copy a frame while the display
//! is between frames (vertical blank) instead of halfway down one, which
//! shows as a tear across moving pictures (video).
//!
//! The firmware (UEFI GOP) set the mode and nothing here changes it: the
//! driver finds the pipe whose primary plane is on, reads its source height,
//! and from then on reads its scanline counter (PIPEDSL), which counts the
//! line being sent to the panel: below the height means a frame is being
//! drawn, at or past it means vertical blank. These registers are at the
//! same offsets from Sandy Bridge to Kaby Lake, the T440p's Haswell
//! included; the pipes' power wells that GOP uses stay on.
//!
//! The one thing written is the panel's backlight. On Haswell with an
//! 8-series (Lynx Point) chipset, the T440p's, the chipset's PWM drives it:
//! BLC_PWM_PCH_CTL1 says whether it's on and which way round it is,
//! BLC_PWM_PCH_CTL2 holds the period (the top 16 bits) and the duty cycle
//! (the bottom 16), the brightness out of the period, as Linux's i915 sets
//! it. Programs see it as `/sys/class/backlight/intel_backlight`.

use core::sync::atomic::{AtomicU64, Ordering};

use super::pci;
use crate::memory::{self, CacheType};
use crate::sync::SpinLock;
use crate::{task, time};

/// One register block per pipe (A, B, C), 0x1000 apart.
const PIPE_STRIDE: usize = 0x1000;
/// Pipe source size: width - 1 in bits 27:16, height - 1 in bits 11:0.
const PIPESRC: usize = 0x6001C;
/// Display scanline: the line being scanned out, bits 12:0.
const PIPEDSL: usize = 0x70000;
/// Primary plane control: bit 31 is "plane on".
const DSPCNTR: usize = 0x70180;
/// The chipset's backlight PWM: control (bit 31 on, bit 29 inverted),
/// then period and duty cycle.
const BLC_PWM_PCH_CTL1: usize = 0xC8250;
const BLC_PWM_PCH_CTL2: usize = 0xC8254;
/// How much of the register space is mapped (past the backlight).
const MAPPED: usize = 0xC9000;

/// The scanline register's address (0: no display found) and the visible
/// height, in lines.
static SCANLINE: AtomicU64 = AtomicU64::new(0);
static HEIGHT: AtomicU64 = AtomicU64::new(0);
/// The register space (0: none), for the backlight.
static BACKLIGHT: AtomicU64 = AtomicU64::new(0);

fn read(base: u64, offset: usize) -> u32 {
    // SAFETY: within the mapped register space (offsets below MAPPED).
    unsafe { ((base as usize + offset) as *const u32).read_volatile() }
}

/// Finds Intel graphics and the pipe GOP lit.
pub fn probe(devices: &[pci::Device]) {
    let Some(device) = devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x03) else { return };
    let Some(bar) = device.memory_bar(0) else {
        info!("display: {:04x}: no register BAR", device.device);
        return;
    };
    // Memory decoding is on (GOP uses the device); nothing is written.
    let base = memory::map_device(bar, MAPPED as u64, CacheType::Uncached) as u64;
    probe_backlight(base);
    let mut found = None;
    for pipe in 0..3 {
        let control = read(base, DSPCNTR + pipe * PIPE_STRIDE);
        let source = read(base, PIPESRC + pipe * PIPE_STRIDE);
        let line = read(base, PIPEDSL + pipe * PIPE_STRIDE);
        debug!("display: pipe {pipe}: plane control {control:#x}, source {source:#x}, scanline {line:#x}");
        if control & (1 << 31) != 0 && found.is_none() {
            found = Some((pipe, (source & 0xFFF) + 1, (source >> 16 & 0xFFF) + 1));
        }
    }
    let Some((pipe, height, width)) = found else {
        info!("display: {:04x}: no pipe lit, no vertical blank timing", device.device);
        return;
    };
    let scanline = base + (PIPEDSL + pipe * PIPE_STRIDE) as u64;
    // The counter must be moving (a panel in self refresh stops it).
    let first = read(scanline, 0) & 0x1FFF;
    let mut moving = false;
    for _ in 0..50 {
        time::delay_us(100);
        if read(scanline, 0) & 0x1FFF != first {
            moving = true;
            break;
        }
    }
    info!(
        "display: {:04x}: pipe {} lit at {width}x{height}, scanline {}",
        device.device,
        (b'A' + pipe as u8) as char,
        if moving { "moving: copies wait for vertical blank" } else { "stopped: not used" }
    );
    if moving && height > 1 {
        HEIGHT.store(u64::from(height), Ordering::Relaxed);
        SCANLINE.store(scanline, Ordering::Release);
    }
}

fn probe_backlight(base: u64) {
    let (control, period_duty) = (read(base, BLC_PWM_PCH_CTL1), read(base, BLC_PWM_PCH_CTL2));
    info!("display: backlight PWM control {control:#010x}, period/duty {period_duty:#010x}");
    if control & (1 << 31) == 0 || period_duty >> 16 == 0 {
        info!("display: the chipset's backlight PWM is off: no brightness control");
        return;
    }
    BACKLIGHT.store(base, Ordering::Release);
}

/// While the lid is closed, the brightness to go back to (the backlight
/// itself is off).
static DARK: SpinLock<Option<u32>> = SpinLock::new(None);

/// The backlight's (brightness, most) if there's one to control. While
/// the lid has it off, the brightness it will come back to.
pub fn backlight() -> Option<(u32, u32)> {
    let dark = DARK.lock();
    let (brightness, most) = lit_backlight()?;
    Some((dark.unwrap_or(brightness), most))
}

/// What the PWM is set to now.
fn lit_backlight() -> Option<(u32, u32)> {
    let base = BACKLIGHT.load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    let inverted = read(base, BLC_PWM_PCH_CTL1) & (1 << 29) != 0;
    let value = read(base, BLC_PWM_PCH_CTL2);
    let (most, duty) = (value >> 16, value & 0xFFFF);
    Some((if inverted { most.saturating_sub(duty) } else { duty }.min(most), most))
}

/// Sets the backlight's brightness, out of [`backlight`]'s most; never
/// below a twentieth of that, so the screen can't go dark by mistake.
/// While the lid is closed it's what comes back when it opens.
pub fn set_backlight(brightness: u32) -> bool {
    let mut dark = DARK.lock();
    let Some((_, most)) = lit_backlight() else { return false };
    let brightness = brightness.clamp(most.div_ceil(20), most);
    match dark.as_mut() {
        Some(saved) => *saved = brightness,
        None => write_backlight(brightness, most),
    }
    debug!("display: backlight {brightness} of {most}");
    true
}

/// Turns the backlight off while the lid is closed (nobody can see the
/// panel, and it's most of a laptop's idle power), and back as it was
/// when it opens.
pub fn set_lid_closed(closed: bool) {
    let mut dark = DARK.lock();
    let Some((brightness, most)) = lit_backlight() else { return };
    match (closed, *dark) {
        (true, None) => {
            *dark = Some(brightness);
            write_backlight(0, most);
            info!("display: lid closed: backlight off (was {brightness} of {most})");
        }
        (false, Some(saved)) => {
            *dark = None;
            write_backlight(saved, most);
            info!("display: lid open: backlight back to {saved} of {most}");
        }
        _ => {}
    }
}

fn write_backlight(brightness: u32, most: u32) {
    let base = BACKLIGHT.load(Ordering::Acquire);
    let inverted = read(base, BLC_PWM_PCH_CTL1) & (1 << 29) != 0;
    let duty = if inverted { most - brightness } else { brightness };
    // SAFETY: the mapped register space; only the duty cycle changes.
    unsafe { ((base as usize + BLC_PWM_PCH_CTL2) as *mut u32).write_volatile(most << 16 | duty) };
}

/// Returns at the start of the next vertical blank (or at once if the
/// display is in one), sleeping while most of a frame is left and
/// spinning for the last stretch. Gives up after two frames' time, so a
/// display that stops never hangs the caller. `false` if there is no
/// display to wait for.
pub fn wait_for_vblank() -> bool {
    let scanline = SCANLINE.load(Ordering::Acquire);
    if scanline == 0 {
        return false;
    }
    let height = HEIGHT.load(Ordering::Relaxed) as u32;
    let deadline = time::uptime_nanos() + 40_000_000;
    loop {
        let line = read(scanline, 0) & 0x1FFF;
        if line >= height || time::uptime_nanos() > deadline {
            return true;
        }
        // About 15 µs a line at 1080 lines and 60 Hz: sleep while more
        // than ~2.5 ms of the frame is left.
        if height - line > height / 6 {
            task::sleep_ms(1);
        } else {
            core::hint::spin_loop();
        }
    }
}
