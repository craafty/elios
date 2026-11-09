//! Device files under /dev: the null and random devices, terminals, and a
//! file for each disk and partition.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;

use super::{FileKind, Inode, Metadata, ramfs};
use crate::drivers::block::{self, Disk, Partition};
use crate::errno::{Errno, Result};
use crate::process::{credentials, user};
use crate::sync::Once;
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

/// /dev/tty: the caller's controlling terminal (the console if it has
/// none), open to everyone since it's their own.
struct ControllingTerminal {
    inode: u64,
}

impl ControllingTerminal {
    fn terminal(&self) -> Arc<dyn Inode> {
        let terminal = crate::task::current_process().and_then(|process| process.terminal.lock().clone());
        terminal.unwrap_or_else(tty::console)
    }
}

impl Inode for ControllingTerminal {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o666)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.terminal().read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        self.terminal().write_at(offset, buf)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.terminal().read_stream(buf, nonblocking)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
        self.terminal().write_stream(buf, nonblocking)
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        self.terminal().ioctl(request, argument)
    }

    fn poll(&self, flags: u32) -> u16 {
        self.terminal().poll(flags)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

/// The `disk` group (as on Linux), which owns the disk device files.
const DISK_GROUP: u32 = 6;

// Block device ioctls (Linux values).
const BLKRRPART: u64 = 0x125F;
const BLKGETSIZE: u64 = 0x1260;
const BLKFLSBUF: u64 = 0x1261;
const BLKSSZGET: u64 = 0x1268;
const BLKPBSZGET: u64 = 0x127B;
const BLKGETSIZE64: u64 = 0x8008_1272;

/// A disk or partition as a file: reading and writing it reads and writes
/// the sectors directly. Partitioning and formatting tools use these.
pub struct BlockDeviceFile {
    inode: u64,
    pub partition: Partition,
}

impl BlockDeviceFile {
    fn size(&self) -> u64 {
        self.partition.sectors * 512
    }
}

/// Bytes moved per device request.
const CHUNK: usize = 128 * 1024;

impl Inode for BlockDeviceFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            size: self.size(),
            gid: DISK_GROUP,
            rdev: block::device_number(&self.partition),
            ..Metadata::virtual_file(FileKind::BlockDevice, self.inode, 0o660)
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let end = (offset + buf.len() as u64).min(self.size());
        if offset >= end {
            return Ok(0);
        }
        // Whole sectors are read into a bounce buffer, then the part asked
        // for is copied out.
        let mut done = 0;
        while offset + (done as u64) < end {
            let position = offset + done as u64;
            let first = position / 512;
            let within = (position % 512) as usize;
            let count = (end - position).min((CHUNK - within) as u64) as usize;
            let mut sectors = vec![0u8; (within + count).next_multiple_of(512)];
            self.partition.read(first, &mut sectors).map_err(|_| Errno::EIO)?;
            buf[done..done + count].copy_from_slice(&sectors[within..within + count]);
            done += count;
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if offset >= self.size() && !buf.is_empty() {
            return Err(Errno::ENOSPC);
        }
        let end = (offset + buf.len() as u64).min(self.size());
        let mut done = 0;
        while offset + (done as u64) < end {
            let position = offset + done as u64;
            let first = position / 512;
            let within = (position % 512) as usize;
            let count = (end - position).min((CHUNK - within) as u64) as usize;
            let mut sectors = vec![0u8; (within + count).next_multiple_of(512)];
            // Partial sectors keep what's around the part written.
            if within != 0 || !count.is_multiple_of(512) {
                self.partition.read(first, &mut sectors).map_err(|_| Errno::EIO)?;
            }
            sectors[within..within + count].copy_from_slice(&buf[done..done + count]);
            self.partition.write(first, &sectors).map_err(|_| Errno::EIO)?;
            done += count;
        }
        Ok(done)
    }

    fn truncate(&self, _len: u64) -> Result<()> {
        // `>` redirections truncate; a device just keeps its size.
        Ok(())
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            BLKGETSIZE64 => user::write_value(argument, &self.size()).map(|_| 0),
            BLKGETSIZE => user::write_value(argument, &self.partition.sectors).map(|_| 0),
            BLKSSZGET | BLKPBSZGET => user::write_value(argument, &512u32).map(|_| 0),
            BLKFLSBUF => Ok(0),
            BLKRRPART => {
                if !credentials::current().is_root() {
                    return Err(Errno::EACCES);
                }
                if self.partition.number != 0 {
                    return Err(Errno::EINVAL);
                }
                reread_partitions(&self.partition.disk)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

static DEV: Once<Arc<dyn Inode>> = Once::new();

/// The /dev directory, made (with the standard devices) on first use.
pub fn directory() -> Arc<dyn Inode> {
    if let Some(dev) = DEV.get() {
        return dev.clone();
    }
    let dev: Arc<dyn Inode> = ramfs::Directory::new(super::Attributes::root(0o755));
    let devices = [("null", Kind::Null), ("zero", Kind::Zero), ("random", Kind::Random), ("urandom", Kind::Random)];
    for (name, kind) in devices {
        let _ = dev.link(name, Arc::new(Device { inode: ramfs::next_inode_number(), kind }));
    }
    let console: Arc<dyn Inode> = tty::console();
    let _ = dev.link("console", console);
    let _ = dev.link("tty", Arc::new(ControllingTerminal { inode: ramfs::next_inode_number() }));
    let _ = dev.link("ptmx", tty::PtyMultiplexer::new());
    // Shared memory files (shm_open), writable by everyone like /tmp.
    let _ = dev.link("shm", ramfs::Directory::new(super::Attributes::root(0o1777)));
    match DEV.set(dev) {
        Ok(dev) => dev.clone(),
        Err(_) => DEV.get().expect("set by someone else").clone(),
    }
}

fn add_node(partition: Partition) {
    let node = Arc::new(BlockDeviceFile { inode: ramfs::next_inode_number(), partition });
    let _ = directory().link(&node.partition.name(), node);
}

/// Adds the files for a new disk and its partitions.
pub fn add_disk(disk: &Arc<dyn Disk>) {
    add_node(Partition::whole(disk));
    for partition in block::partitions(disk) {
        add_node(partition);
    }
}

/// Removes a disk's files (and its partitions').
pub fn remove_disk(name: &str) {
    let dev = directory();
    let Ok(entries) = dev.entries() else { return };
    for (entry, inode) in entries {
        let on_disk = inode
            .as_any()
            .and_then(|any| any.downcast_ref::<BlockDeviceFile>())
            .is_some_and(|file| file.partition.disk.name() == name);
        if on_disk {
            let _ = dev.unlink(&entry);
        }
    }
}

/// Rereads a disk's partition table (after a partitioning tool changed it)
/// and replaces its partitions' files. Refused if a mounted partition
/// would move, shrink or go away; adding one beside mounted ones (as a
/// live stick does for its data) is fine.
fn reread_partitions(disk: &Arc<dyn Disk>) -> Result<u64> {
    let name = String::from(disk.name());
    if super::mount::disk_in_use(&name) {
        let new = block::partitions(disk);
        let dev = directory();
        for (entry, inode) in dev.entries()? {
            let Some(file) = inode.as_any().and_then(|any| any.downcast_ref::<BlockDeviceFile>()) else { continue };
            let old = &file.partition;
            if old.disk.name() != name
                || old.number == 0
                || !super::mount::source_mounted(&alloc::format!("/dev/{entry}"))
            {
                continue;
            }
            if !new.iter().any(|p| p.number == old.number && p.start == old.start && p.sectors == old.sectors) {
                return Err(Errno::EBUSY);
            }
        }
    }
    remove_disk(&name);
    add_disk(disk);
    Ok(0)
}

/// The partition behind a block device file, for mounting it.
pub fn partition_of(inode: &Arc<dyn Inode>) -> Option<Partition> {
    inode.as_any()?.downcast_ref::<BlockDeviceFile>().map(|file| file.partition.clone())
}
