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

use core::sync::atomic::{AtomicU64, Ordering};

use super::pci;
use crate::memory::{self, CacheType};
use crate::{task, time};

/// One register block per pipe (A, B, C), 0x1000 apart.
const PIPE_STRIDE: usize = 0x1000;
/// Pipe source size: width - 1 in bits 27:16, height - 1 in bits 11:0.
const PIPESRC: usize = 0x6001C;
/// Display scanline: the line being scanned out, bits 12:0.
const PIPEDSL: usize = 0x70000;
/// Primary plane control: bit 31 is "plane on".
const DSPCNTR: usize = 0x70180;
/// How much of the register space is mapped (past the last pipe's plane).
const MAPPED: usize = 0x73000;

/// The scanline register's address (0: no display found) and the visible
/// height, in lines.
static SCANLINE: AtomicU64 = AtomicU64::new(0);
static HEIGHT: AtomicU64 = AtomicU64::new(0);

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
