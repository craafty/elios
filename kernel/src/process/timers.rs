//! Timers that send signals: POSIX timers (`timer_create` and friends),
//! `setitimer` and `alarm`.
//!
//! Every armed timer, of every process, is in one list. A kernel thread
//! sleeps until the earliest one is due, sends its signal, and re-arms it if
//! it repeats. A repeating timer that falls behind fires once and counts the
//! periods it missed (its "overrun"), as POSIX says.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::signal::{self, SI_TIMER, SignalInfo};
use super::{Pid, find};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;
use crate::task::{self, WaitQueue};
use crate::time;

/// How a timer tells its process it fired (`sigevent.sigev_notify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// A signal to the process.
    Signal,
    /// Nothing (the program just reads the timer).
    None,
    /// A signal to one thread (`SIGEV_THREAD_ID`).
    Thread(Pid),
}

/// The timer `setitimer` and `alarm` share (it isn't a POSIX timer, so its
/// id can't clash with theirs).
pub const INTERVAL_TIMER: i32 = -1;

struct Timer {
    process: Pid,
    id: i32,
    signal: u32,
    notify: Notify,
    /// `sigev_value`, passed back in the signal's details.
    value: u64,
    /// When it next fires, in nanoseconds of uptime (`None`: disarmed).
    due: Option<u64>,
    /// Its period, for repeating timers (0: fires once).
    interval: u64,
    overrun: u32,
}

static TIMERS: SpinLock<Vec<Timer>> = SpinLock::new(Vec::new());
static CHANGED: WaitQueue = WaitQueue::new();
static STARTED: AtomicBool = AtomicBool::new(false);

/// The thread that fires timers, started with the first one.
fn start_service() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    task::spawn("timers", || {
        loop {
            fire_due();
            let next = TIMERS.lock().iter().filter_map(|timer| timer.due).min();
            // Deadlines are in timer ticks (milliseconds); round up.
            let deadline = next.map(|due| due.div_ceil(1_000_000).max(time::ticks() + 1));
            CHANGED.wait_until_deadline(deadline, || {
                let earliest = TIMERS.lock().iter().filter_map(|timer| timer.due).min();
                (earliest != next).then_some(())
            });
        }
    });
}

/// Sends the signals of every timer that's due.
fn fire_due() {
    let now = time::uptime_nanos();
    let mut firing = Vec::new();
    for timer in TIMERS.lock().iter_mut() {
        let Some(due) = timer.due.filter(|&due| due <= now) else { continue };
        match (now - due).checked_div(timer.interval) {
            Some(missed) => {
                timer.overrun = missed.min(u64::from(i32::MAX as u32)) as u32;
                timer.due = Some(due + (missed + 1) * timer.interval);
            }
            // Not repeating.
            None => {
                timer.overrun = 0;
                timer.due = None;
            }
        }
        firing.push((timer.process, timer.id, timer.signal, timer.notify, timer.value, timer.overrun));
    }
    for (pid, id, signal, notify, value, overrun) in firing {
        let Some(process) = find(pid) else { continue };
        let info = SignalInfo { code: SI_TIMER, fields: [u64::from(id as u32) | u64::from(overrun) << 32, value] };
        match notify {
            Notify::Signal => signal::send_with_info(&process, signal, info),
            Notify::Thread(tid) => {
                let thread = process.threads.lock().iter().find(|thread| thread.tid() == tid).cloned();
                match thread {
                    Some(thread) => signal::send_to_thread_with_info(&thread, signal, info),
                    None => signal::send_with_info(&process, signal, info),
                }
            }
            Notify::None => {}
        }
    }
}

/// A new, disarmed timer. Returns its id.
pub fn create(process: Pid, signal: u32, notify: Notify, value: Option<u64>) -> Result<i32> {
    if signal as usize >= super::SIGNAL_COUNT || (signal == 0 && notify != Notify::None) {
        return Err(Errno::EINVAL);
    }
    start_service();
    let mut timers = TIMERS.lock();
    let id = (0..).find(|id| !timers.iter().any(|t| t.process == process && t.id == *id)).ok_or(Errno::EAGAIN)?;
    if timers.iter().filter(|timer| timer.process == process).count() >= 256 {
        return Err(Errno::EAGAIN);
    }
    // Without a value, the signal carries the timer's id.
    let value = value.unwrap_or(id as u64);
    timers.push(Timer { process, id, signal, notify, value, due: None, interval: 0, overrun: 0 });
    Ok(id)
}

/// Arms (or with `value` 0, disarms) a timer: first due in `value`
/// nanoseconds, then every `interval`. Returns what it was set to before:
/// (time left, interval).
pub fn set(process: Pid, id: i32, value: u64, interval: u64) -> Result<(u64, u64)> {
    if id == INTERVAL_TIMER {
        start_service();
    }
    let now = time::uptime_nanos();
    let old = {
        let mut timers = TIMERS.lock();
        if id == INTERVAL_TIMER && !timers.iter().any(|t| t.process == process && t.id == id) {
            let alarm = Timer {
                process,
                id,
                signal: signal::SIGALRM,
                notify: Notify::Signal,
                value: 0,
                due: None,
                interval: 0,
                overrun: 0,
            };
            timers.push(alarm);
        }
        let timer = timers.iter_mut().find(|t| t.process == process && t.id == id).ok_or(Errno::EINVAL)?;
        let old = (timer.due.map_or(0, |due| due.saturating_sub(now).max(1)), timer.interval);
        timer.due = (value > 0).then(|| now + value);
        timer.interval = if value > 0 { interval } else { 0 };
        timer.overrun = 0;
        old
    };
    CHANGED.wake_all();
    Ok(old)
}

/// (Time left, interval) of a timer, in nanoseconds.
pub fn get(process: Pid, id: i32) -> Result<(u64, u64)> {
    let now = time::uptime_nanos();
    let timers = TIMERS.lock();
    match timers.iter().find(|t| t.process == process && t.id == id) {
        Some(timer) => Ok((timer.due.map_or(0, |due| due.saturating_sub(now)), timer.interval)),
        None if id == INTERVAL_TIMER => Ok((0, 0)),
        None => Err(Errno::EINVAL),
    }
}

pub fn overrun(process: Pid, id: i32) -> Result<u32> {
    TIMERS.lock().iter().find(|t| t.process == process && t.id == id).map(|t| t.overrun).ok_or(Errno::EINVAL)
}

pub fn delete(process: Pid, id: i32) -> Result<()> {
    let mut timers = TIMERS.lock();
    let before = timers.len();
    timers.retain(|t| !(t.process == process && t.id == id));
    if timers.len() == before { Err(Errno::EINVAL) } else { Ok(()) }
}

/// A process ended: its timers go.
pub fn process_ended(process: Pid) {
    TIMERS.lock().retain(|timer| timer.process != process);
}

/// After `execve`: POSIX timers go; `setitimer`'s and `alarm`'s stay.
pub fn process_executed(process: Pid) {
    TIMERS.lock().retain(|timer| timer.process != process || timer.id == INTERVAL_TIMER);
}
