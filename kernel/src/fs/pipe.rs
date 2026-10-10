//! Pipes: a byte buffer with a reading end and a writing end.
//!
//! Readers block while the pipe is empty and writers while it's full.
//! Reading after every writer has closed returns end-of-file; writing after
//! every reader has closed fails with EPIPE.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::{FileKind, Inode, Metadata, O_ACCMODE, O_RDONLY, OpenFile, POLLERR, POLLHUP, POLLIN, POLLOUT, ramfs};
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
        self.read_stream(buf, false)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        self.write_stream(buf, false)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
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
                if state.writers == 0 {
                    return Some(Ok(0));
                }
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            let count = buf.len().min(state.buffer.len());
            for (slot, byte) in buf.iter_mut().zip(state.buffer.drain(..count)) {
                *slot = byte;
            }
            Some(Ok(count))
        })?;
        self.changed.wake_all();
        super::notify_poll();
        Ok(count)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
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
                    return nonblocking.then_some(Err(Errno::EAGAIN));
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
            super::notify_poll();
        }
        Ok(written)
    }

    fn poll(&self, flags: u32) -> u16 {
        let state = self.state.lock();
        if flags & O_ACCMODE == O_RDONLY {
            let data = if state.buffer.is_empty() { 0 } else { POLLIN };
            let hangup = if state.writers == 0 { POLLHUP } else { 0 };
            data | hangup
        } else if state.readers == 0 {
            POLLERR
        } else if state.buffer.len() < CAPACITY {
            POLLOUT
        } else {
            0
        }
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
        super::notify_poll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn pipe_readiness_follows_its_contents() {
            let (reader, writer) = create();
            assert_eq!(reader.inode.poll(reader.flags()), 0);
            assert_eq!(writer.inode.poll(writer.flags()), POLLOUT);
            writer.write(b"x").unwrap();
            assert_eq!(reader.inode.poll(reader.flags()), POLLIN);
            drop(writer);
            assert_eq!(reader.inode.poll(reader.flags()), POLLIN | POLLHUP);
        }
    }
}
