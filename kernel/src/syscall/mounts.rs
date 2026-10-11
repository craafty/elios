//! Mounting and syncing: `mount`, `umount2`, `sync`.

use alloc::string::String;
use alloc::sync::Arc;

use crate::errno::{Errno, Result};
use crate::fs::mount::{self, MountRequest};
use crate::fs::{self, Attributes, FileKind, Inode, devfs, ext2fs, fatfs, procfs, ramfs, sysfs};
use crate::process::{self, credentials, user};

// mount flags (Linux values).
const MS_RDONLY: u64 = 1;
const MS_REMOUNT: u64 = 32;
const MS_BIND: u64 = 4096;
// umount2 flags.
const MNT_FORCE: u64 = 1;
const MNT_DETACH: u64 = 2;

fn optional_string(address: u64) -> Result<Option<String>> {
    if address == 0 { Ok(None) } else { user::read_string(address).map(Some) }
}

/// `mount(source, target, type, flags, data)`: root only. Disk file
/// systems (`ext2`, `vfat`) mount a block device; `proc`, `sysfs`,
/// `devtmpfs` and `tmpfs` need none. Without a type (or with `auto`), ext2
/// and then FAT are tried. Also `MS_REMOUNT` (to switch between read-only
/// and read-write) and `MS_BIND` (to show a directory somewhere else).
pub fn mount(source: u64, target: u64, kind: u64, flags: u64, _data: u64) -> Result<u64> {
    if !credentials::current().is_root() {
        return Err(Errno::EPERM);
    }
    let source = optional_string(source)?.unwrap_or_default();
    let target = user::read_string(target)?;
    let kind = optional_string(kind)?.unwrap_or_default();
    let cwd = process::current().cwd.lock().clone();
    let target = fs::normalize(&cwd, &target);
    let read_only = flags & MS_RDONLY != 0;

    if flags & MS_REMOUNT != 0 {
        return mount::remount(&target, read_only).map(|_| 0);
    }
    if flags & MS_BIND != 0 {
        let root = fs::lookup(&cwd, &source, true)?;
        let request = MountRequest { root, source, kind: "none", disk: None, volume: None };
        return mount::mount(&target, request).map(|_| 0);
    }
    let virtual_root: Option<(Arc<dyn Inode>, &'static str)> = match kind.as_str() {
        "proc" => Some((procfs::create(), "proc")),
        "sysfs" => Some((sysfs::create(), "sysfs")),
        "devtmpfs" => Some((devfs::directory(), "devtmpfs")),
        "tmpfs" | "ramfs" => Some((ramfs::Directory::new(Attributes::root(0o1777)), "tmpfs")),
        _ => None,
    };
    if let Some((root, kind)) = virtual_root {
        let request = MountRequest { root, source, kind, disk: None, volume: None };
        return mount::mount(&target, request).map(|_| 0);
    }

    let device = fs::lookup(&cwd, &source, true)?;
    if device.metadata().kind != FileKind::BlockDevice {
        return Err(Errno::ENOTBLK);
    }
    let partition = devfs::partition_of(&device).ok_or(Errno::ENOTBLK)?;
    let source = fs::normalize(&cwd, &source);
    if mount::source_mounted(&source) {
        return Err(Errno::EBUSY);
    }
    let request = match kind.as_str() {
        "ext2" | "ext3" | "ext4" => ext2fs::mount(partition, source, read_only)?,
        "vfat" | "fat" | "msdos" => fatfs::mount(partition, source)?.0,
        "" | "auto" => match ext2fs::mount(partition.clone(), source.clone(), read_only) {
            Ok(request) => request,
            Err(_) => fatfs::mount(partition, source).map_err(|_| Errno::EINVAL)?.0,
        },
        _ => return Err(Errno::ENODEV),
    };
    mount::mount(&target, request).map(|_| 0)
}

/// `umount2(target, flags)`: root only. Fails with `EBUSY` while files on
/// it are open, unless forced.
pub fn umount(target: u64, flags: u64) -> Result<u64> {
    if !credentials::current().is_root() {
        return Err(Errno::EPERM);
    }
    let target = user::read_string(target)?;
    let cwd = process::current().cwd.lock().clone();
    mount::unmount(&fs::normalize(&cwd, &target), flags & (MNT_FORCE | MNT_DETACH) != 0).map(|_| 0)
}

/// `sync`, `syncfs`, `fsync` and `fdatasync`: writes everything back.
pub fn sync() -> Result<u64> {
    mount::sync_all();
    Ok(0)
}
