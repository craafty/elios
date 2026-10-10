//! FAT volumes (disks) in the file system tree.
//!
//! Every file and directory the VFS sees is a [`Node`] wrapping the FAT
//! entry. Nodes are cached by the entry's position on disk, so the same
//! file is always the same node, however it's reached. FAT has no symbolic
//! links, permissions or hard links.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use super::{FileKind, Inode, Metadata, ramfs};
use crate::drivers::block::Partition;
use crate::errno::{Errno, Result};
use crate::sync::{Mutex, SpinLock};
use crate::time;

pub struct Volume {
    fs: Mutex<fat::FileSystem<Partition>>,
    nodes: SpinLock<BTreeMap<(u32, u32), Weak<Node>>>,
}

pub struct Node {
    volume: Arc<Volume>,
    entry: SpinLock<fat::Entry>,
    inode: u64,
    /// Set once the file is deleted: its clusters may be reused, so it can't
    /// be read or written any more.
    deleted: AtomicBool,
}

fn error(error: fat::Error) -> Errno {
    match error {
        fat::Error::NotFound => Errno::ENOENT,
        fat::Error::Exists => Errno::EEXIST,
        fat::Error::NotDirectory => Errno::ENOTDIR,
        fat::Error::IsDirectory => Errno::EISDIR,
        fat::Error::NotEmpty => Errno::ENOTEMPTY,
        fat::Error::NoSpace => Errno::ENOSPC,
        fat::Error::InvalidName | fat::Error::Unsupported => Errno::EINVAL,
        fat::Error::Io | fat::Error::Corrupt => Errno::EIO,
    }
}

/// Mounts the FAT32 file system on a partition, returning its root.
pub fn mount(partition: Partition) -> Result<(Arc<dyn Inode>, u64, u64)> {
    let mut fs = fat::FileSystem::mount(partition, time::unix_time).map_err(error)?;
    let total = fs.total_bytes();
    let free = fs.free_bytes().map_err(error)?;
    let root = fs.root();
    let volume = Arc::new(Volume { fs: Mutex::new(fs), nodes: SpinLock::new(BTreeMap::new()) });
    Ok((volume.node(root), total, free))
}

impl Volume {
    fn node(self: &Arc<Self>, entry: fat::Entry) -> Arc<dyn Inode> {
        let id = entry.id();
        let mut nodes = self.nodes.lock();
        if let Some(node) = nodes.get(&id).and_then(Weak::upgrade) {
            return node;
        }
        if nodes.len() > 1024 {
            nodes.retain(|_, node| node.strong_count() > 0);
        }
        let node = Arc::new(Node {
            volume: self.clone(),
            entry: SpinLock::new(entry),
            inode: ramfs::next_inode_number(),
            deleted: AtomicBool::new(false),
        });
        nodes.insert(id, Arc::downgrade(&node));
        node
    }

    /// The up-to-date entry for `found` (from a directory listing): a cached
    /// node's copy, if there is one.
    fn current(&self, found: fat::Entry) -> fat::Entry {
        self.nodes.lock().get(&found.id()).and_then(Weak::upgrade).map_or(found, |node| node.entry.lock().clone())
    }
}

impl Node {
    fn entry(&self) -> Result<fat::Entry> {
        if self.deleted.load(Ordering::Relaxed) {
            return Err(Errno::ENOENT);
        }
        Ok(self.entry.lock().clone())
    }
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let entry = self.entry.lock();
        Metadata {
            kind: if entry.directory { FileKind::Directory } else { FileKind::Regular },
            inode: self.inode,
            size: u64::from(entry.size),
            permissions: if entry.directory { 0o755 } else { 0o644 },
            modified: entry.modified_unix(),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut fs = self.volume.fs.lock();
        fs.read(&self.entry()?, offset, buf).map_err(error)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let mut fs = self.volume.fs.lock();
        let mut entry = self.entry()?;
        let written = fs.write(&mut entry, offset, buf).map_err(error)?;
        *self.entry.lock() = entry;
        Ok(written)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        let mut fs = self.volume.fs.lock();
        let mut entry = self.entry()?;
        fs.truncate(&mut entry, len).map_err(error)?;
        *self.entry.lock() = entry;
        Ok(())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let found = self.volume.fs.lock().lookup(&self.entry()?, name).map_err(error)?;
        Ok(self.volume.node(found))
    }

    fn create(&self, name: &str, kind: FileKind) -> Result<Arc<dyn Inode>> {
        let directory = match kind {
            FileKind::Regular => false,
            FileKind::Directory => true,
            _ => return Err(Errno::EPERM),
        };
        let created = self.volume.fs.lock().create(&self.entry()?, name, directory).map_err(error)?;
        Ok(self.volume.node(created))
    }

    fn link(&self, _name: &str, _inode: Arc<dyn Inode>) -> Result<()> {
        Err(Errno::EPERM)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut fs = self.volume.fs.lock();
        let found = fs.lookup(&self.entry()?, name).map_err(error)?;
        let id = found.id();
        let current = self.volume.current(found);
        fs.remove(&current).map_err(error)?;
        if let Some(node) = self.volume.nodes.lock().remove(&id).and_then(|node| node.upgrade()) {
            node.deleted.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, Arc<dyn Inode>)>> {
        let listing = self.volume.fs.lock().list(&self.entry()?).map_err(error)?;
        Ok(listing.into_iter().map(|entry| (entry.name.clone(), self.volume.node(entry))).collect())
    }

    fn rename(&self, old_name: &str, new_parent: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        let target = new_parent.as_any().and_then(|any| any.downcast_ref::<Node>()).ok_or(Errno::EXDEV)?;
        if !Arc::ptr_eq(&self.volume, &target.volume) {
            return Err(Errno::EXDEV);
        }
        let mut fs = self.volume.fs.lock();
        let found = fs.lookup(&self.entry()?, old_name).map_err(error)?;
        let old_id = found.id();
        let current = self.volume.current(found);
        let target_directory = target.entry()?;
        if let Ok(existing) = fs.lookup(&target_directory, new_name) {
            if existing.id() == old_id {
                return Ok(());
            }
            fs.remove(&self.volume.current(existing)).map_err(error)?;
        }
        let moved = fs.rename(&current, &target_directory, new_name).map_err(error)?;
        let mut nodes = self.volume.nodes.lock();
        if let Some(node) = nodes.remove(&old_id).and_then(|node| node.upgrade()) {
            *node.entry.lock() = moved.clone();
            nodes.insert(moved.id(), Arc::downgrade(&node));
        }
        Ok(())
    }

    fn filesystem(&self) -> usize {
        Arc::as_ptr(&self.volume) as usize
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FileKind, lookup, lookup_parent};
    use crate::errno::Errno;

    crate::ktest! {
        fn the_boot_disk_is_mounted_and_writable() {
            let Ok(disk) = lookup("/", "/disk", true) else {
                panic!("/disk isn't mounted");
            };
            assert!(lookup("/", "/disk/EFI/eliOS/kernel.elf", true).is_ok());

            let directory = disk.create("Test Folder With Spaces", FileKind::Directory).unwrap();
            let file = directory.create("a rather long file name.txt", FileKind::Regular).unwrap();
            let data: alloc::vec::Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
            file.write_at(0, &data).unwrap();
            let again = lookup("/", "/disk/test folder with spaces/A RATHER LONG FILE NAME.TXT", true).unwrap();
            let mut back = alloc::vec![0; data.len()];
            assert_eq!(again.read_at(0, &mut back), Ok(data.len()));
            assert_eq!(back, data);

            // Renames stay on the disk; moves to memory are refused.
            directory.rename("a rather long file name.txt", &disk, "moved.txt").unwrap();
            assert_eq!(lookup("/", "/disk/moved.txt", true).unwrap().metadata().size, 20_000);
            let (tmp, _) = lookup_parent("/", "/tmp/x").unwrap();
            assert_eq!(disk.rename("moved.txt", &tmp, "x"), Err(Errno::EXDEV));

            disk.unlink("moved.txt").unwrap();
            disk.unlink("Test Folder With Spaces").unwrap();
            assert!(lookup("/", "/disk/moved.txt", true).is_err());
        }
    }
}
