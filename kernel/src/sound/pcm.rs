//! ALSA PCM devices (`/dev/snd/pcmC0D0p`, `pcmC0D0c`): a stream of samples
//! to play or record, driven by ioctls with Linux's numbers and structures.
//!
//! A program first narrows the parameters down (`HW_REFINE`: "which of
//! these formats, rates and buffer sizes could you do?"), then sets them
//! (`HW_PARAMS`), prepares, and writes or reads frames (`WRITEI_FRAMES`).
//! The buffer is a ring the device loops over. Two counters run around
//! it: the *application pointer* (frames written or read by the program)
//! and the *hardware pointer* (frames played or recorded). Both wrap at the
//! "boundary", a large multiple of the buffer size, as on Linux. If the
//! device catches up with the program (nothing left to play, or no room to
//! record), that's an *xrun*: the stream stops and the program prepares it
//! again.
//!
//! ALSA's library would like to map the status and control records into
//! memory; when that fails it asks with `SYNC_PTR` instead, which is what
//! eliOS supports.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{AUDIO_GROUP, Card, Direction, Hardware};
use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, POLLERR, POLLIN, POLLOUT, PollSource, ramfs};
use crate::process::{self, user};
use crate::sync::SpinLock;
use crate::time;

const VERSION: u32 = 0x0002_0012;

// Requests ('A').
const PVERSION: u64 = 0x8004_4100;
const INFO: u64 = 0x8120_4101;
const TSTAMP: u64 = 0x4004_4102;
const TTSTAMP: u64 = 0x4004_4103;
const USER_PVERSION: u64 = 0x4004_4104;
const HW_REFINE: u64 = 0xC260_4110;
const HW_PARAMS: u64 = 0xC260_4111;
const HW_FREE: u64 = 0x4112;
const SW_PARAMS: u64 = 0xC088_4113;
const STATUS: u64 = 0x8098_4120;
const DELAY: u64 = 0x8008_4121;
const HWSYNC: u64 = 0x4122;
const SYNC_PTR: u64 = 0xC088_4123;
const STATUS_EXT: u64 = 0xC098_4124;
const PREPARE: u64 = 0x4140;
const RESET: u64 = 0x4141;
const START: u64 = 0x4142;
const DROP: u64 = 0x4143;
const DRAIN: u64 = 0x4144;
const PAUSE: u64 = 0x4004_4145;
const REWIND: u64 = 0x4008_4146;
const RESUME: u64 = 0x4147;
const XRUN: u64 = 0x4148;
const FORWARD: u64 = 0x4008_4149;
const WRITEI_FRAMES: u64 = 0x4018_4150;
const READI_FRAMES: u64 = 0x8018_4151;
const LINK: u64 = 0x4004_4160;
const UNLINK: u64 = 0x4161;

/// Stream states, with Linux's numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Open = 0,
    Setup = 1,
    Prepared = 2,
    Running = 3,
    Xrun = 4,
    Draining = 5,
    Paused = 6,
}

// hw_params: three masks, then twelve intervals (parameters 8 to 19).
const ACCESS: usize = 0;
const FORMAT: usize = 1;
const SUBFORMAT: usize = 2;
const SAMPLE_BITS: usize = 8;
const FRAME_BITS: usize = 9;
const CHANNELS: usize = 10;
const RATE: usize = 11;
const PERIOD_TIME: usize = 12;
const PERIOD_SIZE: usize = 13;
const PERIOD_BYTES: usize = 14;
const PERIODS: usize = 15;
const BUFFER_TIME: usize = 16;
const BUFFER_SIZE: usize = 17;
const BUFFER_BYTES: usize = 18;
const TICK_TIME: usize = 19;

const MASKS_OFFSET: usize = 4;
const INTERVALS_OFFSET: usize = 260;
const RMASK_OFFSET: usize = 512;
const CMASK_OFFSET: usize = 516;
const INFO_OFFSET: usize = 520;
const MSBITS_OFFSET: usize = 524;
const RATE_NUM_OFFSET: usize = 528;
const RATE_DEN_OFFSET: usize = 532;
const FIFO_SIZE_OFFSET: usize = 536;
const HW_PARAMS_SIZE: usize = 608;

const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_S16_LE: u32 = 2;
const SUBFORMAT_STD: u32 = 0;
const FRAME_BYTES: u64 = 4;
/// A stream that doesn't move for this long is stuck (Linux's limit too).
const STALL_MS: u64 = 10_000;

// What `info` reports: interleaved samples, transferred in blocks, and
// pause is supported.
const INFO_FLAGS: u32 = 0x100 | 0x1_0000 | 0x8_0000;

// The hardware's limits. Periods are whole multiples of 128 bytes (32
// frames), as HD Audio's buffer descriptors want.
const PERIOD_STEP: u64 = 32;
const MIN_PERIOD: u64 = 32;
const MAX_PERIOD: u64 = 16_384;
const MIN_PERIODS: u64 = 2;
const MAX_PERIODS: u64 = 32;
const MAX_BUFFER: u64 = 32_768;

/// A closed or open range of whole numbers, as `struct snd_interval`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Interval {
    min: u64,
    max: u64,
    open_min: bool,
    open_max: bool,
    integer: bool,
    empty: bool,
}

impl Interval {
    fn any() -> Self {
        Self { min: 0, max: u64::from(u32::MAX), open_min: false, open_max: false, integer: false, empty: false }
    }

    fn exact(value: u64) -> Self {
        Self { min: value, max: value, integer: true, ..Self::any() }
    }

    fn range(min: u64, max: u64) -> Self {
        Self { min, max, integer: true, ..Self::any() }
    }

    fn read(bytes: &[u8]) -> Self {
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let flags = word(8);
        Self {
            min: u64::from(word(0)),
            max: u64::from(word(4)),
            open_min: flags & 1 != 0,
            open_max: flags & 2 != 0,
            integer: flags & 4 != 0,
            empty: flags & 8 != 0,
        }
    }

    fn write(&self, bytes: &mut [u8]) {
        let flags = u32::from(self.open_min)
            | u32::from(self.open_max) << 1
            | u32::from(self.integer) << 2
            | u32::from(self.empty) << 3;
        bytes[0..4].copy_from_slice(&(self.min.min(u64::from(u32::MAX)) as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&(self.max.min(u64::from(u32::MAX)) as u32).to_le_bytes());
        bytes[8..12].copy_from_slice(&flags.to_le_bytes());
    }

    fn single(&self) -> Option<u64> {
        (!self.empty && self.min == self.max && !self.open_min && !self.open_max).then_some(self.min)
    }

    fn normalize(&mut self) {
        if self.integer {
            if self.open_min {
                self.min += 1;
                self.open_min = false;
            }
            if self.open_max && self.max > 0 {
                self.max -= 1;
                self.open_max = false;
            }
        }
        if self.min > self.max || (self.min == self.max && (self.open_min || self.open_max)) {
            self.empty = true;
        }
    }

    /// Narrows to `other`.
    fn refine(&mut self, other: Interval) {
        if self.empty {
            return;
        }
        if other.min > self.min || (other.min == self.min && other.open_min) {
            self.min = other.min;
            self.open_min = other.open_min;
        }
        if other.max < self.max || (other.max == self.max && other.open_max) {
            self.max = other.max;
            self.open_max = other.open_max;
        }
        self.integer |= other.integer;
        self.empty |= other.empty;
        self.normalize();
    }

    /// The range of `numerator / denominator` over x in this interval, for
    /// integer results (rounding inward).
    fn divided(&self, numerator: u64, denominator: u64) -> Interval {
        Interval::range((self.min * numerator).div_ceil(denominator), self.max * numerator / denominator)
    }

    /// The range of `x * numerator / denominator`, for real-valued results
    /// (rounding outward, open where inexact).
    fn scaled(&self, numerator: u64, denominator: u64) -> Interval {
        let low = self.min * numerator;
        let high = self.max * numerator;
        Interval {
            min: low / denominator,
            max: high.div_ceil(denominator),
            open_min: self.open_min || !low.is_multiple_of(denominator),
            open_max: self.open_max || !high.is_multiple_of(denominator),
            integer: false,
            empty: self.empty,
        }
    }
}

/// The parameters of a `struct snd_pcm_hw_params` this code looks at.
struct HwParams {
    bytes: Vec<u8>,
}

impl HwParams {
    fn mask(&self, index: usize) -> [u32; 8] {
        let at = MASKS_OFFSET + index * 32;
        core::array::from_fn(|word| {
            u32::from_le_bytes(self.bytes[at + word * 4..at + word * 4 + 4].try_into().unwrap())
        })
    }

    fn set_mask(&mut self, index: usize, mask: [u32; 8]) {
        let at = MASKS_OFFSET + index * 32;
        for (word, value) in mask.iter().enumerate() {
            self.bytes[at + word * 4..at + word * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn interval(&self, index: usize) -> Interval {
        let at = INTERVALS_OFFSET + (index - SAMPLE_BITS) * 12;
        Interval::read(&self.bytes[at..at + 12])
    }

    fn set_interval(&mut self, index: usize, interval: Interval) {
        let at = INTERVALS_OFFSET + (index - SAMPLE_BITS) * 12;
        interval.write(&mut self.bytes[at..at + 12]);
    }

    fn set_u32(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}

/// Keeps only the bit for `value` in a mask, if it's set.
fn only(mask: [u32; 8], value: u32) -> [u32; 8] {
    let mut result = [0; 8];
    let (word, bit) = ((value / 32) as usize, value % 32);
    result[word] = mask[word] & (1 << bit);
    result
}

/// Narrows hardware parameters to what the device can do and what the
/// requested ones imply for each other. Returns which changed (`cmask`),
/// or `EINVAL` if nothing fits.
fn refine(params: &mut HwParams, rates: &[u32]) -> Result<u32> {
    let mut changed = 0u32;
    for (index, value) in [(ACCESS, ACCESS_RW_INTERLEAVED), (FORMAT, FORMAT_S16_LE), (SUBFORMAT, SUBFORMAT_STD)] {
        let before = params.mask(index);
        let after = only(before, value);
        if after.iter().all(|&word| word == 0) {
            return Err(Errno::EINVAL);
        }
        if after != before {
            changed |= 1 << index;
            params.set_mask(index, after);
        }
    }
    let original: Vec<Interval> = (SAMPLE_BITS..=TICK_TIME).map(|index| params.interval(index)).collect();
    let get = |index: usize| params.interval(index);
    let mut sample_bits = get(SAMPLE_BITS);
    let mut frame_bits = get(FRAME_BITS);
    let mut channels = get(CHANNELS);
    let mut rate = get(RATE);
    let mut period_time = get(PERIOD_TIME);
    let mut period_size = get(PERIOD_SIZE);
    let mut period_bytes = get(PERIOD_BYTES);
    let mut periods = get(PERIODS);
    let mut buffer_time = get(BUFFER_TIME);
    let mut buffer_size = get(BUFFER_SIZE);
    let mut buffer_bytes = get(BUFFER_BYTES);

    sample_bits.refine(Interval::exact(16));
    frame_bits.refine(Interval::exact(32));
    channels.refine(Interval::exact(2));
    rate.refine(Interval::range(u64::from(rates[0]), u64::from(*rates.last().unwrap())));
    period_size.refine(Interval::range(MIN_PERIOD, MAX_PERIOD));
    periods.refine(Interval::range(MIN_PERIODS, MAX_PERIODS));
    buffer_size.refine(Interval::range(MIN_PERIOD * MIN_PERIODS, MAX_BUFFER));

    for _ in 0..16 {
        let before = (rate, period_time, period_size, period_bytes, periods, buffer_time, buffer_size, buffer_bytes);
        // Rates: only the ones offered.
        if !rate.empty {
            let fits = |r: &&u32| {
                let r = u64::from(**r);
                (r > rate.min || (r == rate.min && !rate.open_min))
                    && (r < rate.max || (r == rate.max && !rate.open_max))
            };
            match (rates.iter().find(fits), rates.iter().rev().find(fits)) {
                (Some(&low), Some(&high)) => rate.refine(Interval::range(u64::from(low), u64::from(high))),
                _ => rate.empty = true,
            }
        }
        if rate.empty {
            break;
        }
        // Sizes, bytes and times describe the same thing.
        period_size.refine(period_bytes.divided(1, FRAME_BYTES));
        buffer_size.refine(buffer_bytes.divided(1, FRAME_BYTES));
        period_size.refine(Interval::range(
            (period_time.min * rate.min).div_ceil(1_000_000),
            period_time.max.saturating_mul(rate.max) / 1_000_000,
        ));
        buffer_size.refine(Interval::range(
            (buffer_time.min * rate.min).div_ceil(1_000_000),
            buffer_time.max.saturating_mul(rate.max) / 1_000_000,
        ));
        // Periods are whole multiples of the step.
        period_size.refine(Interval::range(
            period_size.min.next_multiple_of(PERIOD_STEP),
            period_size.max / PERIOD_STEP * PERIOD_STEP,
        ));
        // buffer = period size × periods.
        buffer_size.refine(Interval::range(period_size.min * periods.min, period_size.max * periods.max));
        if periods.max > 0 && periods.min > 0 {
            period_size.refine(Interval::range(buffer_size.min.div_ceil(periods.max), buffer_size.max / periods.min));
        }
        if period_size.max > 0 && period_size.min > 0 {
            periods
                .refine(Interval::range(buffer_size.min.div_ceil(period_size.max), buffer_size.max / period_size.min));
        }
        period_bytes.refine(Interval::range(period_size.min * FRAME_BYTES, period_size.max * FRAME_BYTES));
        buffer_bytes.refine(Interval::range(buffer_size.min * FRAME_BYTES, buffer_size.max * FRAME_BYTES));
        // Times: a size over the rate (period_time = size / rate, in µs).
        let over_rate = |size: &Interval| {
            let low = Interval { max: size.min, ..*size }.scaled(1_000_000, rate.max.max(1));
            let high = Interval { min: size.max, ..*size }.scaled(1_000_000, rate.min.max(1));
            Interval { min: low.min, open_min: low.open_min, max: high.max, open_max: high.open_max, ..Interval::any() }
        };
        period_time.refine(over_rate(&period_size));
        buffer_time.refine(over_rate(&buffer_size));
        let after = (rate, period_time, period_size, period_bytes, periods, buffer_time, buffer_size, buffer_bytes);
        if before == after {
            break;
        }
    }

    let refined = [
        sample_bits,
        frame_bits,
        channels,
        rate,
        period_time,
        period_size,
        period_bytes,
        periods,
        buffer_time,
        buffer_size,
        buffer_bytes,
        original[TICK_TIME - SAMPLE_BITS],
    ];
    for (offset, interval) in refined.iter().enumerate() {
        if interval.empty {
            return Err(Errno::EINVAL);
        }
        if *interval != original[offset] {
            changed |= 1 << (offset + SAMPLE_BITS);
            params.set_interval(offset + SAMPLE_BITS, *interval);
        }
    }
    params.set_u32(INFO_OFFSET, INFO_FLAGS);
    params.set_u32(MSBITS_OFFSET, 16);
    match rate.single() {
        Some(single) => {
            params.set_u32(RATE_NUM_OFFSET, single as u32);
            params.set_u32(RATE_DEN_OFFSET, 1);
        }
        None => {
            params.set_u32(RATE_NUM_OFFSET, 0);
            params.set_u32(RATE_DEN_OFFSET, 0);
        }
    }
    params.bytes[FIFO_SIZE_OFFSET..FIFO_SIZE_OFFSET + 8].fill(0);
    Ok(changed)
}

/// Picks one value for every parameter still open, the way Linux does
/// (the lowest rate and period, then the largest buffer).
fn choose(params: &mut HwParams, rates: &[u32]) -> Result<(u32, u64, u64)> {
    refine(params, rates)?;
    let rate = params.interval(RATE).min;
    params.set_interval(RATE, Interval::exact(rate));
    refine(params, rates)?;
    let period = params.interval(PERIOD_SIZE).min;
    params.set_interval(PERIOD_SIZE, Interval::exact(period));
    refine(params, rates)?;
    let periods = params.interval(PERIODS);
    let count = (params.interval(BUFFER_SIZE).max / period).clamp(periods.min, periods.max);
    params.set_interval(PERIODS, Interval::exact(count));
    params.set_interval(BUFFER_SIZE, Interval::exact(period * count));
    refine(params, rates)?;
    Ok((rate as u32, period, count))
}

/// The running state of an open stream.
#[derive(Clone, Copy)]
struct Runtime {
    state: State,
    rate: u32,
    period: u64,
    buffer: u64,
    boundary: u64,
    hw_ptr: u64,
    appl_ptr: u64,
    /// The device's position last seen (frames into the buffer), and when.
    last_position: u64,
    last_update: u64,
    avail_min: u64,
    start_threshold: u64,
    stop_threshold: u64,
    avail_max: u64,
    trigger_ns: u64,
}

impl Runtime {
    fn new() -> Self {
        Runtime {
            state: State::Open,
            rate: 0,
            period: 0,
            buffer: 0,
            boundary: 0,
            hw_ptr: 0,
            appl_ptr: 0,
            last_position: 0,
            last_update: 0,
            avail_min: 1,
            start_threshold: 1,
            stop_threshold: 0,
            avail_max: 0,
            trigger_ns: 0,
        }
    }

    fn distance(&self, to: u64, from: u64) -> u64 {
        (to + self.boundary - from) % self.boundary.max(1)
    }

    /// Frames the program can write (playback) or read (capture) now.
    fn avail(&self, direction: Direction) -> u64 {
        match direction {
            Direction::Playback => self.buffer.saturating_sub(self.distance(self.appl_ptr, self.hw_ptr)),
            Direction::Capture => self.distance(self.hw_ptr, self.appl_ptr),
        }
    }

    /// Frames waiting to be played, or recorded and not yet read.
    fn queued(&self, direction: Direction) -> u64 {
        match direction {
            Direction::Playback => self.distance(self.appl_ptr, self.hw_ptr),
            Direction::Capture => self.distance(self.hw_ptr, self.appl_ptr),
        }
    }
}

/// What to do to the hardware once the runtime lock is released.
#[derive(PartialEq, Eq)]
enum Then {
    Nothing,
    Stop,
    Start,
}

pub struct Pcm {
    card: usize,
    hardware: Arc<dyn Hardware>,
    direction: Direction,
    runtime: SpinLock<Runtime>,
    /// The hardware's period events, also notified on state changes.
    events: Arc<PollSource>,
    /// Opened (only one program at a time, as with Linux's `hw` devices).
    busy: AtomicBool,
}

impl Pcm {
    pub fn new(card: usize, hardware: Arc<dyn Hardware>) -> Arc<Self> {
        let events = hardware.events();
        Arc::new(Self {
            card,
            direction: hardware.direction(),
            hardware,
            runtime: SpinLock::new(Runtime::new()),
            events,
            busy: AtomicBool::new(false),
        })
    }

    /// Brings the hardware pointer up to date from the device's position,
    /// and notices xruns and the end of draining.
    fn update(&self, runtime: &mut Runtime) -> Then {
        if !matches!(runtime.state, State::Running | State::Draining) || runtime.buffer == 0 {
            return Then::Nothing;
        }
        let position = (self.hardware.position() as u64 / FRAME_BYTES) % runtime.buffer;
        let now = time::uptime_nanos();
        let mut moved = (position + runtime.buffer - runtime.last_position) % runtime.buffer;
        // If more than a buffer's time passed since the last look, the
        // position wrapped unseen: count the whole buffers from the clock.
        let elapsed = (now - runtime.last_update) as u128 * u128::from(runtime.rate) / 1_000_000_000;
        if elapsed as u64 > moved + runtime.buffer / 2 {
            let laps = (elapsed as u64 - moved + runtime.buffer / 2) / runtime.buffer;
            moved += laps * runtime.buffer;
        }
        runtime.last_position = position;
        runtime.last_update = now;
        if self.direction == Direction::Capture && self.card == 0 && moved > 0 {
            self.record_remote(runtime.hw_ptr % runtime.buffer, moved.min(runtime.buffer), runtime);
        }
        runtime.hw_ptr = (runtime.hw_ptr + moved) % runtime.boundary;

        let queued = runtime.queued(self.direction);
        let avail = runtime.avail(self.direction);
        runtime.avail_max = runtime.avail_max.max(avail);
        let overrun = match self.direction {
            // The hardware passed what was written.
            Direction::Playback => queued > runtime.buffer,
            Direction::Capture => queued > runtime.buffer,
        };
        if runtime.state == State::Draining {
            if self.direction == Direction::Playback && (queued == 0 || overrun) {
                runtime.state = State::Setup;
                return Then::Stop;
            }
            return Then::Nothing;
        }
        let threshold_reached = runtime.stop_threshold < runtime.boundary && avail >= runtime.stop_threshold;
        if overrun || threshold_reached {
            if overrun && self.direction == Direction::Playback {
                // Played past the end: what was written has all played.
                runtime.hw_ptr = runtime.appl_ptr;
            }
            runtime.state = State::Xrun;
            return Then::Stop;
        }
        Then::Nothing
    }

    /// Overwrites `count` frames just recorded, from `offset` in the ring,
    /// with a microphone somewhere else, if there is one ([`super::remote`]).
    fn record_remote(&self, offset: u64, count: u64, runtime: &Runtime) {
        let mut bytes = alloc::vec![0u8; (count * FRAME_BYTES) as usize];
        if !super::remote::fill(runtime.rate, &mut bytes) {
            return;
        }
        let first = (count.min(runtime.buffer - offset) * FRAME_BYTES) as usize;
        self.hardware.write_buffer((offset * FRAME_BYTES) as usize, &bytes[..first]);
        if first < bytes.len() {
            self.hardware.write_buffer(0, &bytes[first..]);
        }
    }

    /// Whether the stream is recording or playing now.
    pub fn running(&self) -> bool {
        matches!(self.runtime.lock().state, State::Running | State::Draining)
    }

    fn act(&self, then: Then) {
        match then {
            Then::Nothing => {}
            Then::Stop => self.hardware.stop(),
            Then::Start => self.hardware.start(),
        }
        if then != Then::Nothing {
            self.events.notify();
        }
    }

    /// Starts the device from the prepared state.
    fn start(&self, runtime: &mut Runtime) -> Then {
        runtime.state = State::Running;
        runtime.last_position = 0;
        runtime.last_update = time::uptime_nanos();
        runtime.trigger_ns = runtime.last_update;
        Then::Start
    }

    /// For sending what plays somewhere else as well (a Bluetooth
    /// headset): appends to `out` the frames written and about to play,
    /// from `cursor` (a stream position the caller keeps) to `lead` frames
    /// past the device's position. Frames already played may have been
    /// overwritten, so a cursor that fell behind skips ahead to the device.
    /// Returns the rate, or `None` (and forgets the cursor) when nothing is
    /// playing.
    pub fn tap(&self, cursor: &mut Option<u64>, lead: u64, out: &mut Vec<u8>) -> Option<u32> {
        let (then, region) = {
            let mut runtime = self.runtime.lock();
            let then = self.update(&mut runtime);
            let playing = matches!(runtime.state, State::Running | State::Draining) && runtime.buffer > 0;
            let region = if playing && self.direction == Direction::Playback {
                let device = runtime.hw_ptr;
                let queued = runtime.queued(self.direction);
                let start = cursor.filter(|&at| runtime.distance(at, device) <= queued).unwrap_or(device);
                let count = queued.min(lead).saturating_sub(runtime.distance(start, device));
                *cursor = Some((start + count) % runtime.boundary);
                Some((start % runtime.buffer, count, runtime.buffer, runtime.rate))
            } else {
                *cursor = None;
                None
            };
            (then, region)
        };
        self.act(then);
        let (offset, count, buffer, rate) = region?;
        // The written, unplayed part of the ring doesn't change under us.
        let first = count.min(buffer - offset);
        let at = out.len();
        out.resize(at + (count * FRAME_BYTES) as usize, 0);
        let split = at + (first * FRAME_BYTES) as usize;
        self.hardware.read_buffer((offset * FRAME_BYTES) as usize, &mut out[at..split]);
        if split < out.len() {
            self.hardware.read_buffer(0, &mut out[split..]);
        }
        Some(rate)
    }

    fn info(&self, address: u64) -> Result<u64> {
        let mut info = [0u8; 288];
        info[8..12].copy_from_slice(&(if self.direction == Direction::Playback { 0u32 } else { 1 }).to_le_bytes());
        info[12..16].copy_from_slice(&(self.card as u32).to_le_bytes());
        let name = b"eliOS HD Audio";
        info[16..16 + name.len()].copy_from_slice(name);
        info[80..80 + name.len()].copy_from_slice(name);
        let subname = b"subdevice #0";
        info[160..160 + subname.len()].copy_from_slice(subname);
        info[200..204].copy_from_slice(&1u32.to_le_bytes());
        let avail = u32::from(!self.busy.load(Ordering::Relaxed));
        info[204..208].copy_from_slice(&avail.to_le_bytes());
        user::write_bytes(address, &info)?;
        Ok(0)
    }

    fn hw_params(&self, address: u64, set: bool) -> Result<u64> {
        let mut params = HwParams { bytes: user::read_bytes(address, HW_PARAMS_SIZE)? };
        let rates = self.hardware.rates();
        if !set {
            let changed = refine(&mut params, rates)?;
            params.set_u32(RMASK_OFFSET, 0);
            params.set_u32(CMASK_OFFSET, changed);
            user::write_bytes(address, &params.bytes)?;
            return Ok(0);
        }
        if matches!(self.runtime.lock().state, State::Running | State::Draining | State::Paused) {
            return Err(Errno::EBADFD);
        }
        let (rate, period, periods) = choose(&mut params, rates)?;
        self.hardware.stop();
        self.hardware.configure(rate, (period * FRAME_BYTES) as usize, periods as usize)?;
        let buffer = period * periods;
        let mut boundary = buffer;
        while boundary * 2 <= i64::MAX as u64 - buffer {
            boundary *= 2;
        }
        *self.runtime.lock() = Runtime {
            state: State::Setup,
            rate,
            period,
            buffer,
            boundary,
            avail_min: period,
            start_threshold: 1,
            stop_threshold: buffer,
            ..Runtime::new()
        };
        params.set_u32(RMASK_OFFSET, 0);
        params.set_u32(CMASK_OFFSET, 0);
        user::write_bytes(address, &params.bytes)?;
        Ok(0)
    }

    fn sw_params(&self, address: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(address, 136)?;
        let word = |bytes: &[u8], at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let mut runtime = self.runtime.lock();
        if runtime.state == State::Open {
            return Err(Errno::EBADFD);
        }
        runtime.avail_min = word(&bytes, 16).max(1);
        runtime.start_threshold = word(&bytes, 32);
        runtime.stop_threshold = word(&bytes, 40);
        let boundary = runtime.boundary;
        drop(runtime);
        bytes[64..72].copy_from_slice(&boundary.to_le_bytes());
        user::write_bytes(address, &bytes)?;
        Ok(0)
    }

    fn timestamp(nanos: u64) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&(nanos / 1_000_000_000).to_le_bytes());
        bytes[8..].copy_from_slice(&(nanos % 1_000_000_000).to_le_bytes());
        bytes
    }

    fn status(&self, address: u64) -> Result<u64> {
        let (then, runtime) = {
            let mut runtime = self.runtime.lock();
            let then = self.update(&mut runtime);
            let snapshot = *runtime;
            runtime.avail_max = 0;
            (then, snapshot)
        };
        self.act(then);
        let mut status = [0u8; 152];
        status[0..4].copy_from_slice(&(runtime.state as u32).to_le_bytes());
        status[8..24].copy_from_slice(&Self::timestamp(runtime.trigger_ns));
        status[24..40].copy_from_slice(&Self::timestamp(time::uptime_nanos()));
        status[40..48].copy_from_slice(&runtime.appl_ptr.to_le_bytes());
        status[48..56].copy_from_slice(&runtime.hw_ptr.to_le_bytes());
        status[56..64].copy_from_slice(&(runtime.queued(self.direction) as i64).to_le_bytes());
        status[64..72].copy_from_slice(&runtime.avail(self.direction).to_le_bytes());
        status[72..80].copy_from_slice(&runtime.avail_max.to_le_bytes());
        user::write_bytes(address, &status)?;
        Ok(0)
    }

    fn sync_ptr(&self, address: u64) -> Result<u64> {
        const HWSYNC_FLAG: u32 = 1;
        const APPL_FLAG: u32 = 2;
        const AVAIL_MIN_FLAG: u32 = 4;
        let mut bytes = user::read_bytes(address, 136)?;
        let flags = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let appl = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
        let avail_min = u64::from_le_bytes(bytes[80..88].try_into().unwrap());
        let (then, runtime) = {
            let mut runtime = self.runtime.lock();
            let then = if flags & HWSYNC_FLAG != 0 { self.update(&mut runtime) } else { Then::Nothing };
            if flags & APPL_FLAG == 0 && runtime.boundary > 0 {
                runtime.appl_ptr = appl % runtime.boundary;
            }
            if flags & AVAIL_MIN_FLAG == 0 {
                runtime.avail_min = avail_min.max(1);
            }
            (then, *runtime)
        };
        self.act(then);
        bytes[8..12].copy_from_slice(&(runtime.state as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&runtime.hw_ptr.to_le_bytes());
        bytes[24..40].copy_from_slice(&Self::timestamp(time::uptime_nanos()));
        bytes[72..80].copy_from_slice(&runtime.appl_ptr.to_le_bytes());
        bytes[80..88].copy_from_slice(&runtime.avail_min.to_le_bytes());
        user::write_bytes(address, &bytes)?;
        Ok(0)
    }

    fn prepare(&self) -> Result<u64> {
        {
            let runtime = self.runtime.lock();
            if runtime.state == State::Open {
                return Err(Errno::EBADFD);
            }
        }
        self.hardware.prepare();
        let mut runtime = self.runtime.lock();
        runtime.state = State::Prepared;
        runtime.hw_ptr = 0;
        runtime.appl_ptr = 0;
        runtime.last_position = 0;
        runtime.avail_max = 0;
        drop(runtime);
        self.events.notify();
        Ok(0)
    }

    fn start_request(&self) -> Result<u64> {
        let then = {
            let mut runtime = self.runtime.lock();
            if runtime.state != State::Prepared {
                return Err(Errno::EBADFD);
            }
            if self.direction == Direction::Playback && runtime.queued(self.direction) == 0 {
                return Err(Errno::EPIPE);
            }
            self.start(&mut runtime)
        };
        self.act(then);
        Ok(0)
    }

    fn drop_stream(&self) -> Result<u64> {
        {
            let mut runtime = self.runtime.lock();
            if runtime.state == State::Open {
                return Err(Errno::EBADFD);
            }
            runtime.state = State::Setup;
        }
        self.act(Then::Stop);
        Ok(0)
    }

    fn drain(&self, nonblocking: bool) -> Result<u64> {
        let then = {
            let mut runtime = self.runtime.lock();
            match runtime.state {
                State::Open => return Err(Errno::EBADFD),
                State::Setup | State::Xrun => {
                    runtime.state = State::Setup;
                    Then::Stop
                }
                State::Prepared if self.direction == Direction::Playback && runtime.queued(self.direction) > 0 => {
                    let then = self.start(&mut runtime);
                    runtime.state = State::Draining;
                    then
                }
                State::Running | State::Paused if self.direction == Direction::Playback => {
                    runtime.state = State::Draining;
                    Then::Nothing
                }
                _ => {
                    runtime.state = State::Setup;
                    Then::Stop
                }
            }
        };
        self.act(then);
        if self.runtime.lock().state != State::Draining {
            return Ok(0);
        }
        if nonblocking {
            return Err(Errno::EAGAIN);
        }
        // The buffer holds well under a second, so a drain that takes
        // longer than the stall limit is stuck.
        let draining_since = time::ticks();
        loop {
            let deadline = self.wait_deadline();
            let then = self.events.queue().wait_until_deadline(Some(deadline), || {
                if process::signal_pending() {
                    return Some(Err(Errno::EINTR));
                }
                let mut runtime = self.runtime.lock();
                let then = self.update(&mut runtime);
                (runtime.state != State::Draining).then_some(Ok(then))
            });
            match then {
                Some(Ok(then)) => {
                    self.act(then);
                    return Ok(0);
                }
                Some(Err(error)) => return Err(error),
                None if time::ticks() - draining_since > STALL_MS => {
                    warn!("sound: card {}: the {:?} stream is stuck", self.card, self.direction);
                    self.runtime.lock().state = State::Setup;
                    self.act(Then::Stop);
                    return Err(Errno::EIO);
                }
                None => {}
            }
        }
    }

    fn pause(&self, push: bool) -> Result<u64> {
        let mut runtime = self.runtime.lock();
        match (runtime.state, push) {
            (State::Running, true) => runtime.state = State::Paused,
            (State::Paused, false) => {
                runtime.state = State::Running;
                runtime.last_update = time::uptime_nanos();
            }
            _ => return Err(Errno::EBADFD),
        }
        drop(runtime);
        self.hardware.pause(push);
        self.events.notify();
        Ok(0)
    }

    /// Moves the application pointer back (`rewind`) or forward over frames
    /// not yet played or read. Returns how far it moved.
    fn shift(&self, frames: u64, forward: bool) -> Result<u64> {
        let mut runtime = self.runtime.lock();
        if matches!(runtime.state, State::Open | State::Setup) {
            return Err(Errno::EBADFD);
        }
        let then = self.update(&mut runtime);
        let limit = match (self.direction, forward) {
            (Direction::Playback, false) | (Direction::Capture, true) => runtime.queued(self.direction),
            _ => runtime.avail(self.direction),
        };
        let moved = frames.min(limit);
        runtime.appl_ptr = if forward {
            (runtime.appl_ptr + moved) % runtime.boundary
        } else {
            (runtime.appl_ptr + runtime.boundary - moved) % runtime.boundary
        };
        drop(runtime);
        self.act(then);
        Ok(moved)
    }

    /// How long to wait for the next period at most, in case an interrupt
    /// is lost: two periods.
    fn wait_deadline(&self) -> u64 {
        let runtime = self.runtime.lock();
        let period_ms = if runtime.rate == 0 { 10 } else { runtime.period * 1000 / u64::from(runtime.rate) };
        time::ticks() + 2 * period_ms + 2
    }

    /// `WRITEI_FRAMES` and `READI_FRAMES`: moves up to `frames` frames
    /// between the program's buffer and the ring.
    fn transfer(&self, buffer: u64, frames: u64, nonblocking: bool) -> Result<u64> {
        let mut done = 0;
        while done < frames {
            // Wait for room (or data), but like Linux, give up with EIO if
            // the device doesn't move for 10 seconds (it's stuck).
            let waiting_since = time::ticks();
            let ready = loop {
                let (avail, then) = {
                    let mut runtime = self.runtime.lock();
                    let then = self.update(&mut runtime);
                    match runtime.state {
                        State::Xrun => {
                            drop(runtime);
                            self.act(then);
                            return if done > 0 { Ok(done) } else { Err(Errno::EPIPE) };
                        }
                        State::Open | State::Setup | State::Draining => return Err(Errno::EBADFD),
                        _ => {}
                    }
                    // Capture starts when the program first reads.
                    let then = if runtime.state == State::Prepared && self.direction == Direction::Capture {
                        self.start(&mut runtime)
                    } else {
                        then
                    };
                    (runtime.avail(self.direction), then)
                };
                self.act(then);
                if avail > 0 {
                    break avail;
                }
                if nonblocking {
                    return if done > 0 { Ok(done) } else { Err(Errno::EAGAIN) };
                }
                if time::ticks() - waiting_since > STALL_MS {
                    warn!("sound: card {}: the {:?} stream is stuck", self.card, self.direction);
                    return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
                }
                let deadline = self.wait_deadline();
                let woken = self.events.queue().wait_until_deadline(Some(deadline), || {
                    if process::signal_pending() {
                        return Some(Err(Errno::EINTR));
                    }
                    let mut runtime = self.runtime.lock();
                    let then = self.update(&mut runtime);
                    let ready = runtime.avail(self.direction) > 0 || runtime.state != State::Running;
                    drop(runtime);
                    self.act(then);
                    ready.then_some(Ok(()))
                });
                if let Some(Err(error)) = woken {
                    return if done > 0 { Ok(done) } else { Err(error) };
                }
            };
            let count = ready.min(frames - done);
            let (offset, buffer_frames) = {
                let runtime = self.runtime.lock();
                (runtime.appl_ptr % runtime.buffer, runtime.buffer)
            };
            // Up to the ring's end, then from its start.
            let first = count.min(buffer_frames - offset);
            let address = buffer + done * FRAME_BYTES;
            match self.direction {
                Direction::Playback => {
                    let data = user::read_bytes(address, (count * FRAME_BYTES) as usize)?;
                    let split = (first * FRAME_BYTES) as usize;
                    self.hardware.write_buffer((offset * FRAME_BYTES) as usize, &data[..split]);
                    if split < data.len() {
                        self.hardware.write_buffer(0, &data[split..]);
                    }
                }
                Direction::Capture => {
                    let mut data = alloc::vec![0u8; (count * FRAME_BYTES) as usize];
                    let split = (first * FRAME_BYTES) as usize;
                    self.hardware.read_buffer((offset * FRAME_BYTES) as usize, &mut data[..split]);
                    if split < data.len() {
                        self.hardware.read_buffer(0, &mut data[split..]);
                    }
                    user::write_bytes(address, &data)?;
                }
            }
            let then = {
                let mut runtime = self.runtime.lock();
                runtime.appl_ptr = (runtime.appl_ptr + count) % runtime.boundary;
                if runtime.state == State::Prepared
                    && self.direction == Direction::Playback
                    && runtime.queued(self.direction) >= runtime.start_threshold
                {
                    self.start(&mut runtime)
                } else {
                    Then::Nothing
                }
            };
            self.act(then);
            done += count;
        }
        Ok(done)
    }

    fn xfer(&self, address: u64, nonblocking: bool) -> Result<u64> {
        let buffer: u64 = user::read_value(address + 8)?;
        let frames: u64 = user::read_value(address + 16)?;
        let result = self.transfer(buffer, frames, nonblocking)?;
        user::write_value(address, &(result as i64))?;
        Ok(0)
    }

    fn poll(&self) -> u16 {
        let mut runtime = self.runtime.lock();
        let then = self.update(&mut runtime);
        let ready = if self.direction == Direction::Playback { POLLOUT } else { POLLIN };
        let events =
            match runtime.state {
                State::Running | State::Prepared | State::Paused => {
                    if runtime.avail(self.direction) >= runtime.avail_min { ready } else { 0 }
                }
                State::Draining => 0,
                _ => ready | POLLERR,
            };
        drop(runtime);
        self.act(then);
        events
    }

    fn release(&self) {
        self.hardware.release();
        *self.runtime.lock() = Runtime::new();
        self.busy.store(false, Ordering::Release);
        self.events.notify();
    }
}

/// The device file: each open gets its own [`PcmFile`].
pub struct PcmNode {
    inode: u64,
    pcm: Arc<Pcm>,
}

impl PcmNode {
    pub fn new(pcm: Arc<Pcm>) -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number(), pcm })
    }
}

fn device_metadata(inode: u64, minor: u32) -> Metadata {
    Metadata {
        rdev: 116 << 8 | u64::from(minor),
        gid: AUDIO_GROUP,
        ..Metadata::virtual_file(FileKind::CharDevice, inode, 0o660)
    }
}

impl PcmNode {
    fn minor(&self) -> u32 {
        let base = if self.pcm.direction == Direction::Playback { 16 } else { 24 };
        self.pcm.card as u32 * 32 + base
    }
}

impl Inode for PcmNode {
    fn metadata(&self) -> Metadata {
        device_metadata(self.inode, self.minor())
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        if self.pcm.busy.swap(true, Ordering::AcqRel) {
            return Some(Err(Errno::EBUSY));
        }
        Some(Ok(Arc::new(PcmFile { inode: self.inode, minor: self.minor(), pcm: self.pcm.clone() })))
    }
}

/// An open PCM device.
pub struct PcmFile {
    inode: u64,
    minor: u32,
    pcm: Arc<Pcm>,
}

impl Inode for PcmFile {
    fn metadata(&self) -> Metadata {
        device_metadata(self.inode, self.minor)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn ioctl_with_flags(&self, flags: u32, request: u64, argument: u64) -> Result<u64> {
        let pcm = &self.pcm;
        let nonblocking = flags & fs::O_NONBLOCK != 0;
        match request {
            PVERSION => user::write_value(argument, &VERSION).map(|()| 0),
            INFO => pcm.info(argument),
            TSTAMP | TTSTAMP | USER_PVERSION | LINK | UNLINK => Ok(0),
            HW_REFINE => pcm.hw_params(argument, false),
            HW_PARAMS => pcm.hw_params(argument, true),
            HW_FREE => {
                if matches!(pcm.runtime.lock().state, State::Running | State::Draining | State::Paused) {
                    return Err(Errno::EBADFD);
                }
                pcm.hardware.release();
                *pcm.runtime.lock() = Runtime::new();
                Ok(0)
            }
            SW_PARAMS => pcm.sw_params(argument),
            STATUS | STATUS_EXT => pcm.status(argument),
            DELAY => {
                let mut runtime = pcm.runtime.lock();
                let then = pcm.update(&mut runtime);
                let delay = runtime.queued(pcm.direction) as i64;
                drop(runtime);
                pcm.act(then);
                user::write_value(argument, &delay).map(|()| 0)
            }
            HWSYNC => {
                let mut runtime = pcm.runtime.lock();
                let then = pcm.update(&mut runtime);
                drop(runtime);
                pcm.act(then);
                Ok(0)
            }
            SYNC_PTR => pcm.sync_ptr(argument),
            PREPARE => pcm.prepare(),
            RESET => {
                let mut runtime = pcm.runtime.lock();
                let _ = pcm.update(&mut runtime);
                runtime.appl_ptr = runtime.hw_ptr;
                Ok(0)
            }
            START => pcm.start_request(),
            DROP => pcm.drop_stream(),
            DRAIN => pcm.drain(nonblocking),
            PAUSE => {
                let push: i32 = user::read_value(argument)?;
                pcm.pause(push != 0)
            }
            RESUME => Err(Errno::ENOSYS),
            XRUN => {
                pcm.runtime.lock().state = State::Xrun;
                pcm.act(Then::Stop);
                Ok(0)
            }
            REWIND => pcm.shift(user::read_value(argument)?, false),
            FORWARD => pcm.shift(user::read_value(argument)?, true),
            WRITEI_FRAMES if pcm.direction == Direction::Playback => pcm.xfer(argument, nonblocking),
            READI_FRAMES if pcm.direction == Direction::Capture => pcm.xfer(argument, nonblocking),
            _ => Err(Errno::ENOTTY),
        }
    }

    fn poll(&self, _flags: u32) -> u16 {
        self.pcm.poll()
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.pcm.events.clone());
        true
    }

    fn on_close(&self, _flags: u32) {
        self.pcm.release();
    }
}

impl Card {
    /// The PCM device for a stream direction, if the card has it.
    pub fn pcm(&self, direction: Direction) -> Option<&Arc<Pcm>> {
        match direction {
            Direction::Playback => Some(&self.playback),
            Direction::Capture => self.capture.as_ref(),
        }
    }
}

impl Pcm {
    pub fn card(&self) -> usize {
        self.card
    }

    /// Notified at every period's end and state change.
    pub fn events(&self) -> Arc<PollSource> {
        self.events.clone()
    }

    /// How many periods the device has played or recorded since the stream
    /// was prepared (wrapping), if it's running: what the stream's timer
    /// ([`super::timer`]) counts.
    pub fn periods_elapsed(&self) -> Option<u64> {
        let mut runtime = self.runtime.lock();
        let then = self.update(&mut runtime);
        let count = match runtime.state {
            State::Running | State::Draining if runtime.period > 0 => Some(runtime.hw_ptr / runtime.period),
            _ => None,
        };
        drop(runtime);
        self.act(then);
        count
    }

    /// A period's length in nanoseconds (0 before the parameters are set).
    pub fn period_nanos(&self) -> u64 {
        let runtime = self.runtime.lock();
        if runtime.rate == 0 { 0 } else { runtime.period * 1_000_000_000 / u64::from(runtime.rate) }
    }

    /// Fills a `struct snd_pcm_info` for the control device.
    pub fn write_info(&self, address: u64) -> Result<u64> {
        self.info(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_params() -> HwParams {
        let mut params = HwParams { bytes: alloc::vec![0; HW_PARAMS_SIZE] };
        for mask in 0..3 {
            params.set_mask(mask, [u32::MAX; 8]);
        }
        for index in SAMPLE_BITS..=TICK_TIME {
            params.set_interval(index, Interval::any());
        }
        params
    }

    crate::ktest! {
        fn hw_params_refine_to_what_the_hardware_does() {
            let mut params = any_params();
            refine(&mut params, &[44_100, 48_000]).unwrap();
            assert_eq!(params.mask(FORMAT)[0], 1 << FORMAT_S16_LE);
            assert_eq!(params.interval(CHANNELS).single(), Some(2));
            let rate = params.interval(RATE);
            assert_eq!((rate.min, rate.max), (44_100, 48_000));
            let period = params.interval(PERIOD_SIZE);
            assert_eq!((period.min, period.max), (MIN_PERIOD, MAX_PERIOD));
            assert_eq!(params.interval(PERIOD_BYTES).min, MIN_PERIOD * 4);
        }

        fn hw_params_follow_a_requested_latency() {
            // 48 kHz with a buffer of about 100 ms, as ALSA's library asks.
            let mut params = any_params();
            params.set_interval(RATE, Interval::exact(48_000));
            params.set_interval(BUFFER_TIME, Interval::range(99_000, 101_000));
            refine(&mut params, &[44_100, 48_000]).unwrap();
            let buffer = params.interval(BUFFER_SIZE);
            assert!(buffer.min >= 4752 && buffer.max <= 4848, "{buffer:?}");
            let (rate, period, periods) = choose(&mut params, &[44_100, 48_000]).unwrap();
            assert_eq!(rate, 48_000);
            assert!(period.is_multiple_of(PERIOD_STEP));
            assert!((4752..=4848).contains(&(period * periods)), "{period} x {periods}");
        }

        fn hw_params_reject_what_cant_be_done() {
            let mut params = any_params();
            params.set_interval(RATE, Interval::range(45_000, 47_000));
            assert_eq!(refine(&mut params, &[44_100, 48_000]).err(), Some(Errno::EINVAL));
            let mut params = any_params();
            params.set_mask(FORMAT, only([u32::MAX; 8], 10)); // S32_LE only
            assert_eq!(refine(&mut params, &[44_100, 48_000]).err(), Some(Errno::EINVAL));
        }
    }
}
