//! Files and directories.

use alloc::string::String;
use alloc::vec::Vec;

use crate::io;
use crate::sys::{self, AT_FDCWD, Error, Result};

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
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

    pub fn fd(&self) -> i32 {
        self.fd
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
    Pipe,
    Other,
}

impl FileType {
    fn from_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o100000 => FileType::File,
            0o040000 => FileType::Directory,
            0o120000 => FileType::Symlink,
            0o020000 => FileType::CharDevice,
            0o010000 => FileType::Pipe,
            _ => FileType::Other,
        }
    }

    fn from_dirent(kind: u8) -> Self {
        match kind {
            8 => FileType::File,
            4 => FileType::Directory,
            10 => FileType::Symlink,
            2 => FileType::CharDevice,
            1 => FileType::Pipe,
            _ => FileType::Other,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub kind: FileType,
    pub size: u64,
    pub mode: u32,
    pub inode: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
}

fn stat(path: &str, flags: usize) -> Result<Metadata> {
    let path = c_path(path);
    let mut buffer = [0u64; 18];
    // SAFETY: the buffer is the size of `struct stat`.
    unsafe {
        sys::call(sys::NEWFSTATAT, [AT_FDCWD, path.as_ptr() as usize, buffer.as_mut_ptr() as usize, flags, 0, 0])
    }?;
    let mode = buffer[3] as u32;
    Ok(Metadata {
        kind: FileType::from_mode(mode),
        size: buffer[6],
        mode,
        inode: buffer[1],
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
