//! Files that carry events rather than data: `eventfd` (a counter one
//! thread or process bumps to wake another) and `timerfd` (a timer read
//! like a file). Event loops (GLib's, libwayland's, Firefox's) wait for
//! these alongside sockets and pipes with `poll` or `epoll`.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{FileKind, Inode, Metadata, POLLIN, POLLOUT, ramfs};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;
use crate::task::WaitQueue;
use crate::{process, time};

/// Woken when any eventfd or timerfd changes.
static CHANGED: WaitQueue = WaitQueue::new();

fn changed() {
    CHANGED.wake_all();
    super::notify_poll();
}

/// An eventfd: a 64-bit counter. Writes add to it; a read returns it and
/// resets it to 0 (or, in semaphore mode, returns 1 and decrements), and
/// waits while it's 0.
pub struct EventFd {
    inode: u64,
    counter: SpinLock<u64>,
    semaphore: bool,
}

impl EventFd {
    pub fn new(initial: u64, semaphore: bool) -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number(), counter: SpinLock::new(initial), semaphore })
    }
}

/// The counter's maximum: one less than all ones, so a write can always
/// tell "full" apart.
const COUNTER_MAX: u64 = u64::MAX - 1;

impl Inode for EventFd {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Regular, self.inode, 0o600)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let value = CHANGED.wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut counter = self.counter.lock();
            match *counter {
                0 if nonblocking => Some(Err(Errno::EAGAIN)),
                0 => None,
                value if self.semaphore => {
                    *counter = value - 1;
                    Some(Ok(1))
                }
                value => {
                    *counter = 0;
                    Some(Ok(value))
                }
            }
        })?;
        buf[..8].copy_from_slice(&value.to_ne_bytes());
        changed();
        Ok(8)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
        let add = u64::from_ne_bytes(buf.get(..8).ok_or(Errno::EINVAL)?.try_into().unwrap());
        if add == u64::MAX {
            return Err(Errno::EINVAL);
        }
        CHANGED.wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut counter = self.counter.lock();
            if *counter <= COUNTER_MAX - add {
                *counter += add;
                Some(Ok(()))
            } else if nonblocking {
                Some(Err(Errno::EAGAIN))
            } else {
                None
            }
        })?;
        changed();
        Ok(8)
    }

    fn poll(&self, _flags: u32) -> u16 {
        let counter = *self.counter.lock();
        (if counter > 0 { POLLIN } else { 0 }) | (if counter < COUNTER_MAX { POLLOUT } else { 0 })
    }
}

/// Which clock a timerfd follows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    Realtime,
    Monotonic,
}

impl Clock {
    fn now(self) -> u64 {
        match self {
            Clock::Realtime => time::unix_nanos(),
            Clock::Monotonic => time::uptime_nanos(),
        }
    }
}

/// A timerfd: reading returns how many times it has expired since the last
/// read (waiting for the first), as a 64-bit count.
pub struct TimerFd {
    inode: u64,
    clock: Clock,
    state: SpinLock<TimerState>,
    /// Woken when the timer is set, so a blocked read re-reads its deadline.
    reset: AtomicBool,
}

#[derive(Clone, Copy, Default)]
struct TimerState {
    /// When it next expires, on its clock (0: disarmed).
    due: u64,
    /// Its period in nanoseconds (0: once).
    interval: u64,
    /// Expirations not yet read, counted up to `due`.
    expired: u64,
}

impl TimerState {
    /// Counts expirations up to `now`, moving `due` along.
    fn advance(&mut self, now: u64) {
        if self.due == 0 || now < self.due {
            return;
        }
        match (now - self.due).checked_div(self.interval) {
            None => {
                self.expired += 1;
                self.due = 0;
            }
            Some(missed) => {
                self.expired += missed + 1;
                self.due += (missed + 1) * self.interval;
            }
        }
    }
}

impl TimerFd {
    pub fn new(clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            inode: ramfs::next_inode_number(),
            clock,
            state: SpinLock::new(TimerState::default()),
            reset: AtomicBool::new(false),
        })
    }

    /// Arms (or with `value` 0, disarms) the timer: first expiry after
    /// `value` nanoseconds, or at `value` on its clock if `absolute`, then
    /// every `interval`. Returns the previous (time left, interval).
    pub fn set(&self, value: u64, interval: u64, absolute: bool) -> (u64, u64) {
        let now = self.clock.now();
        let previous = self.get();
        let mut state = self.state.lock();
        state.expired = 0;
        state.interval = interval;
        state.due = match value {
            0 => 0,
            value if absolute => value.max(1),
            value => now + value,
        };
        drop(state);
        self.reset.store(true, Ordering::Release);
        changed();
        previous
    }

    /// (time until the next expiry, interval), in nanoseconds.
    pub fn get(&self) -> (u64, u64) {
        let mut state = self.state.lock();
        let now = self.clock.now();
        state.advance(now);
        let left = if state.due == 0 { 0 } else { state.due.saturating_sub(now).max(1) };
        (left, state.interval)
    }

    /// The timer tick at which it next expires, if armed.
    fn due_tick(&self) -> Option<u64> {
        let state = *self.state.lock();
        (state.due != 0).then(|| time::ticks() + state.due.saturating_sub(self.clock.now()).div_ceil(1_000_000))
    }
}

impl Inode for TimerFd {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Regular, self.inode, 0o600)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        loop {
            {
                let mut state = self.state.lock();
                state.advance(self.clock.now());
                if state.expired > 0 {
                    let count = core::mem::take(&mut state.expired);
                    drop(state);
                    buf[..8].copy_from_slice(&count.to_ne_bytes());
                    super::notify_poll();
                    return Ok(8);
                }
            }
            if nonblocking {
                return Err(Errno::EAGAIN);
            }
            if process::signal_pending() {
                return Err(Errno::EINTR);
            }
            // Sleep until it's due (or it's set again, or a signal).
            self.reset.store(false, Ordering::Release);
            let deadline = self.due_tick();
            CHANGED.wait_until_deadline(deadline, || {
                (process::signal_pending() || self.reset.load(Ordering::Acquire)).then_some(())
            });
        }
    }

    fn poll(&self, _flags: u32) -> u16 {
        let mut state = self.state.lock();
        state.advance(self.clock.now());
        if state.expired > 0 { POLLIN } else { 0 }
    }

    fn poll_deadline(&self) -> Option<u64> {
        self.due_tick()
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}
