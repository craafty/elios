//! Waiting for files to become ready: `poll`, `ppoll`, `select`,
//! `pselect6` and `epoll`; and the event files waited for: `eventfd` and
//! `timerfd`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::epoll::{self, Epoll};
use crate::fs::events::{Clock, EventFd, TimerFd};
use crate::fs::{self, O_CLOEXEC, O_NONBLOCK, OpenFile, POLLERR, POLLHUP, POLLIN, POLLOUT};
use crate::process::{self, user};
use crate::time;

const POLLNVAL: u16 = 0x20;
const POLLPRI: u16 = 0x2;

/// One entry of `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: u16,
    returned: u16,
}

/// Waits until at least one entry is ready (filling in `returned`), the
/// deadline (in ticks) passes, or a signal arrives. Returns how many are
/// ready.
fn wait(entries: &mut [PollFd], deadline: Option<u64>) -> Result<u64> {
    let process = process::current();
    // Files that become ready at a set time (timerfds) need a wakeup then.
    let mut watch = fs::PollWatch::default();
    let files_deadline = {
        let files = process.files.lock();
        let watched: Vec<Arc<OpenFile>> = entries
            .iter()
            .filter(|entry| entry.fd >= 0)
            .filter_map(|entry| files.get(entry.fd as usize).ok())
            .collect();
        drop(files);
        watched.iter().for_each(|file| watch.add(&*file.inode));
        watched.iter().filter_map(|file| file.inode.poll_deadline()).min()
    };
    let effective = match (deadline, files_deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let ready = crate::task::WaitQueue::wait_any(&watch.queues(), effective, || {
        if process::signal_pending() {
            return Some(Err(Errno::EINTR));
        }
        let files = process.files.lock();
        let mut count = 0;
        for entry in entries.iter_mut() {
            entry.returned = if entry.fd < 0 {
                0
            } else {
                match files.get(entry.fd as usize) {
                    Ok(file) => file.inode.poll(file.flags()) & (entry.events | POLLERR | POLLHUP),
                    Err(_) => POLLNVAL,
                }
            };
            if entry.returned != 0 {
                count += 1;
            }
        }
        (count > 0).then_some(Ok(count))
    });
    match ready {
        Some(result) => result,
        // A timerfd came due: look again.
        None if deadline.is_none_or(|deadline| time::ticks() < deadline) => wait(entries, deadline),
        None => {
            entries.iter_mut().for_each(|entry| entry.returned = 0);
            Ok(0)
        }
    }
}

fn deadline_after_ms(ms: u64) -> u64 {
    time::ticks() + ms
}

fn read_entries(address: u64, count: u64) -> Result<Vec<PollFd>> {
    if count > fs::MAX_FILES as u64 {
        return Err(Errno::EINVAL);
    }
    (0..count).map(|index| user::read_value(address + index * 8)).collect()
}

fn write_entries(address: u64, entries: &[PollFd]) -> Result<()> {
    for (index, entry) in entries.iter().enumerate() {
        user::write_value(address + index as u64 * 8, entry)?;
    }
    Ok(())
}

pub fn poll(address: u64, count: u64, timeout_ms: u64) -> Result<u64> {
    let mut entries = read_entries(address, count)?;
    let timeout = timeout_ms as i32;
    let deadline = (timeout >= 0).then(|| deadline_after_ms(timeout as u64));
    let ready = wait(&mut entries, deadline)?;
    write_entries(address, &entries)?;
    Ok(ready)
}

/// A `struct timespec` timeout as a deadline; `None` means wait forever.
fn timespec_deadline(address: u64) -> Result<Option<u64>> {
    if address == 0 {
        return Ok(None);
    }
    let [seconds, nanoseconds]: [u64; 2] = user::read_value(address)?;
    Ok(Some(deadline_after_ms(seconds * 1000 + nanoseconds.div_ceil(1_000_000))))
}

pub fn ppoll(address: u64, count: u64, timeout: u64) -> Result<u64> {
    let mut entries = read_entries(address, count)?;
    let ready = wait(&mut entries, timespec_deadline(timeout)?)?;
    write_entries(address, &entries)?;
    Ok(ready)
}

/// `select` and `pselect6`: the same thing with bitmaps of descriptors.
/// `timeout` is a timeval (`select`) or timespec (`pselect6`).
pub fn select(count: u64, read: u64, write: u64, except: u64, timeout: u64, timespec: bool) -> Result<u64> {
    let count = (count as usize).min(fs::MAX_FILES);
    let words = count.div_ceil(64);
    let load = |address: u64| -> Result<Vec<u64>> {
        if address == 0 {
            return Ok(alloc::vec![0; words]);
        }
        (0..words).map(|word| user::read_value(address + word as u64 * 8)).collect()
    };
    let (read_set, write_set, except_set) = (load(read)?, load(write)?, load(except)?);
    let is_set = |set: &[u64], fd: usize| set[fd / 64] & (1 << (fd % 64)) != 0;

    let mut entries = Vec::new();
    for fd in 0..count {
        let mut events = 0;
        if is_set(&read_set, fd) {
            events |= POLLIN;
        }
        if is_set(&write_set, fd) {
            events |= POLLOUT;
        }
        if is_set(&except_set, fd) {
            events |= POLLPRI;
        }
        if events != 0 {
            entries.push(PollFd { fd: fd as i32, events, returned: 0 });
        }
    }

    let deadline = if timeout == 0 {
        None
    } else if timespec {
        timespec_deadline(timeout)?
    } else {
        let [seconds, microseconds]: [u64; 2] = user::read_value(timeout)?;
        Some(deadline_after_ms(seconds * 1000 + microseconds.div_ceil(1000)))
    };
    wait(&mut entries, deadline)?;
    if entries.iter().any(|entry| entry.returned & POLLNVAL != 0) {
        return Err(Errno::EBADF);
    }

    let mut results = [alloc::vec![0u64; words], alloc::vec![0u64; words], alloc::vec![0u64; words]];
    let mut total = 0;
    for entry in &entries {
        let fd = entry.fd as usize;
        let hits = [
            entry.events & POLLIN != 0 && entry.returned & (POLLIN | POLLHUP | POLLERR) != 0,
            entry.events & POLLOUT != 0 && entry.returned & (POLLOUT | POLLERR) != 0,
            entry.events & POLLPRI != 0 && entry.returned & POLLPRI != 0,
        ];
        for (set, hit) in results.iter_mut().zip(hits) {
            if hit {
                set[fd / 64] |= 1 << (fd % 64);
                total += 1;
            }
        }
    }
    for (address, set) in [read, write, except].into_iter().zip(&results) {
        if address != 0 {
            for (word, value) in set.iter().enumerate() {
                user::write_value(address + word as u64 * 8, value)?;
            }
        }
    }
    Ok(total)
}

fn insert_file(inode: Arc<dyn fs::Inode>, name: &str, flags: u32) -> Result<u64> {
    let file = OpenFile::new(inode, alloc::string::String::from(name), fs::O_RDWR | (flags & O_NONBLOCK));
    let fd = process::current().files.lock().insert(file, flags & O_CLOEXEC != 0, 0)?;
    Ok(fd as u64)
}

/// `epoll_create1` (and `epoll_create`, whose size is ignored).
pub fn epoll_create(flags: u64) -> Result<u64> {
    let flags = flags as u32;
    if flags & !O_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    insert_file(Epoll::new(), "anon_inode:[eventpoll]", flags)
}

fn epoll_of(file: &Arc<OpenFile>) -> Result<&Epoll> {
    file.inode.as_any().and_then(|any| any.downcast_ref::<Epoll>()).ok_or(Errno::EINVAL)
}

pub fn epoll_ctl(epfd: u64, op: u64, fd: u64, event: u64) -> Result<u64> {
    let process = process::current();
    let (epoll_file, file) = {
        let files = process.files.lock();
        (files.get(epfd as i32 as usize)?, files.get(fd as i32 as usize)?)
    };
    let epoll = epoll_of(&epoll_file)?;
    let (events, data) = if op as u32 == epoll::CTL_DEL {
        (0, 0)
    } else {
        // struct epoll_event is packed: a u32 of events, then 8 bytes of data.
        let events: u32 = user::read_value(event)?;
        let data: u64 = user::read_value(event + 4)?;
        (events & epoll::VALID_EVENTS, data)
    };
    epoll.control(op as u32, fd as i32, &file, events, data).map(|()| 0)
}

/// `epoll_wait`, `epoll_pwait` (the signal mask isn't applied) and
/// `epoll_pwait2` (`timeout` is a timespec address, if `timespec`).
pub fn epoll_wait(epfd: u64, events: u64, max: u64, timeout: u64, timespec: bool) -> Result<u64> {
    let max = max as i32;
    if max <= 0 {
        return Err(Errno::EINVAL);
    }
    let file = process::current().files.lock().get(epfd as i32 as usize)?;
    let epoll = epoll_of(&file)?;
    let deadline = if timespec {
        timespec_deadline(timeout)?
    } else {
        let ms = timeout as i32;
        (ms >= 0).then(|| deadline_after_ms(ms as u64))
    };
    let ready = loop {
        let effective = match (deadline, epoll.deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        // Wait on the watched files' sources. If the set changes meanwhile
        // (another thread's epoll_ctl), start over with the new set.
        let mut watch = fs::PollWatch::default();
        watch.add(epoll);
        let membership = epoll.membership_changes();
        let result = crate::task::WaitQueue::wait_any(&watch.queues(), effective, || {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let ready = epoll.collect(max as usize);
            if !ready.is_empty() {
                return Some(Ok(Some(ready)));
            }
            (epoll.membership_changes() != membership).then_some(Ok(None))
        });
        match result {
            Some(Ok(Some(ready))) => break ready,
            Some(Ok(None)) => continue,
            Some(Err(error)) => return Err(error),
            None if deadline.is_none_or(|deadline| time::ticks() < deadline) => continue,
            None => break Vec::new(),
        }
    };
    for (index, &(state, data)) in ready.iter().enumerate() {
        let address = events + index as u64 * 12;
        user::write_value(address, &state)?;
        user::write_value(address + 4, &data)?;
    }
    Ok(ready.len() as u64)
}

const EFD_SEMAPHORE: u32 = 1;

/// `eventfd2` (and `eventfd`, with no flags).
pub fn eventfd(initial: u64, flags: u64) -> Result<u64> {
    let flags = flags as u32;
    if flags & !(EFD_SEMAPHORE | O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    insert_file(EventFd::new(initial as u32 as u64, flags & EFD_SEMAPHORE != 0), "anon_inode:[eventfd]", flags)
}

pub fn timerfd_create(clock: u64, flags: u64) -> Result<u64> {
    let flags = flags as u32;
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    // CLOCK_REALTIME and its alarm variant; CLOCK_MONOTONIC and the
    // boot-time ones (the same thing here: eliOS doesn't suspend).
    let clock = match clock {
        0 | 8 => Clock::Realtime,
        1 | 7 | 9 => Clock::Monotonic,
        _ => return Err(Errno::EINVAL),
    };
    insert_file(TimerFd::new(clock), "anon_inode:[timerfd]", flags)
}

fn timer_of(fd: u64) -> Result<Arc<OpenFile>> {
    let file = process::current().files.lock().get(fd as i32 as usize)?;
    if file.inode.as_any().is_some_and(|any| any.is::<TimerFd>()) { Ok(file) } else { Err(Errno::EINVAL) }
}

/// Writes a `struct itimerspec` (interval, then value) in nanoseconds.
fn write_itimerspec(address: u64, (value, interval): (u64, u64)) -> Result<()> {
    let split = |nanos: u64| [nanos / 1_000_000_000, nanos % 1_000_000_000];
    let [a, b] = split(interval);
    let [c, d] = split(value);
    user::write_value(address, &[a, b, c, d])
}

const TFD_TIMER_ABSTIME: u64 = 1;

pub fn timerfd_settime(fd: u64, flags: u64, new: u64, old: u64) -> Result<u64> {
    let file = timer_of(fd)?;
    let timer = file.inode.as_any().and_then(|any| any.downcast_ref::<TimerFd>()).ok_or(Errno::EINVAL)?;
    let spec: [i64; 4] = user::read_value(new)?;
    if spec.iter().any(|&part| part < 0) || spec[1] >= 1_000_000_000 || spec[3] >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let nanos = |seconds: i64, nanoseconds: i64| seconds as u64 * 1_000_000_000 + nanoseconds as u64;
    let previous = timer.set(nanos(spec[2], spec[3]), nanos(spec[0], spec[1]), flags & TFD_TIMER_ABSTIME != 0);
    if old != 0 {
        write_itimerspec(old, previous)?;
    }
    Ok(0)
}

pub fn timerfd_gettime(fd: u64, current: u64) -> Result<u64> {
    let file = timer_of(fd)?;
    let timer = file.inode.as_any().and_then(|any| any.downcast_ref::<TimerFd>()).ok_or(Errno::EINVAL)?;
    write_itimerspec(current, timer.get())?;
    Ok(0)
}
