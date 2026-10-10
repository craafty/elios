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
        Metadata { kind: FileKind::CharDevice, inode: self.inode, size: 0, permissions: 0o666, modified: 0 }
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

pub fn populate(dev: &Arc<dyn Inode>) {
    let devices = [("null", Kind::Null), ("zero", Kind::Zero), ("random", Kind::Random), ("urandom", Kind::Random)];
    for (name, kind) in devices {
        let _ = dev.link(name, Arc::new(Device { inode: ramfs::next_inode_number(), kind }));
    }
    let console: Arc<dyn Inode> = tty::console();
    let _ = dev.link("console", console.clone());
    let _ = dev.link("tty", console);
}
