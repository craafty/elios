//! The ALSA timer device (`/dev/snd/timer`), as far as programs need it:
//! a timer that ticks at the end of each period of a PCM stream.
//!
//! ALSA's library uses one when a program asks for *period events*
//! (`snd_pcm_sw_params_set_period_event`), as PulseAudio does when it can't
//! schedule by the clock (no mmap, or in a VM). It opens the timer, selects
//! the PCM's timer (class PCM, by card, device and stream), starts it, and
//! then polls the timer's file next to the PCM's: each time it's readable,
//! it reads the queued tick events and lets the program fill or drain a
//! period. Here a timer counts the stream's periods by its hardware
//! pointer, and waits on the PCM's period interrupts.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{AUDIO_GROUP, Direction, pcm::Pcm};
use crate::errno::{Errno, Result};
use crate::fs::{FileKind, Inode, Metadata, POLLIN, PollSource, ramfs};
use crate::process::user;
use crate::sync::SpinLock;
use crate::time;

/// Protocol 2.0.8: suspend/resume events, no `TIMER_START` quirks.
const VERSION: u32 = 0x0002_0008;

const PVERSION: u64 = 0x8004_5400;
const TREAD_OLD: u64 = 0x4004_5402;
const SELECT: u64 = 0x4034_5410;
const INFO: u64 = 0x80E8_5411;
const PARAMS: u64 = 0x4050_5412;
const STATUS: u64 = 0x8060_5414;
const START: u64 = 0x54A0;
const STOP: u64 = 0x54A1;
const CONTINUE: u64 = 0x54A2;
const PAUSE: u64 = 0x54A3;
const TREAD64: u64 = 0x4004_54A4;

/// `SNDRV_TIMER_CLASS_PCM`, and the tick event.
const CLASS_PCM: i32 = 3;
const EVENT_TICK: u32 = 1;

/// The device file: each open is its own timer.
pub struct TimerNode {
    inode: u64,
}

impl TimerNode {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number() })
    }
}

fn metadata(inode: u64) -> Metadata {
    Metadata { rdev: 116 << 8 | 33, gid: AUDIO_GROUP, ..Metadata::virtual_file(FileKind::CharDevice, inode, 0o660) }
}

impl Inode for TimerNode {
    fn metadata(&self) -> Metadata {
        metadata(self.inode)
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        Some(Ok(Arc::new(TimerFile { inode: self.inode, state: SpinLock::new(Timer::default()) })))
    }
}

#[derive(Default)]
struct Timer {
    pcm: Option<Arc<Pcm>>,
    /// Reads give `struct snd_timer_tread` events rather than
    /// `struct snd_timer_read` counts.
    tread: bool,
    running: bool,
    /// The stream's period count when last read.
    seen: u64,
}

impl Timer {
    /// Periods passed since the last read. A stream that stopped, or was
    /// prepared again, counts from 0.
    fn pending(&mut self) -> u64 {
        let Some(pcm) = &self.pcm else { return 0 };
        let Some(count) = pcm.periods_elapsed() else {
            self.seen = 0;
            return 0;
        };
        if count < self.seen {
            self.seen = 0;
        }
        if self.running { count - self.seen } else { 0 }
    }
}

pub struct TimerFile {
    inode: u64,
    state: SpinLock<Timer>,
}

impl TimerFile {
    fn select(&self, address: u64) -> Result<u64> {
        let id: [i32; 5] = user::read_value(address)?;
        let [class, _subclass, card, device, subdevice] = id;
        if class != CLASS_PCM || device != 0 || subdevice >> 1 != 0 {
            return Err(Errno::ENODEV);
        }
        let direction = if subdevice & 1 == 0 { Direction::Playback } else { Direction::Capture };
        let pcm = super::find_pcm(card as usize, direction).ok_or(Errno::ENODEV)?;
        let mut state = self.state.lock();
        state.seen = pcm.periods_elapsed().unwrap_or(0);
        state.pcm = Some(pcm);
        state.running = false;
        Ok(0)
    }
}

impl Inode for TimerFile {
    fn metadata(&self) -> Metadata {
        metadata(self.inode)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn ioctl_with_flags(&self, _flags: u32, request: u64, argument: u64) -> Result<u64> {
        match request {
            PVERSION => user::write_value(argument, &VERSION).map(|()| 0),
            TREAD_OLD | TREAD64 => {
                let on: i32 = user::read_value(argument)?;
                self.state.lock().tread = on != 0;
                Ok(0)
            }
            SELECT => self.select(argument),
            INFO => {
                let state = self.state.lock();
                let pcm = state.pcm.as_ref().ok_or(Errno::EBADFD)?;
                let mut info = [0u8; 232];
                // Flags 0; the card; an ID and name; then the resolution
                // (a period, in nanoseconds) at offset 160.
                info[4..8].copy_from_slice(&(pcm.card() as i32).to_le_bytes());
                info[8..11].copy_from_slice(b"PCM");
                info[72..81].copy_from_slice(b"PCM timer");
                info[160..168].copy_from_slice(&pcm.period_nanos().to_le_bytes());
                drop(state);
                user::write_bytes(argument, &info).map(|()| 0)
            }
            // Auto-start, one tick per event, the queue size and the event
            // filter: every tick is one period here, and the queue is
            // a counter, so there's nothing to keep.
            PARAMS => {
                if self.state.lock().pcm.is_none() {
                    return Err(Errno::EBADFD);
                }
                Ok(0)
            }
            STATUS => {
                let mut state = self.state.lock();
                let mut status = [0u8; 96];
                let now = time::uptime_nanos();
                status[..8].copy_from_slice(&(now / 1_000_000_000).to_le_bytes());
                status[8..16].copy_from_slice(&(now % 1_000_000_000).to_le_bytes());
                if let Some(pcm) = &state.pcm {
                    status[16..20].copy_from_slice(&(pcm.period_nanos() as u32).to_le_bytes());
                }
                status[28..32].copy_from_slice(&(state.pending().min(1) as u32).to_le_bytes());
                drop(state);
                user::write_bytes(argument, &status).map(|()| 0)
            }
            START | CONTINUE => {
                let mut state = self.state.lock();
                let pcm = state.pcm.clone().ok_or(Errno::EBADFD)?;
                if request == START || !state.running {
                    state.seen = pcm.periods_elapsed().unwrap_or(0);
                }
                state.running = true;
                Ok(0)
            }
            STOP | PAUSE => {
                self.state.lock().running = false;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        let pcm = self.state.lock().pcm.clone().ok_or(Errno::EBADFD)?;
        let take = || {
            let mut state = self.state.lock();
            let pending = state.pending();
            if pending == 0 {
                return None;
            }
            state.seen += pending;
            Some((pending, state.tread))
        };
        let (ticks, tread) = match take() {
            Some(taken) => taken,
            None if nonblocking => return Err(Errno::EAGAIN),
            None => pcm
                .events()
                .queue()
                .wait_until(|| {
                    if crate::process::signal_pending() {
                        return Some(None);
                    }
                    take().map(Some)
                })
                .ok_or(Errno::EINTR)?,
        };
        if tread {
            // struct snd_timer_tread: event, timestamp, ticks.
            if buf.len() < 32 {
                return Err(Errno::EINVAL);
            }
            let now = time::uptime_nanos();
            buf[..32].fill(0);
            buf[..4].copy_from_slice(&EVENT_TICK.to_le_bytes());
            buf[8..16].copy_from_slice(&(now / 1_000_000_000).to_le_bytes());
            buf[16..24].copy_from_slice(&(now % 1_000_000_000).to_le_bytes());
            buf[24..28].copy_from_slice(&(ticks as u32).to_le_bytes());
            Ok(32)
        } else {
            // struct snd_timer_read: resolution, ticks.
            if buf.len() < 8 {
                return Err(Errno::EINVAL);
            }
            buf[..4].copy_from_slice(&(pcm.period_nanos() as u32).to_le_bytes());
            buf[4..8].copy_from_slice(&(ticks as u32).to_le_bytes());
            Ok(8)
        }
    }

    fn poll(&self, _flags: u32) -> u16 {
        if self.state.lock().pending() > 0 { POLLIN } else { 0 }
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        match &self.state.lock().pcm {
            Some(pcm) => {
                sources.push(pcm.events());
                true
            }
            None => false,
        }
    }
}
