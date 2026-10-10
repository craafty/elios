//! Pipes: a byte buffer with a reading end and a writing end.
//!
//! Readers block while the pipe is empty and writers while it's full.
//! Reading after every writer has closed returns end-of-file; writing after
//! every reader has closed fails with EPIPE.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::{FileKind, Inode, Metadata, O_ACCMODE, O_RDONLY, OpenFile, ramfs};
use crate::errno::{Errno, Result};
use crate::process;
use crate::sync::SpinLock;
use crate::task::WaitQueue;

const CAPACITY: usize = 64 * 1024;

struct State {
    buffer: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

pub struct Pipe {
    inode: u64,
    state: SpinLock<State>,
    /// Woken whenever data, space, or a closed end appears.
    changed: WaitQueue,
}

/// Creates a pipe, returning its read end and write end.
pub fn create() -> (Arc<OpenFile>, Arc<OpenFile>) {
    let pipe = Arc::new(Pipe {
        inode: ramfs::next_inode_number(),
        state: SpinLock::new(State { buffer: VecDeque::new(), readers: 1, writers: 1 }),
        changed: WaitQueue::new(),
    });
    let reader = OpenFile::new(pipe.clone(), "pipe:".into(), O_RDONLY);
    let writer = OpenFile::new(pipe, "pipe:".into(), super::O_WRONLY);
    (reader, writer)
}

impl Inode for Pipe {
    fn metadata(&self) -> Metadata {
        let size = self.state.lock().buffer.len() as u64;
        Metadata { kind: FileKind::Fifo, inode: self.inode, size, permissions: 0o600, modified: 0 }
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let count = self.changed.wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut state = self.state.lock();
            if state.buffer.is_empty() {
                // No writers left means end of file.
                return (state.writers == 0).then_some(Ok(0));
            }
            let count = buf.len().min(state.buffer.len());
            for (slot, byte) in buf.iter_mut().zip(state.buffer.drain(..count)) {
                *slot = byte;
            }
            Some(Ok(count))
        })?;
        self.changed.wake_all();
        Ok(count)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        let mut written = 0;
        while written < buf.len() {
            let count = self.changed.wait_until(|| {
                if process::signal_pending() {
                    return Some(Err(Errno::EINTR));
                }
                let mut state = self.state.lock();
                if state.readers == 0 {
                    return Some(Err(Errno::EPIPE));
                }
                let space = CAPACITY - state.buffer.len();
                if space == 0 {
                    return None;
                }
                let count = space.min(buf.len() - written);
                state.buffer.extend(&buf[written..written + count]);
                Some(Ok(count))
            });
            match count {
                Ok(count) => written += count,
                // Report what made it through before the error, if anything.
                Err(_) if written > 0 => break,
                Err(error) => {
                    if error == Errno::EPIPE {
                        process::send_signal_to_self(process::SIGPIPE);
                    }
                    return Err(error);
                }
            }
            self.changed.wake_all();
        }
        Ok(written)
    }

    /// Each end is one open file, shared (not copied) by `dup` and `fork`,
    /// so this runs once per end.
    fn on_close(&self, flags: u32) {
        let mut state = self.state.lock();
        if flags & O_ACCMODE == O_RDONLY {
            state.readers -= 1;
        } else {
            state.writers -= 1;
        }
        drop(state);
        self.changed.wake_all();
    }
}
