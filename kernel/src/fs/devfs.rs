//! Device files under /dev.

use alloc::sync::Arc;

use super::{FileKind, Inode, Metadata, ramfs};
use crate::errno::Result;
use crate::{random, tty};

/// A character device that behaves like a bottomless source and sink.
struct Device {
    inode: u64,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    /// Reads nothing, swallows writes.
    Null,
    /// Reads zeros, swallows writes.
    Zero,
    /// Reads random bytes.
    Random,
}

impl Inode for Device {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o666)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        match self.kind {
            Kind::Null => Ok(0),
            Kind::Zero => {
                buf.fill(0);
                Ok(buf.len())
            }
            Kind::Random => {
                random::fill(buf);
                Ok(buf.len())
            }
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

/// /dev/tty: the terminal itself, but open to everyone (it's the caller's
/// own terminal), unlike /dev/console, which belongs to whoever logged in.
struct ControllingTerminal {
    inode: u64,
    terminal: Arc<dyn Inode>,
}

impl Inode for ControllingTerminal {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o666)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.terminal.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        self.terminal.write_at(offset, buf)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.terminal.read_stream(buf, nonblocking)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
        self.terminal.write_stream(buf, nonblocking)
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        self.terminal.ioctl(request, argument)
    }

    fn poll(&self, flags: u32) -> u16 {
        self.terminal.poll(flags)
    }

    fn on_close(&self, flags: u32) {
        self.terminal.on_close(flags);
    }
}

pub fn populate(dev: &Arc<dyn Inode>) {
    let devices = [("null", Kind::Null), ("zero", Kind::Zero), ("random", Kind::Random), ("urandom", Kind::Random)];
    for (name, kind) in devices {
        let _ = dev.link(name, Arc::new(Device { inode: ramfs::next_inode_number(), kind }));
    }
    let console: Arc<dyn Inode> = tty::console();
    let _ = dev.link("console", console.clone());
    let _ = dev.link("tty", Arc::new(ControllingTerminal { inode: ramfs::next_inode_number(), terminal: console }));
}
