//! Mounting: attaching file systems to directories.
//!
//! The mount table lists which file system covers which directory. Path
//! lookup checks each directory it reaches against the table and, if one is
//! covered, continues in the mounted file system's root instead. A covered
//! directory is identified by its file system and inode number, so it's
//! found however the path to it was spelled.
//!
//! The root is a mount too: an in-memory tree unpacked from the initrd (the
//! live system), or an ext2 partition (an installed one), chosen at boot.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Attributes, FileKind, Inode, devfs, procfs, ramfs, sysfs};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;

/// A file system on a device: what the mount table needs to write it back
/// and detach it.
pub trait Volume: Send + Sync {
    /// Writes everything changed to the device.
    fn sync(&self) -> Result<()>;
    /// What `fsync` asks for: by default the same as [`sync`](Self::sync).
    /// A volume on slow storage may instead have the changes written
    /// within a moment, so a program calling it many times a second
    /// doesn't pay for a write each time.
    fn fsync(&self) -> Result<()> {
        self.sync()
    }
    /// Writes everything back and marks the volume cleanly unmounted.
    /// Fails with `EBUSY` if files on it are still in use.
    fn unmount(&self) -> Result<()>;
    fn read_only(&self) -> bool;
    fn set_read_only(&self, read_only: bool) -> Result<()>;
}

/// One entry in the mount table.
pub struct Mount {
    /// The directory it covers: (file system, inode number). The root
    /// mount covers nothing.
    covered: Option<(usize, u64)>,
    pub root: Arc<dyn Inode>,
    /// Where it's mounted.
    pub path: String,
    /// What's mounted: a device (`/dev/sda2`) or a name (`proc`).
    pub source: String,
    /// The file system type, as Linux names it (`ext2`, `vfat`, `proc`).
    pub kind: &'static str,
    /// The disk it's on, if any (so partition tables aren't reread under
    /// it).
    pub disk: Option<String>,
    pub volume: Option<Arc<dyn Volume>>,
}

static MOUNTS: SpinLock<Vec<Mount>> = SpinLock::new(Vec::new());

/// What's being mounted, for [`mount`].
pub struct MountRequest {
    pub root: Arc<dyn Inode>,
    pub source: String,
    pub kind: &'static str,
    pub disk: Option<String>,
    pub volume: Option<Arc<dyn Volume>>,
}

pub fn root() -> Arc<dyn Inode> {
    try_root().expect("no root file system")
}

/// The root, if one has been mounted yet (early in startup it hasn't).
pub fn try_root() -> Option<Arc<dyn Inode>> {
    MOUNTS.lock().iter().find(|mount| mount.covered.is_none()).map(|mount| mount.root.clone())
}

/// The root of whatever is mounted on `inode`, if anything; otherwise
/// `inode` itself. Mounts can stack: the last one wins.
pub fn cross(inode: Arc<dyn Inode>) -> Arc<dyn Inode> {
    let filesystem = inode.filesystem();
    let candidates: Vec<(u64, Arc<dyn Inode>)> = MOUNTS
        .lock()
        .iter()
        .filter_map(|mount| mount.covered.filter(|(fs, _)| *fs == filesystem).map(|(_, n)| (n, mount.root.clone())))
        .collect();
    if candidates.is_empty() {
        return inode;
    }
    // Metadata may take other locks, so it's read outside the table's.
    let number = inode.metadata().inode;
    match candidates.into_iter().rev().find(|(covered, _)| *covered == number) {
        Some((_, root)) => cross(root),
        None => inode,
    }
}

/// Mounts a file system on the directory at `path` (or as the root, for
/// `/` when nothing is mounted yet).
pub fn mount(path: &str, request: MountRequest) -> Result<()> {
    let path = super::normalize("/", path);
    let covered = if MOUNTS.lock().is_empty() && path == "/" {
        None
    } else {
        let directory = super::lookup("/", &path, true)?;
        let metadata = directory.metadata();
        if metadata.kind != FileKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        Some((directory.filesystem(), metadata.inode))
    };
    MOUNTS.lock().push(Mount {
        covered,
        root: request.root,
        path,
        source: request.source,
        kind: request.kind,
        disk: request.disk,
        volume: request.volume,
    });
    Ok(())
}

/// Detaches the file system mounted at `path`. Unless `force`, it must not
/// be in use.
pub fn unmount(path: &str, force: bool) -> Result<()> {
    let path = super::normalize("/", path);
    let volume = {
        let mounts = MOUNTS.lock();
        let mount = mounts.iter().rev().find(|mount| mount.path == path).ok_or(Errno::EINVAL)?;
        if mount.covered.is_none() {
            return Err(Errno::EBUSY);
        }
        // Something mounted inside it has to go first.
        let inside = alloc::format!("{}/", path.trim_end_matches('/'));
        if mounts.iter().any(|other| other.path.starts_with(&inside)) {
            return Err(Errno::EBUSY);
        }
        mount.volume.clone()
    };
    if let Some(volume) = volume {
        match volume.unmount() {
            Err(Errno::EBUSY) if force => {}
            result => result?,
        }
    }
    let mut mounts = MOUNTS.lock();
    if let Some(index) = mounts.iter().rposition(|mount| mount.path == path) {
        mounts.remove(index);
    }
    Ok(())
}

/// Changes a mount between read-only and read-write.
pub fn remount(path: &str, read_only: bool) -> Result<()> {
    let path = super::normalize("/", path);
    let volume = {
        let mounts = MOUNTS.lock();
        let mount = mounts.iter().rev().find(|mount| mount.path == path).ok_or(Errno::EINVAL)?;
        mount.volume.clone()
    };
    match volume {
        Some(volume) => volume.set_read_only(read_only),
        None if read_only => Err(Errno::EINVAL),
        None => Ok(()),
    }
}

/// Writes every mounted volume back to its device.
pub fn sync_all() {
    let volumes: Vec<Arc<dyn Volume>> = MOUNTS.lock().iter().filter_map(|mount| mount.volume.clone()).collect();
    for volume in volumes {
        if let Err(error) = volume.sync() {
            warn!("fs: sync failed: {error:?}");
        }
    }
}

/// Writes back the volume holding files of file system `filesystem` (see
/// [`Inode::filesystem`]), for `fsync`: one file's changes shouldn't wait
/// for every other disk's. Nothing to do for in-memory file systems.
pub fn sync_filesystem(filesystem: usize) {
    let volumes: Vec<Arc<dyn Volume>> = MOUNTS
        .lock()
        .iter()
        .filter(|mount| mount.root.filesystem() == filesystem)
        .filter_map(|mount| mount.volume.clone())
        .collect();
    for volume in volumes {
        if let Err(error) = volume.fsync() {
            warn!("fs: sync failed: {error:?}");
        }
    }
}

/// Before the machine turns off: writes every volume back and marks it
/// cleanly unmounted (files may still be open; nothing more will be
/// written).
pub fn shut_down() {
    let volumes: Vec<(String, Arc<dyn Volume>)> =
        MOUNTS.lock().iter().filter_map(|mount| Some((mount.path.clone(), mount.volume.clone()?))).collect();
    for (path, volume) in volumes {
        match volume.unmount() {
            Ok(()) | Err(Errno::EBUSY) => {
                let _ = volume.sync();
            }
            Err(error) => warn!("fs: {path}: can't unmount: {error:?}"),
        }
    }
}

/// Whether a file system is mounted at `path` (exactly).
pub fn is_mounted(path: &str) -> bool {
    MOUNTS.lock().iter().any(|mount| mount.path == path)
}

/// Whether any partition of disk `name` is mounted.
pub fn disk_in_use(name: &str) -> bool {
    MOUNTS.lock().iter().any(|mount| mount.disk.as_deref() == Some(name))
}

/// Whether the device at `source` (`/dev/sda1`) is mounted somewhere.
pub fn source_mounted(source: &str) -> bool {
    MOUNTS.lock().iter().any(|mount| mount.source == source)
}

/// The mount table as /proc/mounts shows it.
pub fn table() -> String {
    let mounts = MOUNTS.lock();
    let mut text = String::new();
    for mount in mounts.iter() {
        let mode = if mount.volume.as_ref().is_some_and(|volume| volume.read_only()) { "ro" } else { "rw" };
        text.push_str(&alloc::format!("{} {} {} {mode},relatime 0 0\n", mount.source, mount.path, mount.kind));
    }
    text
}

/// The live system's root: an in-memory tree with the initrd unpacked
/// into it.
pub fn init_memory_root(initrd: Option<&[u8]>) {
    let root: Arc<dyn Inode> = ramfs::Directory::new(Attributes::root(0o755));
    mount("/", MountRequest { root, source: "rootfs".into(), kind: "rootfs", disk: None, volume: None })
        .expect("can't mount the root");
    if let Some(archive) = initrd {
        match super::cpio::unpack(archive) {
            Ok(count) => info!("fs: unpacked {count} files from the initrd"),
            Err(error) => warn!("fs: initrd is damaged: {error}"),
        }
    }
}

/// Mounts a disk file system as the root.
pub fn init_disk_root(request: MountRequest) {
    mount("/", request).expect("can't mount the root");
}

/// Makes the directories eliOS needs at the top of the root (if they're
/// missing) and mounts /dev, /proc and /sys. On a disk root, /tmp and /run
/// get fresh in-memory file systems, so they start empty at every boot.
pub fn mount_system(disk_root: bool) {
    let root = root();
    // /tmp is for everyone, but people can only delete their own files.
    let directories = [
        ("dev", 0o755),
        ("proc", 0o555),
        ("sys", 0o555),
        ("tmp", 0o1777),
        ("home", 0o755),
        ("root", 0o700),
        ("run", 0o755),
        ("disk", 0o755),
        ("mnt", 0o755),
        ("data", 0o755),
    ];
    for (name, permissions) in directories {
        if root.lookup(name).is_err() {
            let _ = root.create(name, FileKind::Directory, Attributes::root(permissions));
        }
    }
    let virtual_mounts: [(&str, &'static str, Arc<dyn Inode>); 3] = [
        ("/dev", "devtmpfs", devfs::directory()),
        ("/proc", "proc", procfs::create()),
        ("/sys", "sysfs", sysfs::create()),
    ];
    for (path, kind, root) in virtual_mounts {
        let request = MountRequest { root, source: kind.to_string(), kind, disk: None, volume: None };
        if let Err(error) = mount(path, request) {
            warn!("fs: can't mount {path}: {error:?}");
        }
    }
    if disk_root {
        for (path, permissions) in [("/tmp", 0o1777), ("/run", 0o755)] {
            let root: Arc<dyn Inode> = ramfs::Directory::new(Attributes::root(permissions));
            let request = MountRequest { root, source: "tmpfs".into(), kind: "tmpfs", disk: None, volume: None };
            if let Err(error) = mount(path, request) {
                warn!("fs: can't mount {path}: {error:?}");
            }
        }
    }
}
