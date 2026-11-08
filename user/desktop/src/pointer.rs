//! Pointer acceleration: how far the pointer moves for a mouse's or
//! touchpad's movement.
//!
//! Like libinput, this works in physical units: movement is turned into
//! millimetres (PS/2 devices are set to 8 counts per millimetre; others are
//! taken to be 1000 dpi mice) and its speed into millimetres per second,
//! from the events' timestamps rather than the size of each report (a
//! report's size depends on the sample rate, so speed from it differs from
//! device to device). Slow movements move the pointer a fixed distance per
//! millimetre, for precise pointing; faster ones are sped up, up to a limit,
//! so the screen can be crossed with one flick on a small touchpad. The
//! speed is averaged over the last few reports, which smooths out jitter in
//! when they arrive.
//!
//! Slow movements move a PS/2 device's pointer 2 pixels per count (16 per
//! millimetre: a laptop's touchpad is small), and a mouse's 1 pixel per
//! count, as everywhere else. The first report after a pause gets no
//! acceleration, so a single movement (`@monitor mouse_move` in a test
//! session, which QEMU's USB mouse delivers) moves exactly that.
/// Below this speed (mm/s) there's no acceleration.
const SLOW_MM_PER_S: f64 = 30.0;
/// The extra gain per mm/s above `SLOW_MM_PER_S`.
const GAIN_PER_MM_PER_S: f64 = 1.0 / 135.0;
/// The most the gain grows (3: a fast flick moves three times as far).
const MAX_GAIN: f64 = 3.0;
/// Reports further apart than this start a new movement.
const PAUSE_NANOS: u64 = 50_000_000;
/// Times between reports are counted as at least this (PS/2 mice report at
/// most 100 times a second; bytes queued together arrive at once).
const MIN_INTERVAL_NANOS: u64 = 8_000_000;
/// How many reports the speed is averaged over.
const HISTORY: usize = 3;

/// `BUS_I8042` in `struct input_id`: a PS/2 device.
const BUS_I8042: u16 = 0x11;

pub struct Acceleration {
    counts_per_mm: f64,
    /// Pixels per count for slow movements.
    base: f64,
    /// When the last report arrived.
    last_nanos: u64,
    /// The latest reports' distances (mm) and intervals (ns), newest last.
    history: [(f64, u64); HISTORY],
    len: usize,
}

impl Acceleration {
    pub fn new(bus: u16) -> Self {
        let (counts_per_mm, base) = if bus == BUS_I8042 { (8.0, 2.0) } else { (1000.0 / 25.4, 1.0) };
        Self { counts_per_mm, base, last_nanos: 0, history: [(0.0, 0); HISTORY], len: 0 }
    }

    /// The pointer's movement in pixels for a report of (dx, dy) counts that
    /// arrived at `nanos`.
    pub fn apply(&mut self, dx: f64, dy: f64, nanos: u64) -> (f64, f64) {
        let fresh = self.last_nanos == 0 || nanos.saturating_sub(self.last_nanos) > PAUSE_NANOS;
        let interval = nanos.saturating_sub(self.last_nanos);
        self.last_nanos = nanos;
        let distance = hypot(dx, dy) / self.counts_per_mm;
        let mut gain = 1.0;
        if fresh {
            self.len = 0;
        } else {
            if self.len == HISTORY {
                self.history.copy_within(1.., 0);
                self.len -= 1;
            }
            self.history[self.len] = (distance, interval.max(MIN_INTERVAL_NANOS));
            self.len += 1;
            let (millimetres, nanos) =
                self.history[..self.len].iter().fold((0.0, 0u64), |(mm, ns), &(d, t)| (mm + d, ns + t));
            let speed = millimetres / (nanos as f64 / 1e9);
            gain = (1.0 + (speed - SLOW_MM_PER_S).max(0.0) * GAIN_PER_MM_PER_S).min(MAX_GAIN);
        }
        (dx * self.base * gain, dy * self.base * gain)
    }
}

/// sqrt(x² + y²), without a maths library.
fn hypot(x: f64, y: f64) -> f64 {
    let square = x * x + y * y;
    if square == 0.0 {
        return 0.0;
    }
    // Newton's method from a rough start.
    let mut root = square.max(1.0);
    for _ in 0..20 {
        root = (root + square / root) / 2.0;
    }
    root
}
