//! `memfd_create`: an anonymous in-memory file, in no directory. Programs
//! use them to share memory: a Wayland client draws into one and passes
//! its descriptor to the compositor, which maps the same pages.
//!
//! A memfd can be *sealed* (`fcntl(F_ADD_SEALS)`) so whoever it's given to
//! can trust it: a compositor wants to know the file can't shrink under it
//! while it reads, which would make its accesses fault.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use super::{Attributes, FileKind, Inode, Metadata, ramfs};
use crate::errno::{Errno, Result};
use crate::memory::object::MemoryObject;
use crate::process::credentials::{Gid, Uid};

// Seals (Linux values).
pub const SEAL_SEAL: u32 = 0x1;
const SEAL_SHRINK: u32 = 0x2;
const SEAL_GROW: u32 = 0x4;
const SEAL_WRITE: u32 = 0x8;
const SEAL_FUTURE_WRITE: u32 = 0x10;
const ALL_SEALS: u32 = SEAL_SEAL | SEAL_SHRINK | SEAL_GROW | SEAL_WRITE | SEAL_FUTURE_WRITE;

pub struct Memfd {
    file: Arc<ramfs::File>,
    seals: AtomicU32,
}

impl Memfd {
    /// A new, empty memfd. Without `allow_sealing`, it starts sealed
    /// against sealing, as on Linux.
    pub fn new(attributes: Attributes, allow_sealing: bool) -> Arc<Self> {
        let seals = if allow_sealing { 0 } else { SEAL_SEAL };
        Arc::new(Self { file: ramfs::File::new(FileKind::Regular, attributes), seals: AtomicU32::new(seals) })
    }

    fn check_size_change(&self, new_size: u64) -> Result<()> {
        let size = self.file.metadata().size;
        let seals = self.seals.load(Ordering::Relaxed);
        if (new_size < size && seals & SEAL_SHRINK != 0) || (new_size > size && seals & SEAL_GROW != 0) {
            return Err(Errno::EPERM);
        }
        Ok(())
    }
}

impl Inode for Memfd {
    fn metadata(&self) -> Metadata {
        self.file.metadata()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.file.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if self.seals.load(Ordering::Relaxed) & (SEAL_WRITE | SEAL_FUTURE_WRITE) != 0 {
            return Err(Errno::EPERM);
        }
        let end = offset.checked_add(buf.len() as u64).ok_or(Errno::EFBIG)?;
        if end > self.file.metadata().size {
            self.check_size_change(end)?;
        }
        self.file.write_at(offset, buf)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        self.check_size_change(len)?;
        self.file.truncate(len)
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.file.set_permissions(permissions)
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.file.set_owner(uid, gid)
    }

    fn memory_object(&self) -> Option<Arc<MemoryObject>> {
        self.file.memory_object()
    }

    fn add_seals(&self, seals: u32) -> Result<()> {
        if seals & !ALL_SEALS != 0 {
            return Err(Errno::EINVAL);
        }
        self.seals
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current & SEAL_SEAL == 0).then_some(current | seals)
            })
            .map(|_| ())
            .map_err(|_| Errno::EPERM)
    }

    fn seals(&self) -> Result<u32> {
        Ok(self.seals.load(Ordering::Relaxed))
    }
}
