//! File system calls.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{
    self, FileKind, Inode, O_ACCMODE, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_NOFOLLOW, O_RDONLY, O_TRUNC, OpenFile,
    pipe,
};
use crate::process::{self, user};

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

/// File descriptors are C `int`s: only the low 32 bits of the register
/// count (as on Linux).
fn descriptor(fd: u64) -> Result<usize> {
    usize::try_from(fd as i32).map_err(|_| Errno::EBADF)
}

fn file(fd: u64) -> Result<Arc<OpenFile>> {
    process::current().files.lock().get(descriptor(fd)?)
}

/// The directory a `*at` call's relative paths start from.
fn base_directory(dirfd: u64) -> Result<String> {
    if i64::from(dirfd as i32) == AT_FDCWD {
        return Ok(process::current().cwd.lock().clone());
    }
    let directory = file(dirfd)?;
    if directory.inode.metadata().kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    Ok(directory.path.clone())
}

pub fn read(fd: u64, buffer: u64, count: u64) -> Result<u64> {
    let file = file(fd)?;
    let buffer = user::buffer_mut(buffer, count as usize)?;
    file.read(buffer).map(|n| n as u64)
}

pub fn write(fd: u64, buffer: u64, count: u64) -> Result<u64> {
    let file = file(fd)?;
    let buffer = user::buffer(buffer, count as usize)?;
    file.write(buffer).map(|n| n as u64)
}

pub fn pread(fd: u64, buffer: u64, count: u64, offset: u64) -> Result<u64> {
    let file = file(fd)?;
    let buffer = user::buffer_mut(buffer, count as usize)?;
    file.inode.read_at(offset, buffer).map(|n| n as u64)
}

pub fn pwrite(fd: u64, buffer: u64, count: u64, offset: u64) -> Result<u64> {
    let file = file(fd)?;
    let buffer = user::buffer(buffer, count as usize)?;
    file.inode.write_at(offset, buffer).map(|n| n as u64)
}

/// `readv` and `writev`: one read or write per buffer in the list.
pub fn vectored(fd: u64, vectors: u64, count: u64, writing: bool) -> Result<u64> {
    let file = file(fd)?;
    let mut total = 0;
    for index in 0..count.min(1024) {
        let [base, len]: [u64; 2] = user::read_value(vectors + index * 16)?;
        if len == 0 {
            continue;
        }
        let done = if writing {
            file.write(user::buffer(base, len as usize)?)?
        } else {
            file.read(user::buffer_mut(base, len as usize)?)?
        };
        total += done as u64;
        if (done as u64) < len {
            break;
        }
    }
    Ok(total)
}

pub fn open_at(dirfd: u64, path: u64, flags: u64, _mode: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let flags = flags as u32;
    let base = base_directory(dirfd)?;
    let full = fs::normalize(&base, &path);

    let inode = match fs::lookup(&base, &path, flags & O_NOFOLLOW == 0) {
        Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(Errno::EEXIST),
        Ok(inode) => inode,
        Err(Errno::ENOENT) if flags & O_CREAT != 0 => {
            let (parent, name) = fs::lookup_parent(&base, &path)?;
            parent.create(&name, FileKind::Regular)?
        }
        Err(error) => return Err(error),
    };

    let kind = inode.metadata().kind;
    if flags & O_DIRECTORY != 0 && kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    if kind == FileKind::Directory && flags & O_ACCMODE != O_RDONLY {
        return Err(Errno::EISDIR);
    }
    if kind == FileKind::Symlink {
        return Err(Errno::ELOOP);
    }
    if flags & O_TRUNC != 0 && flags & O_ACCMODE != O_RDONLY && kind == FileKind::Regular {
        inode.truncate(0)?;
    }
    let open = OpenFile::new(inode, full, flags);
    let fd = process::current().files.lock().insert(open, flags & O_CLOEXEC != 0, 0)?;
    Ok(fd as u64)
}

pub fn close(fd: u64) -> Result<u64> {
    // Keep a reference so the file is really closed (which can wake other
    // threads, for pipes) after the table's lock is released.
    let process = process::current();
    let file = {
        let mut files = process.files.lock();
        let file = files.get(descriptor(fd)?)?;
        files.close(descriptor(fd)?)?;
        file
    };
    drop(file);
    Ok(0)
}

pub fn seek(fd: u64, offset: u64, whence: u64) -> Result<u64> {
    file(fd)?.seek(offset as i64, whence as u32)
}

pub fn ioctl(fd: u64, request: u64, argument: u64) -> Result<u64> {
    file(fd)?.inode.ioctl(request, argument)
}

/// Linux's `struct stat` on x86_64.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Stat {
    device: u64,
    inode: u64,
    links: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    _pad: u32,
    rdev: u64,
    size: i64,
    block_size: i64,
    blocks: i64,
    access_time: [i64; 2],
    modify_time: [i64; 2],
    change_time: [i64; 2],
    _reserved: [i64; 3],
}

fn write_stat(inode: &Arc<dyn Inode>, address: u64) -> Result<u64> {
    let metadata = inode.metadata();
    let time = [metadata.modified as i64, 0];
    let stat = Stat {
        device: 1,
        inode: metadata.inode,
        links: 1,
        mode: metadata.kind.mode_bits() | metadata.permissions,
        size: metadata.size as i64,
        block_size: 4096,
        blocks: metadata.size.div_ceil(512) as i64,
        access_time: time,
        modify_time: time,
        change_time: time,
        ..Default::default()
    };
    user::write_value(address, &stat)?;
    Ok(0)
}

pub fn stat_at(dirfd: u64, path: u64, stat: u64, flags: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return write_stat(&file(dirfd)?.inode, stat);
    }
    let base = base_directory(dirfd)?;
    let inode = fs::lookup(&base, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    write_stat(&inode, stat)
}

pub fn fstat(fd: u64, stat: u64) -> Result<u64> {
    write_stat(&file(fd)?.inode, stat)
}

pub fn access_at(dirfd: u64, path: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    fs::lookup(&base_directory(dirfd)?, &path, true).map(|_| 0)
}

pub fn pipe(fds: u64, flags: u64) -> Result<u64> {
    let (reader, writer) = pipe::create();
    let close_on_exec = flags as u32 & O_CLOEXEC != 0;
    let process = process::current();
    let mut files = process.files.lock();
    let read_fd = files.insert(reader, close_on_exec, 0)?;
    let write_fd = match files.insert(writer, close_on_exec, 0) {
        Ok(fd) => fd,
        Err(error) => {
            let _ = files.close(read_fd);
            return Err(error);
        }
    };
    drop(files);
    user::write_value(fds, &[read_fd as i32, write_fd as i32])?;
    Ok(0)
}

pub fn dup(fd: u64) -> Result<u64> {
    let process = process::current();
    let mut files = process.files.lock();
    let file = files.get(descriptor(fd)?)?;
    files.insert(file, false, 0).map(|fd| fd as u64)
}

pub fn dup3(old: u64, new: u64, flags: u64) -> Result<u64> {
    let new_fd = descriptor(new)?;
    if new_fd >= fs::MAX_FILES {
        return Err(Errno::EBADF);
    }
    let process = process::current();
    let mut files = process.files.lock();
    let file = files.get(descriptor(old)?)?;
    if old == new {
        return Ok(new);
    }
    let replaced = files.get(new_fd).ok();
    files.place(new_fd, file, flags as u32 & O_CLOEXEC != 0);
    drop(files);
    drop(replaced);
    Ok(new)
}

pub fn dup2(old: u64, new: u64) -> Result<u64> {
    if old == new {
        return file(old).map(|_| new);
    }
    dup3(old, new, 0)
}

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const FD_CLOEXEC: u64 = 1;

pub fn fcntl(fd: u64, command: u64, argument: u64) -> Result<u64> {
    let process = process::current();
    let mut files = process.files.lock();
    match command {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let file = files.get(descriptor(fd)?)?;
            files.insert(file, command == F_DUPFD_CLOEXEC, argument as usize).map(|fd| fd as u64)
        }
        F_GETFD => files.close_on_exec(descriptor(fd)?).map(u64::from),
        F_SETFD => files.set_close_on_exec(descriptor(fd)?, argument & FD_CLOEXEC != 0).map(|_| 0),
        F_GETFL => files.get(descriptor(fd)?).map(|file| u64::from(file.flags())),
        F_SETFL => {
            files.get(descriptor(fd)?)?.set_status_flags(argument as u32);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub fn getcwd(buffer: u64, size: u64) -> Result<u64> {
    let mut cwd = process::current().cwd.lock().clone().into_bytes();
    cwd.push(0);
    if cwd.len() as u64 > size {
        return Err(Errno::ERANGE);
    }
    user::write_bytes(buffer, &cwd)?;
    Ok(cwd.len() as u64)
}

pub fn chdir(path: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let process = process::current();
    let cwd = process.cwd.lock().clone();
    let inode = fs::lookup(&cwd, &path, true)?;
    if inode.metadata().kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    *process.cwd.lock() = fs::normalize(&cwd, &path);
    Ok(0)
}

pub fn fchdir(fd: u64) -> Result<u64> {
    let file = file(fd)?;
    if file.inode.metadata().kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    *process::current().cwd.lock() = file.path.clone();
    Ok(0)
}

pub fn mkdir_at(dirfd: u64, path: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let (parent, name) = fs::lookup_parent(&base_directory(dirfd)?, &path)?;
    parent.create(&name, FileKind::Directory).map(|_| 0)
}

pub fn unlink_at(dirfd: u64, path: u64, flags: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let base = base_directory(dirfd)?;
    let (parent, name) = fs::lookup_parent(&base, &path)?;
    let inode = parent.lookup(&name)?;
    let is_directory = inode.metadata().kind == FileKind::Directory;
    if flags & AT_REMOVEDIR != 0 {
        if !is_directory {
            return Err(Errno::ENOTDIR);
        }
        if !inode.entries()?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
    } else if is_directory {
        return Err(Errno::EISDIR);
    }
    parent.unlink(&name).map(|_| 0)
}

pub fn rename_at(old_dirfd: u64, old_path: u64, new_dirfd: u64, new_path: u64) -> Result<u64> {
    let old_path = user::read_string(old_path)?;
    let new_path = user::read_string(new_path)?;
    let (old_parent, old_name) = fs::lookup_parent(&base_directory(old_dirfd)?, &old_path)?;
    let (new_parent, new_name) = fs::lookup_parent(&base_directory(new_dirfd)?, &new_path)?;
    old_parent.rename(&old_name, &new_parent, &new_name).map(|_| 0)
}

pub fn symlink_at(target: u64, dirfd: u64, path: u64) -> Result<u64> {
    let target = user::read_string(target)?;
    let path = user::read_string(path)?;
    let (parent, name) = fs::lookup_parent(&base_directory(dirfd)?, &path)?;
    let link = parent.create(&name, FileKind::Symlink)?;
    link.write_at(0, target.as_bytes())?;
    Ok(0)
}

pub fn readlink_at(dirfd: u64, path: u64, buffer: u64, size: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let inode = fs::lookup(&base_directory(dirfd)?, &path, false)?;
    let target = inode.read_link()?;
    let count = target.len().min(size as usize);
    user::write_bytes(buffer, &target.as_bytes()[..count])?;
    Ok(count as u64)
}

pub fn truncate(path: u64, len: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let cwd = process::current().cwd.lock().clone();
    fs::lookup(&cwd, &path, true)?.truncate(len).map(|_| 0)
}

pub fn ftruncate(fd: u64, len: u64) -> Result<u64> {
    let file = file(fd)?;
    if !file.writable() {
        return Err(Errno::EINVAL);
    }
    file.inode.truncate(len).map(|_| 0)
}

/// `struct statfs` for an inode's file system.
fn write_statfs(inode: &Arc<dyn Inode>, address: u64) -> Result<u64> {
    let (block_size, blocks, free, magic) = inode.space();
    // type, bsize, blocks, bfree, bavail, files, ffree, fsid (2 x i32),
    // namelen, frsize, flags, spare[4].
    let fields: [u64; 15] = [magic, block_size, blocks, free, free, 0, 0, 0, 255, block_size, 0, 0, 0, 0, 0];
    user::write_value(address, &fields)?;
    Ok(0)
}

pub fn statfs(path: u64, address: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let cwd = process::current().cwd.lock().clone();
    write_statfs(&fs::lookup(&cwd, &path, true)?, address)
}

pub fn fstatfs(fd: u64, address: u64) -> Result<u64> {
    write_statfs(&file(fd)?.inode, address)
}

/// `sendfile`: copies from one file to another inside the kernel.
pub fn sendfile(out_fd: u64, in_fd: u64, offset: u64, count: u64) -> Result<u64> {
    let output = file(out_fd)?;
    let input = file(in_fd)?;
    let mut position = if offset == 0 { None } else { Some(user::read_value::<u64>(offset)?) };
    let mut buffer = alloc::vec![0u8; 64 * 1024];
    let mut total = 0u64;
    while total < count {
        let chunk = ((count - total) as usize).min(buffer.len());
        let read = match &mut position {
            Some(position) => {
                let read = input.inode.read_at(*position, &mut buffer[..chunk])?;
                *position += read as u64;
                read
            }
            None => input.read(&mut buffer[..chunk])?,
        };
        if read == 0 {
            break;
        }
        let mut written = 0;
        while written < read {
            written += output.write(&buffer[written..read])?;
        }
        total += read as u64;
    }
    if let Some(position) = position {
        user::write_value(offset, &position)?;
    }
    Ok(total)
}

/// `getdents64`: directory entries, packed as `struct linux_dirent64`. The
/// file position counts entries, starting with `.` and `..`.
pub fn get_directory_entries(fd: u64, buffer: u64, size: u64) -> Result<u64> {
    let file = file(fd)?;
    let metadata = file.inode.metadata();
    if metadata.kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    let mut entries: Vec<(String, u64, FileKind)> =
        alloc::vec![(".".into(), metadata.inode, FileKind::Directory), ("..".into(), 0, FileKind::Directory)];
    for (name, inode) in file.inode.entries()? {
        let metadata = inode.metadata();
        entries.push((name, metadata.inode, metadata.kind));
    }

    let mut output = Vec::new();
    let mut position = file.position() as usize;
    while let Some((name, inode, kind)) = entries.get(position) {
        // Header (19 bytes), name, NUL, padded to 8.
        let record_len = (19 + name.len() + 1).next_multiple_of(8);
        if output.len() + record_len > size as usize {
            break;
        }
        output.extend_from_slice(&inode.max(&1).to_le_bytes());
        output.extend_from_slice(&((position + 1) as i64).to_le_bytes());
        output.extend_from_slice(&(record_len as u16).to_le_bytes());
        output.push(kind.dirent_type());
        output.extend_from_slice(name.as_bytes());
        output.resize(output.len() + record_len - 19 - name.len(), 0);
        position += 1;
    }
    if output.is_empty() && position < entries.len() {
        return Err(Errno::EINVAL);
    }
    user::write_bytes(buffer, &output)?;
    file.set_position(position as u64);
    Ok(output.len() as u64)
}
