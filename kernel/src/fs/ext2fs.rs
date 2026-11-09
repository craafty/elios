//! ext2 volumes in the file system tree (an installed system's root, or
//! anything mounted with `mount -t ext2`).
//!
//! Every file the VFS sees is a [`Node`]: an inode number and a cached copy
//! of the inode, so `stat` and path lookups don't wait for the volume's
//! lock. There's one node per inode however it's reached, so the copies
//! stay right: anything that changes an inode refreshes its node.
//!
//! A file deleted while still open keeps its inode and blocks (as Unix
//! requires) until its last node goes away; then it's queued, and freed by
//! the next operation on the volume.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;

use super::mount::{self, MountRequest};
use super::writeback::{self, WriteBack};
use super::{Attributes, FileKind, Inode, Metadata};
use crate::drivers::block::Partition;
use crate::errno::{Errno, Result};
use crate::process::credentials::{Gid, Uid};
use crate::sync::{Mutex, SpinLock};
use crate::time;

type FileSystem = ext2::FileSystem<WriteBack>;

pub struct Volume {
    fs: Mutex<FileSystem>,
    nodes: SpinLock<BTreeMap<u32, Weak<Node>>>,
    /// Inodes with no names left whose last node has gone: freed by the
    /// next operation.
    orphans: SpinLock<Vec<u32>>,
}

pub struct Node {
    volume: Arc<Volume>,
    number: u32,
    inode: SpinLock<ext2::Inode>,
}

fn error(error: ext2::Error) -> Errno {
    use ext2::Error as E;
    match error {
        E::NotFound => Errno::ENOENT,
        E::Exists => Errno::EEXIST,
        E::NotDirectory => Errno::ENOTDIR,
        E::IsDirectory => Errno::EISDIR,
        E::NotEmpty => Errno::ENOTEMPTY,
        E::NoSpace => Errno::ENOSPC,
        E::InvalidName | E::Loop => Errno::EINVAL,
        E::NameTooLong => Errno::ENAMETOOLONG,
        E::TooBig => Errno::EFBIG,
        E::TooManyLinks => Errno::EMLINK,
        E::ReadOnly => Errno::EROFS,
        E::Unsupported => Errno::EOPNOTSUPP,
        E::Io | E::Corrupt => Errno::EIO,
    }
}

fn kind_of(kind: ext2::FileType) -> FileKind {
    match kind {
        ext2::FileType::Directory => FileKind::Directory,
        ext2::FileType::Symlink => FileKind::Symlink,
        ext2::FileType::CharDevice => FileKind::CharDevice,
        ext2::FileType::BlockDevice => FileKind::BlockDevice,
        ext2::FileType::Fifo => FileKind::Fifo,
        ext2::FileType::Socket => FileKind::Socket,
        ext2::FileType::Regular | ext2::FileType::Unknown => FileKind::Regular,
    }
}

/// Mounts the ext2 volume on a partition.
pub fn mount(partition: Partition, source: String, read_only: bool) -> Result<MountRequest> {
    let disk = String::from(partition.disk.name());
    let fs = FileSystem::mount(WriteBack::new(partition), time::unix_time, read_only).map_err(error)?;
    writeback::start_flushing();
    let volume = Arc::new(Volume {
        fs: Mutex::new(fs),
        nodes: SpinLock::new(BTreeMap::new()),
        orphans: SpinLock::new(Vec::new()),
    });
    let root = {
        let mut fs = volume.fs.lock();
        volume.node(&mut fs, ext2::ROOT)?
    };
    Ok(MountRequest { root, source, kind: "ext2", disk: Some(disk), volume: Some(volume) })
}

impl Volume {
    /// The node for inode `number`, made if there isn't one.
    fn node(self: &Arc<Self>, fs: &mut FileSystem, number: u32) -> Result<Arc<dyn Inode>> {
        if let Some(node) = self.nodes.lock().get(&number).and_then(Weak::upgrade) {
            return Ok(node);
        }
        let inode = fs.inode(number).map_err(error)?;
        let mut nodes = self.nodes.lock();
        if let Some(node) = nodes.get(&number).and_then(Weak::upgrade) {
            return Ok(node);
        }
        if nodes.len() > 1024 {
            nodes.retain(|_, node| node.strong_count() > 0);
        }
        let node = Arc::new(Node { volume: self.clone(), number, inode: SpinLock::new(inode) });
        nodes.insert(number, Arc::downgrade(&node));
        Ok(node)
    }

    /// Re-reads a node's inode after something changed it.
    fn refresh(&self, fs: &mut FileSystem, number: u32) {
        let node = self.nodes.lock().get(&number).and_then(Weak::upgrade);
        if let Some(node) = node
            && let Ok(inode) = fs.inode(number)
        {
            *node.inode.lock() = inode;
        }
    }

    /// Frees an inode whose last name was just removed, unless a node for
    /// it is still around (an open file); then it's freed when that goes.
    fn forget(&self, fs: &mut FileSystem, unlinked: ext2::Unlinked) {
        if !unlinked.orphaned {
            self.refresh(fs, unlinked.inode);
            return;
        }
        let alive = self.nodes.lock().get(&unlinked.inode).and_then(Weak::upgrade);
        match alive {
            Some(node) => {
                if let Ok(inode) = fs.inode(unlinked.inode) {
                    *node.inode.lock() = inode;
                }
            }
            None => {
                if let Err(problem) = fs.release(unlinked.inode) {
                    warn!("ext2: can't free inode {}: {problem:?}", unlinked.inode);
                }
            }
        }
    }

    /// The volume, locked, with any orphans freed first.
    fn lock(&self) -> crate::sync::MutexGuard<'_, FileSystem> {
        let mut fs = self.fs.lock();
        let orphans = core::mem::take(&mut *self.orphans.lock());
        for number in orphans {
            if let Err(problem) = fs.release(number) {
                warn!("ext2: can't free inode {number}: {problem:?}");
            }
        }
        fs
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // No names and no users: free it (later, since this may run where
        // the volume can't be locked).
        if self.inode.get_mut().links == 0 {
            self.volume.orphans.lock().push(self.number);
        }
    }
}

impl mount::Volume for Volume {
    fn sync(&self) -> Result<()> {
        self.lock().sync().map_err(error)
    }

    fn unmount(&self) -> Result<()> {
        let mut fs = self.lock();
        // The mount table holds the root's node; anything else alive is in
        // use.
        let live = self.nodes.lock().values().filter(|node| node.strong_count() > 0).count();
        fs.sync().map_err(error)?;
        if live > 1 {
            return Err(Errno::EBUSY);
        }
        fs.unmount().map_err(error)
    }

    fn read_only(&self) -> bool {
        self.fs.lock().is_read_only()
    }

    fn set_read_only(&self, read_only: bool) -> Result<()> {
        self.lock().set_read_only(read_only).map_err(error)
    }
}

impl Node {
    /// Another node of the same volume (for links and renames).
    fn same_volume<'a>(&self, other: &'a Arc<dyn Inode>) -> Result<&'a Node> {
        let node = other.as_any().and_then(|any| any.downcast_ref::<Node>()).ok_or(Errno::EXDEV)?;
        if !Arc::ptr_eq(&self.volume, &node.volume) {
            return Err(Errno::EXDEV);
        }
        Ok(node)
    }

    fn reload(&self, fs: &mut FileSystem) -> Result<()> {
        *self.inode.lock() = fs.inode(self.number).map_err(error)?;
        Ok(())
    }
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let inode = self.inode.lock();
        Metadata {
            kind: kind_of(inode.kind()),
            inode: u64::from(self.number),
            size: inode.size,
            permissions: u32::from(inode.permissions()),
            uid: inode.uid,
            gid: inode.gid,
            links: u32::from(inode.links),
            modified: u64::from(inode.modified),
            rdev: 0,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.volume.lock().read(self.number, offset, buf).map_err(error)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        // Mappings made from now on must see the change.
        crate::memory::object::file_changed(self);
        let mut fs = self.volume.lock();
        // A symbolic link is given its target once, right after creation.
        let written = if self.inode.lock().kind() == ext2::FileType::Symlink {
            let target = core::str::from_utf8(buf).map_err(|_| Errno::EINVAL)?;
            fs.set_link_target(self.number, target).map(|_| buf.len())
        } else {
            fs.write(self.number, offset, buf)
        };
        let reloaded = self.reload(&mut fs);
        let written = written.map_err(error)?;
        reloaded?;
        Ok(written)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        // Mappings made from now on must see the change.
        crate::memory::object::file_changed(self);
        let mut fs = self.volume.lock();
        let result = fs.truncate(self.number, len).map_err(error);
        self.reload(&mut fs)?;
        result
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let mut fs = self.volume.lock();
        let number = fs.lookup(self.number, name).map_err(error)?;
        self.volume.node(&mut fs, number)
    }

    fn create(&self, name: &str, kind: FileKind, attributes: Attributes) -> Result<Arc<dyn Inode>> {
        let kind = match kind {
            FileKind::Regular => ext2::FileType::Regular,
            FileKind::Directory => ext2::FileType::Directory,
            FileKind::Symlink => ext2::FileType::Symlink,
            _ => return Err(Errno::EPERM),
        };
        let owner = ext2::Owner { uid: attributes.uid, gid: attributes.gid };
        let mut fs = self.volume.lock();
        let created = fs.create(self.number, name, kind, attributes.permissions as u16, owner);
        self.reload(&mut fs)?;
        let number = created.map_err(error)?;
        self.volume.node(&mut fs, number)
    }

    fn link(&self, name: &str, inode: Arc<dyn Inode>) -> Result<()> {
        let target = self.same_volume(&inode)?;
        let mut fs = self.volume.lock();
        let result = fs.link(self.number, name, target.number).map_err(error);
        self.reload(&mut fs)?;
        target.reload(&mut fs)?;
        result
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut fs = self.volume.lock();
        let number = fs.lookup(self.number, name).map_err(error)?;
        let directory = fs.inode(number).map_err(error)?.kind() == ext2::FileType::Directory;
        let result = if directory { fs.remove_directory(self.number, name) } else { fs.unlink(self.number, name) };
        self.reload(&mut fs)?;
        let unlinked = result.map_err(error)?;
        self.volume.forget(&mut fs, unlinked);
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, Arc<dyn Inode>)>> {
        let mut fs = self.volume.lock();
        let listing = fs.list(self.number).map_err(error)?;
        listing.into_iter().map(|entry| Ok((entry.name, self.volume.node(&mut fs, entry.inode)?))).collect()
    }

    fn read_link(&self) -> Result<String> {
        self.volume.lock().read_link(self.number).map_err(error)
    }

    fn rename(&self, old_name: &str, new_parent: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        let target = self.same_volume(new_parent)?;
        let mut fs = self.volume.lock();
        let moved = fs.lookup(self.number, old_name).map_err(error)?;
        let result = fs.rename(self.number, old_name, target.number, new_name);
        self.reload(&mut fs)?;
        target.reload(&mut fs)?;
        self.volume.refresh(&mut fs, moved);
        if let Some(replaced) = result.map_err(error)? {
            self.volume.forget(&mut fs, replaced);
        }
        Ok(())
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        let mut fs = self.volume.lock();
        let inode = fs.set_attributes(self.number, Some(permissions as u16), None, None).map_err(error)?;
        *self.inode.lock() = inode;
        Ok(())
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        let mut fs = self.volume.lock();
        let owner = ext2::Owner { uid, gid };
        let inode = fs.set_attributes(self.number, None, Some(owner), None).map_err(error)?;
        *self.inode.lock() = inode;
        Ok(())
    }

    fn set_modified(&self, time: u64) -> Result<()> {
        let mut fs = self.volume.lock();
        let time = time.min(u64::from(u32::MAX)) as u32;
        let inode = fs.set_attributes(self.number, None, None, Some(time)).map_err(error)?;
        *self.inode.lock() = inode;
        Ok(())
    }

    fn space(&self) -> (u64, u64, u64, u64) {
        const EXT2_SUPER_MAGIC: u64 = 0xEF53;
        let fs = self.volume.fs.lock();
        (fs.block_size() as u64, fs.total_blocks(), fs.free_blocks(), EXT2_SUPER_MAGIC)
    }

    fn filesystem(&self) -> usize {
        Arc::as_ptr(&self.volume) as usize
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::super::mount;
    use super::super::{Attributes, FileKind, lookup};
    use super::*;
    use crate::drivers::block::Disk;

    /// A disk in memory.
    pub(in crate::fs) struct MemoryDisk {
        pub data: SpinLock<Vec<u8>>,
    }

    impl Disk for MemoryDisk {
        fn name(&self) -> &str {
            "ram-test"
        }

        fn model(&self) -> &str {
            "test"
        }

        fn sectors(&self) -> u64 {
            self.data.lock().len() as u64 / 512
        }

        fn read(&self, lba: u64, buffer: &mut [u8]) -> core::result::Result<(), &'static str> {
            let start = lba as usize * 512;
            buffer.copy_from_slice(&self.data.lock()[start..start + buffer.len()]);
            Ok(())
        }

        fn write(&self, lba: u64, data: &[u8]) -> core::result::Result<(), &'static str> {
            let start = lba as usize * 512;
            self.data.lock()[start..start + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    crate::ktest! {
        fn deleted_files_live_while_open() {
            let disk: Arc<dyn Disk> = Arc::new(MemoryDisk { data: SpinLock::new(vec![0; 8 << 20]) });
            let mut partition = Partition::whole(&disk);
            let options = ext2::FormatOptions { label: "test", uuid: [1; 16], time: 0, block_size: None };
            ext2::FileSystem::format(&mut partition, &options).unwrap();
            let request = mount(partition, "ram-test".into(), false).unwrap();
            let volume = request.volume.clone().unwrap();
            let tmp = lookup("/", "/tmp", true).unwrap();
            tmp.create("ext2-test", FileKind::Directory, Attributes::root(0o755)).unwrap();
            mount::mount("/tmp/ext2-test", request).unwrap();

            let root = lookup("/", "/tmp/ext2-test", true).unwrap();
            assert_eq!(root.metadata().inode, u64::from(ext2::ROOT));
            let free_blocks = || root.space().2;
            let free_before = free_blocks();
            let file = root.create("doomed", FileKind::Regular, Attributes { permissions: 0o640, uid: 7, gid: 8 }).unwrap();
            file.write_at(0, &[0x5A; 100_000]).unwrap();
            let metadata = lookup("/", "/tmp/ext2-test/doomed", true).unwrap().metadata();
            assert_eq!((metadata.size, metadata.uid, metadata.gid, metadata.permissions), (100_000, 7, 8, 0o640));

            // Unlinked but still held: the data stays readable.
            root.unlink("doomed").unwrap();
            assert!(lookup("/", "/tmp/ext2-test/doomed", true).is_err());
            let mut back = [0u8; 16];
            assert_eq!(file.read_at(99_990, &mut back), Ok(10));
            assert_eq!(back[..10], [0x5A; 10]);
            // Busy until it's let go; then the next operation frees it.
            assert_eq!(volume.unmount(), Err(Errno::EBUSY));
            assert!(free_blocks() < free_before);
            drop(file);
            root.lookup("lost+found").unwrap();
            assert_eq!(free_blocks(), free_before);
            assert_eq!(mount::unmount("/tmp/ext2-test", false), Ok(()));
            tmp.unlink("ext2-test").unwrap();
        }
    }
}
