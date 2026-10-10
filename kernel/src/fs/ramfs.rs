//! The in-memory file system that holds the root directory tree.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{FileKind, Inode, Metadata};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;
use crate::time;

static NEXT_INODE: AtomicU64 = AtomicU64::new(1);

pub fn next_inode_number() -> u64 {
    NEXT_INODE.fetch_add(1, Ordering::Relaxed)
}

/// A regular file or symbolic link (whose contents are its target).
pub struct File {
    inode: u64,
    kind: FileKind,
    permissions: u32,
    data: SpinLock<Vec<u8>>,
    modified: AtomicU64,
}

impl File {
    pub fn new(kind: FileKind, permissions: u32) -> Arc<Self> {
        Arc::new(Self {
            inode: next_inode_number(),
            kind,
            permissions,
            data: SpinLock::new(Vec::new()),
            modified: AtomicU64::new(time::unix_time()),
        })
    }
}

/// Files bigger than this are refused, so one runaway write can't take all
/// of memory.
const MAX_FILE_SIZE: u64 = 64 << 20;

impl Inode for File {
    fn metadata(&self) -> Metadata {
        Metadata {
            kind: self.kind,
            inode: self.inode,
            size: self.data.lock().len() as u64,
            permissions: self.permissions,
            modified: self.modified.load(Ordering::Relaxed),
        }
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
        self.modified.store(time::unix_time(), Ordering::Relaxed);
        Ok(buf.len())
    }

    fn truncate(&self, len: u64) -> Result<()> {
        if len > MAX_FILE_SIZE {
            return Err(Errno::EFBIG);
        }
        self.data.lock().resize(len as usize, 0);
        self.modified.store(time::unix_time(), Ordering::Relaxed);
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
    permissions: u32,
    entries: SpinLock<BTreeMap<String, Arc<dyn Inode>>>,
    modified: AtomicU64,
}

impl Directory {
    pub fn new(permissions: u32) -> Arc<Self> {
        Arc::new(Self {
            inode: next_inode_number(),
            permissions,
            entries: SpinLock::new(BTreeMap::new()),
            modified: AtomicU64::new(time::unix_time()),
        })
    }

    fn touch(&self) {
        self.modified.store(time::unix_time(), Ordering::Relaxed);
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
        Metadata {
            kind: FileKind::Directory,
            inode: self.inode,
            size: self.entries.lock().len() as u64,
            permissions: self.permissions,
            modified: self.modified.load(Ordering::Relaxed),
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        self.entries.lock().get(name).cloned().ok_or(Errno::ENOENT)
    }

    fn create(&self, name: &str, kind: FileKind) -> Result<Arc<dyn Inode>> {
        valid_name(name)?;
        let inode: Arc<dyn Inode> = match kind {
            FileKind::Directory => Directory::new(0o755),
            FileKind::Regular => File::new(FileKind::Regular, 0o644),
            FileKind::Symlink => File::new(FileKind::Symlink, 0o777),
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
