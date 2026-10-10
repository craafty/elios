//! Waiting for files to become ready: `poll`, `ppoll`, `select` and
//! `pselect6`.

use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{self, POLLERR, POLLHUP, POLLIN, POLLOUT};
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
    let ready = fs::POLL_EVENTS.wait_until_deadline(deadline, || {
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
