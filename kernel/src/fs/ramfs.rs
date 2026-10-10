//! The in-memory file system that holds the root directory tree.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{Attributes, FileKind, Inode, Metadata};
use crate::errno::{Errno, Result};
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
    data: SpinLock<Vec<u8>>,
}

impl File {
    pub fn new(kind: FileKind, attributes: Attributes) -> Arc<Self> {
        Arc::new(Self {
            inode: next_inode_number(),
            kind,
            ownership: Ownership::new(attributes),
            data: SpinLock::new(Vec::new()),
        })
    }
}

/// Files bigger than this are refused, so one runaway write can't take all
/// of memory.
const MAX_FILE_SIZE: u64 = 64 << 20;

impl Inode for File {
    fn metadata(&self) -> Metadata {
        self.ownership.metadata(self.kind, self.inode, self.data.lock().len() as u64, 1)
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
        let data = self.data.lock();
        let start = (offset as usize).min(data.len());
        let count = buf.len().min(data.len() - start);
        buf[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let end = offset.checked_add(buf.len() as u64).ok_or(Errno::EFBIG)?;
        if end > MAX_FILE_SIZE {
            return Err(Errno::EFBIG);
        }
        let mut data = self.data.lock();
        if data.len() < end as usize {
            data.resize(end as usize, 0);
        }
        data[offset as usize..end as usize].copy_from_slice(buf);
        drop(data);
        self.ownership.touch();
        Ok(buf.len())
    }

    fn truncate(&self, len: u64) -> Result<()> {
        if len > MAX_FILE_SIZE {
            return Err(Errno::EFBIG);
        }
        self.data.lock().resize(len as usize, 0);
        self.ownership.touch();
        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        if self.kind != FileKind::Symlink {
            return Err(Errno::EINVAL);
        }
        String::from_utf8(self.data.lock().clone()).map_err(|_| Errno::EINVAL)
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
