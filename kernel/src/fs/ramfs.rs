//! The in-memory file system that holds the root directory tree.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{Attributes, FileKind, Inode, Metadata};
use crate::errno::{Errno, Result};
use crate::memory::frames;
use crate::memory::object::MemoryObject;
use crate::process::credentials::{Gid, Uid};
use crate::sync::SpinLock;
use crate::time;

static NEXT_INODE: AtomicU64 = AtomicU64::new(1);

pub fn next_inode_number() -> u64 {
    NEXT_INODE.fetch_add(1, Ordering::Relaxed)
}

/// The owner, group and permissions of a file or directory, changeable
/// without a lock.
struct Ownership {
    permissions: AtomicU32,
    uid: AtomicU32,
    gid: AtomicU32,
    modified: AtomicU64,
}

impl Ownership {
    fn new(attributes: Attributes) -> Self {
        Self {
            permissions: AtomicU32::new(attributes.permissions & 0o7777),
            uid: AtomicU32::new(attributes.uid),
            gid: AtomicU32::new(attributes.gid),
            modified: AtomicU64::new(time::unix_time()),
        }
    }

    fn metadata(&self, kind: FileKind, inode: u64, size: u64, links: u32) -> Metadata {
        Metadata {
            kind,
            inode,
            size,
            permissions: self.permissions.load(Ordering::Relaxed),
            uid: self.uid.load(Ordering::Relaxed),
            gid: self.gid.load(Ordering::Relaxed),
            links,
            modified: self.modified.load(Ordering::Relaxed),
            rdev: 0,
        }
    }

    fn set_permissions(&self, permissions: u32) {
        self.permissions.store(permissions & 0o7777, Ordering::Relaxed);
    }

    fn set_owner(&self, uid: Uid, gid: Gid) {
        self.uid.store(uid, Ordering::Relaxed);
        self.gid.store(gid, Ordering::Relaxed);
    }

    fn touch(&self) {
        self.modified.store(time::unix_time(), Ordering::Relaxed);
    }
}

/// A regular file or symbolic link (whose contents are its target).
pub struct File {
    inode: u64,
    kind: FileKind,
    ownership: Ownership,
    contents: Contents,
}

enum Contents {
    /// A regular file's data is a memory object, so programs that map the
    /// file share its pages (see `memory::object`).
    Data(Arc<MemoryObject>),
    Link(SpinLock<Vec<u8>>),
}

impl File {
    pub fn new(kind: FileKind, attributes: Attributes) -> Arc<Self> {
        let contents = match kind {
            FileKind::Symlink => Contents::Link(SpinLock::new(Vec::new())),
            _ => Contents::Data(MemoryObject::new()),
        };
        Arc::new(Self { inode: next_inode_number(), kind, ownership: Ownership::new(attributes), contents })
    }

    fn size(&self) -> u64 {
        match &self.contents {
            Contents::Data(object) => object.size(),
            Contents::Link(target) => target.lock().len() as u64,
        }
    }
}

/// Writes that would leave less free memory than this fail with `ENOSPC`,
/// so one runaway write can't take all of memory.
const MEMORY_RESERVE: u64 = 32 << 20;

fn check_space(growth: u64) -> Result<()> {
    if frames::stats().free_bytes < growth.saturating_add(MEMORY_RESERVE) { Err(Errno::ENOSPC) } else { Ok(()) }
}

impl Inode for File {
    fn metadata(&self) -> Metadata {
        self.ownership.metadata(self.kind, self.inode, self.size(), 1)
    }

    fn memory_object(&self) -> Option<Arc<MemoryObject>> {
        match &self.contents {
            Contents::Data(object) => Some(object.clone()),
            Contents::Link(_) => None,
        }
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.ownership.set_permissions(permissions);
        Ok(())
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.ownership.set_owner(uid, gid);
        Ok(())
    }

    fn set_modified(&self, time: u64) -> Result<()> {
        self.ownership.modified.store(time, Ordering::Relaxed);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match &self.contents {
            Contents::Data(object) => object.read(offset, buf),
            Contents::Link(target) => {
                let target = target.lock();
                let start = (offset as usize).min(target.len());
                let count = buf.len().min(target.len() - start);
                buf[..count].copy_from_slice(&target[start..start + count]);
                Ok(count)
            }
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let end = offset.checked_add(buf.len() as u64).ok_or(Errno::EFBIG)?;
        check_space(end.saturating_sub(self.size()))?;
        let written = match &self.contents {
            Contents::Data(object) => object.write(offset, buf)?,
            Contents::Link(target) => {
                let mut target = target.lock();
                if target.len() < end as usize {
                    target.resize(end as usize, 0);
                }
                target[offset as usize..end as usize].copy_from_slice(buf);
                buf.len()
            }
        };
        self.ownership.touch();
        Ok(written)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        match &self.contents {
            Contents::Data(object) => object.truncate(len),
            Contents::Link(target) => target.lock().resize(len as usize, 0),
        }
        self.ownership.touch();
        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        match &self.contents {
            Contents::Link(target) => String::from_utf8(target.lock().clone()).map_err(|_| Errno::EINVAL),
            Contents::Data(_) => Err(Errno::EINVAL),
        }
    }
}

pub struct Directory {
    inode: u64,
    ownership: Ownership,
    entries: SpinLock<BTreeMap<String, Arc<dyn Inode>>>,
}

impl Directory {
    pub fn new(attributes: Attributes) -> Arc<Self> {
        Arc::new(Self {
            inode: next_inode_number(),
            ownership: Ownership::new(attributes),
            entries: SpinLock::new(BTreeMap::new()),
        })
    }

    fn touch(&self) {
        self.ownership.touch();
    }
}

fn valid_name(name: &str) -> Result<()> {
    match name {
        "" | "." | ".." => Err(Errno::EINVAL),
        name if name.contains('/') => Err(Errno::EINVAL),
        name if name.len() > 255 => Err(Errno::ENAMETOOLONG),
        _ => Ok(()),
    }
}

impl Inode for Directory {
    fn metadata(&self) -> Metadata {
        self.ownership.metadata(FileKind::Directory, self.inode, self.entries.lock().len() as u64, 2)
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.ownership.set_permissions(permissions);
        Ok(())
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.ownership.set_owner(uid, gid);
        Ok(())
    }

    fn set_modified(&self, time: u64) -> Result<()> {
        self.ownership.modified.store(time, Ordering::Relaxed);
        Ok(())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        self.entries.lock().get(name).cloned().ok_or(Errno::ENOENT)
    }

    fn create(&self, name: &str, kind: FileKind, attributes: Attributes) -> Result<Arc<dyn Inode>> {
        valid_name(name)?;
        let inode: Arc<dyn Inode> = match kind {
            FileKind::Directory => Directory::new(attributes),
            FileKind::Regular => File::new(FileKind::Regular, attributes),
            FileKind::Symlink => File::new(FileKind::Symlink, Attributes { permissions: 0o777, ..attributes }),
            _ => return Err(Errno::EINVAL),
        };
        self.link(name, inode.clone())?;
        Ok(inode)
    }

    fn link(&self, name: &str, inode: Arc<dyn Inode>) -> Result<()> {
        valid_name(name)?;
        let mut entries = self.entries.lock();
        if entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        entries.insert(name.to_string(), inode);
        drop(entries);
        self.touch();
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<()> {
        self.entries.lock().remove(name).ok_or(Errno::ENOENT)?;
        self.touch();
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, Arc<dyn Inode>)>> {
        Ok(self.entries.lock().iter().map(|(name, inode)| (name.clone(), inode.clone())).collect())
    }
}
