//! Files and directories.

use alloc::string::String;
use alloc::vec::Vec;

use crate::io;
use crate::sys::{self, AT_FDCWD, Error, Result};

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_TRUNC: usize = 0o1000;
const O_APPEND: usize = 0o2000;
const O_DIRECTORY: usize = 0o200000;
const O_CLOEXEC: usize = 0o2000000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_REMOVEDIR: usize = 0x200;

/// A path as a NUL-terminated C string.
fn c_path(path: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(path.len() + 1);
    bytes.extend_from_slice(path.as_bytes());
    bytes.push(0);
    bytes
}

/// An open file, closed when dropped.
pub struct File {
    fd: i32,
}

impl File {
    fn open_with(path: &str, flags: usize) -> Result<Self> {
        let path = c_path(path);
        // SAFETY: the path is NUL-terminated.
        let fd = unsafe { sys::call(sys::OPENAT, [AT_FDCWD, path.as_ptr() as usize, flags | O_CLOEXEC, 0o644, 0, 0]) }?;
        Ok(Self { fd: fd as i32 })
    }

    pub fn open(path: &str) -> Result<Self> {
        Self::open_with(path, O_RDONLY)
    }

    /// Opens for writing, creating the file or emptying it.
    pub fn create(path: &str) -> Result<Self> {
        Self::open_with(path, O_WRONLY | O_CREAT | O_TRUNC)
    }

    /// Opens for writing at the end, creating the file if needed.
    pub fn append(path: &str) -> Result<Self> {
        Self::open_with(path, O_WRONLY | O_CREAT | O_APPEND)
    }

    /// Opens for reading and writing, without creating or emptying it (for
    /// a disk, say).
    pub fn open_read_write(path: &str) -> Result<Self> {
        Self::open_with(path, O_RDWR)
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Reads at a position, without moving the file's own (`pread`).
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        // SAFETY: the buffer is valid for its length.
        unsafe {
            sys::call(
                sys::PREAD64,
                [self.fd as usize, buffer.as_mut_ptr() as usize, buffer.len(), offset as usize, 0, 0],
            )
        }
    }

    /// Writes at a position (`pwrite`).
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize> {
        // SAFETY: the buffer is valid for its length.
        unsafe {
            sys::call(sys::PWRITE64, [self.fd as usize, data.as_ptr() as usize, data.len(), offset as usize, 0, 0])
        }
    }

    /// Reads exactly `buffer.len()` bytes at `offset`, or fails.
    pub fn read_exact_at(&self, mut offset: u64, mut buffer: &mut [u8]) -> Result<()> {
        while !buffer.is_empty() {
            match self.read_at(offset, buffer)? {
                0 => return Err(Error(5)),
                count => {
                    offset += count as u64;
                    buffer = &mut buffer[count..];
                }
            }
        }
        Ok(())
    }

    /// Writes all of `data` at `offset`.
    pub fn write_all_at(&self, mut offset: u64, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            match self.write_at(offset, data)? {
                0 => return Err(Error(28)),
                count => {
                    offset += count as u64;
                    data = &data[count..];
                }
            }
        }
        Ok(())
    }

    /// The size of a disk or partition, in bytes (`BLKGETSIZE64`).
    pub fn device_size(&self) -> Result<u64> {
        const BLKGETSIZE64: usize = 0x8008_1272;
        let mut size = 0u64;
        // SAFETY: room for the size.
        unsafe { sys::call(sys::IOCTL, [self.fd as usize, BLKGETSIZE64, &raw mut size as usize, 0, 0, 0]) }?;
        Ok(size)
    }

    /// Asks the kernel to reread a disk's partition table (`BLKRRPART`).
    pub fn reread_partitions(&self) -> Result<()> {
        const BLKRRPART: usize = 0x125F;
        // SAFETY: takes no pointers.
        unsafe { sys::call(sys::IOCTL, [self.fd as usize, BLKRRPART, 0, 0, 0, 0]) }.map(|_| ())
    }

    pub fn read(&self, buffer: &mut [u8]) -> Result<usize> {
        io::read(self.fd, buffer)
    }

    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        io::write_all(self.fd, data)
    }

    pub fn read_to_end(&self) -> Result<Vec<u8>> {
        io::read_to_end(self.fd)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = io::close(self.fd);
    }
}

pub fn read(path: &str) -> Result<Vec<u8>> {
    File::open(path)?.read_to_end()
}

pub fn read_to_string(path: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&read(path)?).into_owned())
}

pub fn write(path: &str, data: &[u8]) -> Result<()> {
    File::create(path)?.write_all(data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Pipe,
    Socket,
    Other,
}

impl FileType {
    fn from_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o100000 => FileType::File,
            0o040000 => FileType::Directory,
            0o120000 => FileType::Symlink,
            0o020000 => FileType::CharDevice,
            0o060000 => FileType::BlockDevice,
            0o010000 => FileType::Pipe,
            0o140000 => FileType::Socket,
            _ => FileType::Other,
        }
    }

    fn from_dirent(kind: u8) -> Self {
        match kind {
            8 => FileType::File,
            4 => FileType::Directory,
            10 => FileType::Symlink,
            2 => FileType::CharDevice,
            6 => FileType::BlockDevice,
            1 => FileType::Pipe,
            12 => FileType::Socket,
            _ => FileType::Other,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub kind: FileType,
    pub size: u64,
    /// The file type and permission bits (`st_mode`).
    pub mode: u32,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
}

impl Metadata {
    /// The permission bits, including setuid, setgid and sticky.
    pub fn permissions(&self) -> u32 {
        self.mode & 0o7777
    }
}

fn stat(path: &str, flags: usize) -> Result<Metadata> {
    let path = c_path(path);
    let mut buffer = [0u64; 18];
    // SAFETY: the buffer is the size of `struct stat`.
    unsafe {
        sys::call(sys::NEWFSTATAT, [AT_FDCWD, path.as_ptr() as usize, buffer.as_mut_ptr() as usize, flags, 0, 0])
    }?;
    // struct stat: dev, ino, nlink, then mode, uid, gid as 32-bit values.
    let mode = buffer[3] as u32;
    Ok(Metadata {
        kind: FileType::from_mode(mode),
        size: buffer[6],
        mode,
        inode: buffer[1],
        uid: (buffer[3] >> 32) as u32,
        gid: buffer[4] as u32,
        links: buffer[2],
        modified: buffer[11] as i64,
    })
}

pub fn metadata(path: &str) -> Result<Metadata> {
    stat(path, 0)
}

/// Like [`metadata`], but describes a symbolic link itself.
pub fn symlink_metadata(path: &str) -> Result<Metadata> {
    stat(path, AT_SYMLINK_NOFOLLOW)
}

pub fn exists(path: &str) -> bool {
    metadata(path).is_ok()
}

pub struct DirEntry {
    pub name: String,
    pub kind: FileType,
}

/// A directory's entries (without `.` and `..`), sorted by name.
pub fn read_dir(path: &str) -> Result<Vec<DirEntry>> {
    let directory = File::open_with(path, O_RDONLY | O_DIRECTORY)?;
    let mut entries = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        // SAFETY: the buffer is valid for its length.
        let len = unsafe {
            sys::call(sys::GETDENTS64, [directory.fd as usize, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0])
        }?;
        if len == 0 {
            break;
        }
        let mut offset = 0;
        while offset < len {
            let record_len = u16::from_le_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
            let kind = buffer[offset + 18];
            let name = &buffer[offset + 19..offset + record_len];
            let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
            let name = String::from_utf8_lossy(name).into_owned();
            if name != "." && name != ".." {
                entries.push(DirEntry { name, kind: FileType::from_dirent(kind) });
            }
            offset += record_len;
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

pub fn create_dir(path: &str) -> Result<()> {
    let path = c_path(path);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::MKDIRAT, [AT_FDCWD, path.as_ptr() as usize, 0o755, 0, 0, 0]) }.map(|_| ())
}

pub fn remove_file(path: &str) -> Result<()> {
    let path = c_path(path);
    // SAFETY: as above.
    unsafe { sys::call(sys::UNLINKAT, [AT_FDCWD, path.as_ptr() as usize, 0, 0, 0, 0]) }.map(|_| ())
}

pub fn remove_dir(path: &str) -> Result<()> {
    let path = c_path(path);
    // SAFETY: as above.
    unsafe { sys::call(sys::UNLINKAT, [AT_FDCWD, path.as_ptr() as usize, AT_REMOVEDIR, 0, 0, 0]) }.map(|_| ())
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    let (from, to) = (c_path(from), c_path(to));
    // SAFETY: both paths are NUL-terminated.
    unsafe { sys::call(sys::RENAMEAT, [AT_FDCWD, from.as_ptr() as usize, AT_FDCWD, to.as_ptr() as usize, 0, 0]) }
        .map(|_| ())
}

/// Creates a symbolic link at `link` pointing to `target`.
pub fn symlink(target: &str, link: &str) -> Result<()> {
    let (target, link) = (c_path(target), c_path(link));
    // SAFETY: both paths are NUL-terminated.
    unsafe { sys::call(sys::SYMLINKAT, [target.as_ptr() as usize, AT_FDCWD, link.as_ptr() as usize, 0, 0, 0]) }
        .map(|_| ())
}

pub fn read_link(path: &str) -> Result<String> {
    let path = c_path(path);
    let mut buffer = [0u8; 4096];
    // SAFETY: the path is NUL-terminated and the buffer valid.
    let len = unsafe {
        sys::call(sys::READLINKAT, [AT_FDCWD, path.as_ptr() as usize, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0])
    }?;
    Ok(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

pub fn current_dir() -> Result<String> {
    let mut buffer = [0u8; 4096];
    // SAFETY: the buffer is valid for its length.
    let len = unsafe { sys::call(sys::GETCWD, [buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0, 0]) }?;
    Ok(String::from_utf8_lossy(&buffer[..len.saturating_sub(1)]).into_owned())
}

pub fn set_current_dir(path: &str) -> Result<()> {
    let path = c_path(path);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::CHDIR, [path.as_ptr() as usize, 0, 0, 0, 0, 0]) }.map(|_| ())
}

/// Copies a file's contents.
pub fn copy(from: &str, to: &str) -> Result<u64> {
    let data = read(from)?;
    if metadata(from)?.kind == FileType::Directory {
        return Err(Error::IS_DIRECTORY);
    }
    write(to, &data)?;
    Ok(data.len() as u64)
}

/// Changes a file's permission bits (`chmod`).
pub fn set_permissions(path: &str, permissions: u32) -> Result<()> {
    let path = c_path(path);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::FCHMODAT, [AT_FDCWD, path.as_ptr() as usize, permissions as usize, 0, 0, 0]) }.map(|_| ())
}

/// Changes a file's owner and group (`None` leaves one alone). Symbolic
/// links themselves are changed, not what they point to (`lchown`).
pub fn set_owner(path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
    let path = c_path(path);
    let id = |value: Option<u32>| value.map_or(u32::MAX as usize, |id| id as usize);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::FCHOWNAT, [AT_FDCWD, path.as_ptr() as usize, id(uid), id(gid), AT_SYMLINK_NOFOLLOW, 0]) }
        .map(|_| ())
}

/// Sets a file's modification time (seconds since the Unix epoch), or to
/// now with `None`.
pub fn set_modified(path: &str, time: Option<u64>) -> Result<()> {
    let path = c_path(path);
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    let times: [i64; 4] = match time {
        Some(time) => [0, UTIME_OMIT, time as i64, 0],
        None => [0, UTIME_OMIT, 0, UTIME_NOW],
    };
    // SAFETY: the path is NUL-terminated and `times` is two timespecs.
    unsafe { sys::call(sys::UTIMENSAT, [AT_FDCWD, path.as_ptr() as usize, times.as_ptr() as usize, 0, 0, 0]) }
        .map(|_| ())
}

/// Makes `link` another name for the file at `target` (a hard link).
pub fn hard_link(target: &str, link: &str) -> Result<()> {
    let (target, link) = (c_path(target), c_path(link));
    // SAFETY: both paths are NUL-terminated.
    unsafe { sys::call(sys::LINKAT, [AT_FDCWD, target.as_ptr() as usize, AT_FDCWD, link.as_ptr() as usize, 0, 0]) }
        .map(|_| ())
}

/// Creates a directory with the given permissions (less the umask).
pub fn create_dir_with(path: &str, permissions: u32) -> Result<()> {
    let path = c_path(path);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::MKDIRAT, [AT_FDCWD, path.as_ptr() as usize, permissions as usize, 0, 0, 0]) }.map(|_| ())
}

/// Sets the umask (permission bits new files don't get), returning the old
/// one.
pub fn set_umask(mask: u32) -> u32 {
    // SAFETY: takes no pointers.
    unsafe { sys::syscall(sys::UMASK, [mask as usize, 0, 0, 0, 0, 0]) as u32 }
}

/// Whether the real user could access `path` the way `mode` asks (4 read,
/// 2 write, 1 execute, 0 just exists).
pub fn accessible(path: &str, mode: u32) -> bool {
    let path = c_path(path);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::FACCESSAT, [AT_FDCWD, path.as_ptr() as usize, mode as usize, 0, 0, 0]) }.is_ok()
}

/// Copies a directory tree, keeping permissions (and, for root, owners).
pub fn copy_tree(from: &str, to: &str) -> Result<()> {
    let metadata = symlink_metadata(from)?;
    match metadata.kind {
        FileType::Directory => {
            match create_dir(to) {
                Ok(()) | Err(Error::EXISTS) => {}
                Err(error) => return Err(error),
            }
            for entry in read_dir(from)? {
                copy_tree(&alloc::format!("{from}/{}", entry.name), &alloc::format!("{to}/{}", entry.name))?;
            }
        }
        FileType::Symlink => symlink(&read_link(from)?, to)?,
        _ => {
            copy(from, to)?;
        }
    }
    if crate::users::euid() == 0 {
        set_owner(to, Some(metadata.uid), Some(metadata.gid))?;
    }
    if metadata.kind != FileType::Symlink {
        set_permissions(to, metadata.permissions())?;
    }
    Ok(())
}

/// Writes everything waiting to the disks.
pub fn sync() {
    // SAFETY: takes no arguments.
    unsafe { sys::syscall(sys::SYNC, [0; 6]) };
}

pub const MOUNT_READ_ONLY: usize = 1;
pub const MOUNT_REMOUNT: usize = 32;
pub const MOUNT_BIND: usize = 4096;

/// Mounts `source` (a device like `/dev/sda2`, or a name for virtual file
/// systems) of type `kind` (`ext2`, `vfat`, `proc`...; empty to detect) at
/// `target`.
pub fn mount(source: &str, target: &str, kind: &str, flags: usize) -> Result<()> {
    let (source, target, kind) = (c_path(source), c_path(target), c_path(kind));
    // SAFETY: the strings are NUL-terminated.
    unsafe {
        sys::call(sys::MOUNT, [source.as_ptr() as usize, target.as_ptr() as usize, kind.as_ptr() as usize, flags, 0, 0])
    }
    .map(|_| ())
}

pub fn unmount(target: &str) -> Result<()> {
    let target = c_path(target);
    // SAFETY: the path is NUL-terminated.
    unsafe { sys::call(sys::UMOUNT2, [target.as_ptr() as usize, 0, 0, 0, 0, 0]) }.map(|_| ())
}

/// Space on the file system holding `path`: (block size, total blocks,
/// free blocks).
pub fn space(path: &str) -> Result<(u64, u64, u64)> {
    let path = c_path(path);
    let mut fields = [0u64; 15];
    // SAFETY: room for a `struct statfs`.
    unsafe { sys::call(sys::STATFS, [path.as_ptr() as usize, fields.as_mut_ptr() as usize, 0, 0, 0, 0]) }?;
    Ok((fields[1], fields[2], fields[4]))
}

/// One line of /proc/mounts: (device, mount point, type, options).
pub struct MountEntry {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub options: String,
}

pub fn mounts() -> Vec<MountEntry> {
    read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(MountEntry {
                source: fields.next()?.into(),
                target: fields.next()?.into(),
                kind: fields.next()?.into(),
                options: fields.next().unwrap_or("").into(),
            })
        })
        .collect()
}
