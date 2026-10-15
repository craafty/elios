//! The virtual file system.
//!
//! Everything a program can open is an [`Inode`]: files and directories in
//! the in-memory file system, devices like the console, pipes. Paths are
//! resolved from the root (or a process's working directory) one component
//! at a time, following symbolic links.
//!
//! An [`OpenFile`] is what `open` creates: an inode plus a position and
//! flags. File descriptors point at open files, and `dup` and `fork` share
//! them, so the position is shared too, as on Linux.

pub mod cpio;
pub mod devfs;
pub mod efivarfs;
pub mod ext2fs;
pub mod fatfs;
pub mod mount;
pub mod pipe;
pub mod procfs;
pub mod ramfs;
pub mod sysfs;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::process::credentials::{self, Credentials, Gid, Uid};
use crate::sync::SpinLock;
use crate::task::WaitQueue;

// Readiness bits for `poll` (Linux values).
pub const POLLIN: u16 = 0x1;
pub const POLLOUT: u16 = 0x4;
pub const POLLERR: u16 = 0x8;
pub const POLLHUP: u16 = 0x10;

/// Woken whenever any file might have become ready, so `poll` can recheck.
pub static POLL_EVENTS: WaitQueue = WaitQueue::new();

/// Tells waiting `poll` calls that something changed.
pub fn notify_poll() {
    POLL_EVENTS.wake_all();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl FileKind {
    /// The file type bits of `st_mode`.
    pub fn mode_bits(self) -> u32 {
        match self {
            FileKind::Regular => 0o100000,
            FileKind::Directory => 0o040000,
            FileKind::Symlink => 0o120000,
            FileKind::CharDevice => 0o020000,
            FileKind::BlockDevice => 0o060000,
            FileKind::Fifo => 0o010000,
            FileKind::Socket => 0o140000,
        }
    }

    /// The `d_type` value used by `getdents64`.
    pub fn dirent_type(self) -> u8 {
        match self {
            FileKind::Fifo => 1,
            FileKind::CharDevice => 2,
            FileKind::Directory => 4,
            FileKind::BlockDevice => 6,
            FileKind::Regular => 8,
            FileKind::Symlink => 10,
            FileKind::Socket => 12,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub kind: FileKind,
    pub inode: u64,
    pub size: u64,
    /// Permission bits (the low 12 bits of `st_mode`).
    pub permissions: u32,
    pub uid: Uid,
    pub gid: Gid,
    /// How many directory entries refer to it.
    pub links: u32,
    /// Seconds since the Unix epoch.
    pub modified: u64,
    /// For device files, the device's number (Linux's `dev_t` encoding).
    pub rdev: u64,
}

impl Metadata {
    /// Metadata for something owned by root that isn't stored anywhere (a
    /// device, a pipe, a /proc file): one link, never modified.
    pub const fn virtual_file(kind: FileKind, inode: u64, permissions: u32) -> Self {
        Self { kind, inode, size: 0, permissions, uid: 0, gid: 0, links: 1, modified: 0, rdev: 0 }
    }
}

/// Who a new file belongs to and its permission bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attributes {
    pub permissions: u32,
    pub uid: Uid,
    pub gid: Gid,
}

impl Attributes {
    /// Root's, with these permissions (for files the kernel makes).
    pub const fn root(permissions: u32) -> Self {
        Self { permissions, uid: 0, gid: 0 }
    }
}

/// A file system object. Every operation has a default that refuses it, so
/// each kind of inode implements only what makes sense for it.
pub trait Inode: Send + Sync {
    fn metadata(&self) -> Metadata;

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Err(self.not_a_file())
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        Err(self.not_a_file())
    }

    fn truncate(&self, _len: u64) -> Result<()> {
        Err(self.not_a_file())
    }

    /// Reads from a stream (an inode that isn't `seekable`). With
    /// `nonblocking` (`O_NONBLOCK`), fails with `EAGAIN` instead of waiting.
    fn read_stream(&self, buf: &mut [u8], _nonblocking: bool) -> Result<usize> {
        self.read_at(0, buf)
    }

    /// Writes to a stream, like [`read_stream`](Self::read_stream).
    fn write_stream(&self, buf: &[u8], _nonblocking: bool) -> Result<usize> {
        self.write_at(0, buf)
    }

    /// Whether reads and writes use the file position (pipes and terminals
    /// don't).
    fn seekable(&self) -> bool {
        true
    }

    fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>> {
        Err(Errno::ENOTDIR)
    }

    /// Makes a new, empty entry. Symbolic links start with an empty target,
    /// written with `write_at`.
    fn create(&self, _name: &str, _kind: FileKind, _attributes: Attributes) -> Result<Arc<dyn Inode>> {
        Err(Errno::ENOTDIR)
    }

    /// Changes the permission bits (the low 12 of the mode).
    fn set_permissions(&self, _permissions: u32) -> Result<()> {
        Err(Errno::EPERM)
    }

    /// Changes the owner and group.
    fn set_owner(&self, _uid: Uid, _gid: Gid) -> Result<()> {
        Err(Errno::EPERM)
    }

    /// Changes the modification time (seconds since the Unix epoch).
    fn set_modified(&self, _time: u64) -> Result<()> {
        Ok(())
    }

    /// Adds an existing inode under another name (hard link or rename).
    fn link(&self, _name: &str, _inode: Arc<dyn Inode>) -> Result<()> {
        Err(Errno::ENOTDIR)
    }

    fn unlink(&self, _name: &str) -> Result<()> {
        Err(Errno::ENOTDIR)
    }

    /// Directory entries, not including `.` and `..`.
    fn entries(&self) -> Result<Vec<(String, Arc<dyn Inode>)>> {
        Err(Errno::ENOTDIR)
    }

    /// Where a symbolic link points.
    fn read_link(&self) -> Result<String> {
        Err(Errno::EINVAL)
    }

    fn ioctl(&self, _request: u64, _argument: u64) -> Result<u64> {
        Err(Errno::ENOTTY)
    }

    /// Which of `POLLIN`, `POLLOUT`, `POLLERR` and `POLLHUP` apply now, for
    /// an open file with these flags. Most things are always ready.
    fn poll(&self, _flags: u32) -> u16 {
        POLLIN | POLLOUT
    }

    /// Called when the last open file referring to this inode is closed.
    fn on_close(&self, _flags: u32) {}

    /// For devices that keep state for each open file (an input device
    /// gives every reader its own queue of events): the inode the new open
    /// file should refer to instead of this one. `None` means this one.
    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        None
    }

    /// Moves entry `old_name` of this directory to `new_name` in
    /// `new_parent`, replacing what's there. The default works for
    /// file systems that can link any inode into any directory.
    fn rename(&self, old_name: &str, new_parent: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        if self.filesystem() != new_parent.filesystem() {
            return Err(Errno::EXDEV);
        }
        let inode = self.lookup(old_name)?;
        if let Ok(existing) = new_parent.lookup(new_name) {
            if Arc::ptr_eq(&existing, &inode) {
                return Ok(());
            }
            if existing.metadata().kind == FileKind::Directory && !existing.entries()?.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
            new_parent.unlink(new_name)?;
        }
        new_parent.link(new_name, inode)?;
        self.unlink(old_name)
    }

    /// Space on the file system this belongs to: (block size, total blocks,
    /// free blocks, the `statfs` type magic number).
    fn space(&self) -> (u64, u64, u64, u64) {
        // The in-memory file systems share RAM.
        let memory = crate::memory::frames::stats();
        (4096, memory.managed_bytes / 4096, memory.free_bytes / 4096, 0x858458F6)
    }

    /// Which file system this belongs to: entries can only be renamed
    /// within one. The in-memory file systems are all 0.
    fn filesystem(&self) -> usize {
        0
    }

    /// For file systems that need to recognize their own inodes.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    fn not_a_file(&self) -> Errno {
        if self.metadata().kind == FileKind::Directory { Errno::EISDIR } else { Errno::EINVAL }
    }
}

/// Makes `path` absolute (relative to `cwd`) and removes `.`, `..` and
/// repeated slashes. Symbolic links are not involved.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();
    let full = if path.starts_with('/') { [path, ""] } else { [cwd, path] };
    for component in full.iter().flat_map(|part| part.split('/')) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            name => components.push(name),
        }
    }
    let mut result = String::new();
    for component in &components {
        result.push('/');
        result.push_str(component);
    }
    if result.is_empty() {
        result.push('/');
    }
    result
}

const MAX_SYMLINK_DEPTH: usize = 8;

/// Finds the inode at `path`, as the running process. A symbolic link as
/// the last component is followed only if `follow` is set. Every directory
/// passed through must be searchable (have execute permission).
pub fn lookup(cwd: &str, path: &str, follow: bool) -> Result<Arc<dyn Inode>> {
    lookup_as(&credentials::current(), cwd, path, follow)
}

/// [`lookup`] with someone else's credentials.
pub fn lookup_as(credentials: &Credentials, cwd: &str, path: &str, follow: bool) -> Result<Arc<dyn Inode>> {
    lookup_depth(credentials, cwd, path, follow, 0)
}

fn lookup_depth(
    credentials: &Credentials,
    cwd: &str,
    path: &str,
    follow: bool,
    depth: usize,
) -> Result<Arc<dyn Inode>> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    let full = normalize(cwd, path);
    let components: Vec<&str> = full.split('/').filter(|c| !c.is_empty()).collect();
    let mut inode = mount::try_root().ok_or(Errno::ENOENT)?;
    for (index, name) in components.iter().enumerate() {
        if !credentials.is_root() {
            check_access(credentials, &inode.metadata(), MAY_EXEC)?;
        }
        let next = mount::cross(inode.lookup(name)?);
        let last = index + 1 == components.len();
        if next.metadata().kind == FileKind::Symlink && (follow || !last) {
            if depth >= MAX_SYMLINK_DEPTH {
                return Err(Errno::ELOOP);
            }
            // Resolve the target relative to the link's directory, then
            // continue with the rest of the path from there.
            let directory = format_path(&components[..index]);
            let target = normalize(&directory, &next.read_link()?);
            let rest = format_path(&components[index + 1..]);
            let combined = if rest == "/" { target } else { target + &rest };
            return lookup_depth(credentials, "/", &combined, follow, depth + 1);
        }
        inode = next;
    }
    Ok(inode)
}

fn format_path(components: &[&str]) -> String {
    normalize("/", &components.join("/"))
}

/// Finds the directory that would contain `path`, and the final name.
pub fn lookup_parent(cwd: &str, path: &str) -> Result<(Arc<dyn Inode>, String)> {
    let full = normalize(cwd, path);
    let (directory, name) = full.rsplit_once('/').ok_or(Errno::EINVAL)?;
    if name.is_empty() {
        return Err(Errno::EEXIST);
    }
    let parent = lookup("/", if directory.is_empty() { "/" } else { directory }, true)?;
    if parent.metadata().kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    Ok((parent, name.to_string()))
}

// Access kinds for permission checks (the bits of each rwx triple).
pub const MAY_EXEC: u32 = 1;
pub const MAY_WRITE: u32 = 2;
pub const MAY_READ: u32 = 4;

/// Whether `credentials` grant every access in `want` (`MAY_*` bits) to a
/// file: the owner's bits apply to its owner, the group's to members of its
/// group, and the rest to everyone else. Root may read and write anything,
/// and execute anything with an execute bit (or search any directory).
pub fn check_access(credentials: &Credentials, metadata: &Metadata, want: u32) -> Result<()> {
    let permissions = metadata.permissions;
    if credentials.is_root() {
        let executable = metadata.kind == FileKind::Directory || permissions & 0o111 != 0;
        return if want & MAY_EXEC == 0 || executable { Ok(()) } else { Err(Errno::EACCES) };
    }
    let granted = if credentials.euid == metadata.uid {
        permissions >> 6
    } else if credentials.in_group(metadata.gid) {
        permissions >> 3
    } else {
        permissions
    } & 7;
    if granted & want == want { Ok(()) } else { Err(Errno::EACCES) }
}

/// [`check_access`] for the running process.
pub fn check(metadata: &Metadata, want: u32) -> Result<()> {
    check_access(&credentials::current(), metadata, want)
}

/// Whether the running process may add or remove entries in `directory`.
pub fn check_writable_directory(directory: &Arc<dyn Inode>) -> Result<()> {
    check(&directory.metadata(), MAY_WRITE | MAY_EXEC)
}

/// The sticky bit: in a directory with it (like /tmp), only an entry's
/// owner, the directory's owner, or root may delete or rename it.
pub const STICKY: u32 = 0o1000;
pub const SET_GROUP_ID: u32 = 0o2000;
pub const SET_USER_ID: u32 = 0o4000;

/// Whether the running process may remove (or rename away) entry `name` of
/// `directory`.
pub fn check_removable(directory: &Arc<dyn Inode>, entry: &Arc<dyn Inode>) -> Result<()> {
    let credentials = credentials::current();
    let directory_metadata = directory.metadata();
    check_access(&credentials, &directory_metadata, MAY_WRITE | MAY_EXEC)?;
    if directory_metadata.permissions & STICKY != 0
        && !credentials.is_root()
        && credentials.euid != directory_metadata.uid
        && credentials.euid != entry.metadata().uid
    {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// The attributes of a file the running process creates in `parent`: owned
/// by its effective user, with `permissions` less its umask. The group is
/// the process's, or the directory's if that has the setgid bit (which new
/// subdirectories inherit), as on Linux.
pub fn new_file_attributes(parent: &Arc<dyn Inode>, kind: FileKind, permissions: u32) -> Attributes {
    let credentials = credentials::current();
    let umask = crate::task::current_process().map_or(0o022, |process| process.umask());
    let parent = parent.metadata();
    let mut permissions = permissions & 0o7777 & !umask;
    let gid = if parent.permissions & SET_GROUP_ID != 0 {
        if kind == FileKind::Directory {
            permissions |= SET_GROUP_ID;
        }
        parent.gid
    } else {
        credentials.egid
    };
    Attributes { permissions, uid: credentials.euid, gid }
}

// Open flags (Linux values).
pub const O_ACCMODE: u32 = 0o3;
pub const O_RDONLY: u32 = 0o0;
pub const O_WRONLY: u32 = 0o1;
pub const O_RDWR: u32 = 0o2;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
pub const O_CLOEXEC: u32 = 0o2000000;

/// An open file: an inode, a position, and how it was opened.
pub struct OpenFile {
    pub inode: Arc<dyn Inode>,
    pub path: String,
    position: SpinLock<u64>,
    flags: SpinLock<u32>,
}

impl OpenFile {
    pub fn new(inode: Arc<dyn Inode>, path: String, flags: u32) -> Arc<Self> {
        Arc::new(Self { inode, path, position: SpinLock::new(0), flags: SpinLock::new(flags & !O_CLOEXEC) })
    }

    pub fn flags(&self) -> u32 {
        *self.flags.lock()
    }

    pub fn set_status_flags(&self, flags: u32) {
        let mut current = self.flags.lock();
        *current = (*current & !(O_APPEND | O_NONBLOCK)) | (flags & (O_APPEND | O_NONBLOCK));
    }

    pub fn readable(&self) -> bool {
        self.flags() & O_ACCMODE != O_WRONLY
    }

    pub fn writable(&self) -> bool {
        self.flags() & O_ACCMODE != O_RDONLY
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if !self.readable() {
            return Err(Errno::EBADF);
        }
        if !self.inode.seekable() {
            return self.inode.read_stream(buf, self.flags() & O_NONBLOCK != 0);
        }
        // The position lock isn't held across the read: reads may block,
        // and a spinlock must never be held while sleeping.
        let position = *self.position.lock();
        let count = self.inode.read_at(position, buf)?;
        *self.position.lock() = position + count as u64;
        Ok(count)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        if !self.writable() {
            return Err(Errno::EBADF);
        }
        if !self.inode.seekable() {
            return self.inode.write_stream(buf, self.flags() & O_NONBLOCK != 0);
        }
        let position = if self.flags() & O_APPEND != 0 { self.inode.metadata().size } else { *self.position.lock() };
        let count = self.inode.write_at(position, buf)?;
        *self.position.lock() = position + count as u64;
        Ok(count)
    }

    pub fn seek(&self, offset: i64, whence: u32) -> Result<u64> {
        if !self.inode.seekable() {
            return Err(Errno::ESPIPE);
        }
        let mut position = self.position.lock();
        let base = match whence {
            0 => 0,
            1 => *position as i64,
            2 => self.inode.metadata().size as i64,
            _ => return Err(Errno::EINVAL),
        };
        let target = base.checked_add(offset).filter(|&target| target >= 0).ok_or(Errno::EINVAL)?;
        *position = target as u64;
        Ok(*position)
    }

    pub fn position(&self) -> u64 {
        *self.position.lock()
    }

    pub fn set_position(&self, position: u64) {
        *self.position.lock() = position;
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        self.inode.on_close(*self.flags.get_mut());
    }
}

pub const MAX_FILES: usize = 256;

#[derive(Clone)]
struct Descriptor {
    file: Arc<OpenFile>,
    close_on_exec: bool,
}

/// A process's file descriptors.
#[derive(Clone, Default)]
pub struct FileTable {
    slots: Vec<Option<Descriptor>>,
}

impl FileTable {
    /// Stores `file` in the lowest free descriptor at or above `minimum`.
    pub fn insert(&mut self, file: Arc<OpenFile>, close_on_exec: bool, minimum: usize) -> Result<usize> {
        let fd =
            (minimum..MAX_FILES).find(|&fd| self.slots.get(fd).is_none_or(Option::is_none)).ok_or(Errno::EMFILE)?;
        self.place(fd, file, close_on_exec);
        Ok(fd)
    }

    /// Stores `file` at `fd`, closing what was there.
    pub fn place(&mut self, fd: usize, file: Arc<OpenFile>, close_on_exec: bool) {
        if self.slots.len() <= fd {
            self.slots.resize(fd + 1, None);
        }
        self.slots[fd] = Some(Descriptor { file, close_on_exec });
    }

    pub fn get(&self, fd: usize) -> Result<Arc<OpenFile>> {
        self.slots.get(fd).and_then(Option::as_ref).map(|d| d.file.clone()).ok_or(Errno::EBADF)
    }

    pub fn close(&mut self, fd: usize) -> Result<()> {
        self.slots.get_mut(fd).and_then(Option::take).map(|_| ()).ok_or(Errno::EBADF)
    }

    pub fn close_on_exec(&self, fd: usize) -> Result<bool> {
        self.slots.get(fd).and_then(Option::as_ref).map(|d| d.close_on_exec).ok_or(Errno::EBADF)
    }

    pub fn set_close_on_exec(&mut self, fd: usize, value: bool) -> Result<()> {
        let descriptor = self.slots.get_mut(fd).and_then(Option::as_mut).ok_or(Errno::EBADF)?;
        descriptor.close_on_exec = value;
        Ok(())
    }

    /// Closes the descriptors marked close-on-exec.
    pub fn exec(&mut self) {
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|d| d.close_on_exec) {
                *slot = None;
            }
        }
    }

    pub fn close_all(&mut self) {
        self.slots.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn paths_are_normalized() {
            assert_eq!(normalize("/home", "notes.txt"), "/home/notes.txt");
            assert_eq!(normalize("/a/b", "../c/./d/"), "/a/c/d");
            assert_eq!(normalize("/a", "/x//y/.."), "/x");
            assert_eq!(normalize("/", ".."), "/");
        }

        fn files_can_be_created_written_and_read() {
            let tmp = lookup("/", "/tmp", true).unwrap();
            let file = tmp.create("vfs-test", FileKind::Regular, Attributes::root(0o644)).unwrap();
            let open = OpenFile::new(file, "/tmp/vfs-test".into(), O_RDWR);
            assert_eq!(open.write(b"hello, world"), Ok(12));
            open.seek(7, 0).unwrap();
            let mut buf = [0; 16];
            assert_eq!(open.read(&mut buf), Ok(5));
            assert_eq!(&buf[..5], b"world");
            assert_eq!(lookup("/tmp", "vfs-test", true).unwrap().metadata().size, 12);
            tmp.unlink("vfs-test").unwrap();
            assert!(lookup("/", "/tmp/vfs-test", true).is_err());
        }

        fn symlinks_are_followed() {
            let tmp = lookup("/", "/tmp", true).unwrap();
            let target = tmp.create("link-target", FileKind::Directory, Attributes::root(0o755)).unwrap();
            target.create("inside", FileKind::Regular, Attributes::root(0o644)).unwrap();
            let link = tmp.create("link", FileKind::Symlink, Attributes::root(0o777)).unwrap();
            link.write_at(0, b"link-target").unwrap();
            assert_eq!(lookup("/", "/tmp/link/inside", true).unwrap().metadata().kind, FileKind::Regular);
            assert_eq!(lookup("/", "/tmp/link", false).unwrap().metadata().kind, FileKind::Symlink);
            assert_eq!(lookup("/", "/tmp/link", true).unwrap().metadata().kind, FileKind::Directory);
        }

        fn file_tables_hand_out_the_lowest_descriptor() {
            let null = lookup("/", "/dev/null", true).unwrap();
            let mut table = FileTable::default();
            let file = OpenFile::new(null, "/dev/null".into(), O_RDWR);
            assert_eq!(table.insert(file.clone(), false, 0), Ok(0));
            assert_eq!(table.insert(file.clone(), true, 0), Ok(1));
            table.close(0).unwrap();
            assert_eq!(table.insert(file.clone(), false, 0), Ok(0));
            table.exec();
            assert_eq!(table.get(1).err(), Some(Errno::EBADF));
        }
    }
}
