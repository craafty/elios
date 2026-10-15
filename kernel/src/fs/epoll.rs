//! `epoll`: a set of files a program watches, waited for all at once. It's
//! what Linux event loops use instead of `poll` (libwayland's, Firefox's
//! IPC, libuv's).
//!
//! It's built on the same mechanism as `poll`: one wait queue that's woken
//! whenever any file may have become ready, after which each watched file
//! is asked its state. An entry refers to the open file weakly, so closing
//! the last descriptor for a file drops it from every epoll set, as on
//! Linux.
//!
//! **Edge-triggered entries** (`EPOLLET`) should be reported once each
//! time a file *becomes* ready, which needs every file to count its state
//! changes. Instead, an edge-triggered entry is reported at most once per
//! "generation" of the poll wait queue (every wakeup starts a new one).
//! A program that reads until `EAGAIN`, as the edge-triggered contract
//! requires, sees every new event, sometimes with a spurious extra report,
//! and an always-writable socket doesn't make `epoll_wait` spin.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use super::{FileKind, Inode, Metadata, OpenFile, POLLERR, POLLHUP, POLLIN, ramfs};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;

pub const EPOLLERR: u32 = 0x8;
pub const EPOLLHUP: u32 = 0x10;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLLWAKEUP: u32 = 1 << 29;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
/// The readiness bits `poll` can report (`EPOLLIN` and friends have the
/// same values as `POLLIN` and friends).
const READINESS: u32 = 0x3F | EPOLLRDHUP;

pub const CTL_ADD: u32 = 1;
pub const CTL_DEL: u32 = 2;
pub const CTL_MOD: u32 = 3;

struct Entry {
    file: Weak<OpenFile>,
    events: u32,
    data: u64,
    /// The poll generation this (edge-triggered) entry was last reported in.
    reported: Option<u64>,
}

pub struct Epoll {
    inode: u64,
    /// Watched files, by descriptor number.
    entries: SpinLock<BTreeMap<i32, Entry>>,
}

impl Epoll {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number(), entries: SpinLock::new(BTreeMap::new()) })
    }

    pub fn control(&self, op: u32, fd: i32, file: &Arc<OpenFile>, events: u32, data: u64) -> Result<()> {
        // An epoll set can't watch itself.
        if core::ptr::eq(Arc::as_ptr(&file.inode).cast::<u8>(), (self as *const Self).cast::<u8>()) {
            return Err(Errno::EINVAL);
        }
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.file.strong_count() > 0);
        match op {
            CTL_ADD => {
                if entries.contains_key(&fd) {
                    return Err(Errno::EEXIST);
                }
                entries.insert(fd, Entry { file: Arc::downgrade(file), events, data, reported: None });
            }
            CTL_MOD => {
                let entry = entries.get_mut(&fd).ok_or(Errno::ENOENT)?;
                if !Weak::ptr_eq(&entry.file, &Arc::downgrade(file)) {
                    return Err(Errno::ENOENT);
                }
                if events & EPOLLEXCLUSIVE != 0 {
                    return Err(Errno::EINVAL);
                }
                *entry = Entry { file: Arc::downgrade(file), events, data, reported: None };
            }
            CTL_DEL => {
                entries.remove(&fd).ok_or(Errno::ENOENT)?;
            }
            _ => return Err(Errno::EINVAL),
        }
        drop(entries);
        super::notify_poll();
        Ok(())
    }

    /// The entries ready now, up to `max`, as (events, data); marks
    /// edge-triggered and one-shot ones as reported.
    pub fn collect(&self, max: usize, generation: u64) -> Vec<(u32, u64)> {
        let mut ready = Vec::new();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.file.strong_count() > 0);
        for entry in entries.values_mut() {
            if ready.len() == max {
                break;
            }
            let Some(file) = entry.file.upgrade() else { continue };
            let wanted = entry.events & READINESS | EPOLLERR | EPOLLHUP;
            if entry.events & READINESS == 0 && entry.events & EPOLLONESHOT != 0 {
                // A one-shot entry that already fired, until re-armed.
                continue;
            }
            let state = u32::from(file.inode.poll(file.flags())) & wanted;
            if state == 0 {
                entry.reported = None;
                continue;
            }
            if entry.events & EPOLLET != 0 {
                if entry.reported == Some(generation) {
                    continue;
                }
                entry.reported = Some(generation);
            }
            if entry.events & EPOLLONESHOT != 0 {
                entry.events &= !READINESS;
            }
            ready.push((state, entry.data));
        }
        ready
    }

    /// The earliest timer tick a watched file becomes ready by itself (a
    /// timerfd), for the wait's deadline.
    pub fn deadline(&self) -> Option<u64> {
        let files: Vec<Arc<OpenFile>> = self.entries.lock().values().filter_map(|entry| entry.file.upgrade()).collect();
        files.iter().filter_map(|file| file.inode.poll_deadline()).min()
    }
}

/// Flags `epoll_ctl` accepts.
pub const VALID_EVENTS: u32 = READINESS | EPOLLEXCLUSIVE | EPOLLWAKEUP | EPOLLONESHOT | EPOLLET;

impl Inode for Epoll {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Regular, self.inode, 0o600)
    }

    fn seekable(&self) -> bool {
        false
    }

    /// An epoll file is readable when something in it is ready (programs
    /// nest them, or `poll` one).
    fn poll(&self, _flags: u32) -> u16 {
        let files: Vec<(Arc<OpenFile>, u32)> =
            self.entries.lock().values().filter_map(|entry| Some((entry.file.upgrade()?, entry.events))).collect();
        let ready = files.iter().any(|(file, events)| {
            let wanted = (events & READINESS) as u16 | POLLERR | POLLHUP;
            file.inode.poll(file.flags()) & wanted != 0
        });
        if ready { POLLIN } else { 0 }
    }

    fn poll_deadline(&self) -> Option<u64> {
        self.deadline()
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}
