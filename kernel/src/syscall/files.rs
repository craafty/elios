//! File system calls.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{
    self, FileKind, Inode, MAY_EXEC, MAY_READ, MAY_WRITE, O_ACCMODE, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL,
    O_NOFOLLOW, O_RDONLY, O_TRUNC, O_WRONLY, OpenFile, SET_GROUP_ID, SET_USER_ID, pipe,
};
use crate::process::credentials::{self, Gid, Uid};
use crate::process::{self, user};

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EACCESS: u64 = 0x200;
const AT_SYMLINK_FOLLOW: u64 = 0x400;
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

pub fn open_at(dirfd: u64, path: u64, flags: u64, mode: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let flags = flags as u32;
    let base = base_directory(dirfd)?;
    let full = fs::normalize(&base, &path);

    let inode = match fs::lookup(&base, &path, flags & O_NOFOLLOW == 0) {
        Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(Errno::EEXIST),
        Ok(inode) => {
            check_open(&inode, flags)?;
            inode
        }
        // A file the caller creates can be opened however it asks, even if
        // its permissions say otherwise (as on Linux).
        Err(Errno::ENOENT) if flags & O_CREAT != 0 => {
            let (parent, name) = fs::lookup_parent(&base, &path)?;
            fs::check_writable_directory(&parent)?;
            let attributes = fs::new_file_attributes(&parent, FileKind::Regular, mode as u32);
            parent.create(&name, FileKind::Regular, attributes)?
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

/// Whether the running process may open an existing file the way `flags`
/// ask. Opening a setuid or setgid file for writing as someone other than
/// root takes those bits away, so nobody can turn a privileged program into
/// their own (Linux does this on the first write).
fn check_open(inode: &Arc<dyn Inode>, flags: u32) -> Result<()> {
    let metadata = inode.metadata();
    let want = match flags & O_ACCMODE {
        O_RDONLY if flags & O_TRUNC != 0 => MAY_READ | MAY_WRITE,
        O_RDONLY => MAY_READ,
        O_WRONLY => MAY_WRITE,
        _ => MAY_READ | MAY_WRITE,
    };
    let credentials = credentials::current();
    fs::check_access(&credentials, &metadata, want)?;
    if want & MAY_WRITE != 0
        && !credentials.is_root()
        && metadata.kind == FileKind::Regular
        && metadata.permissions & (SET_USER_ID | SET_GROUP_ID) != 0
    {
        inode.set_permissions(metadata.permissions & !(SET_USER_ID | SET_GROUP_ID))?;
    }
    Ok(())
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

const FIONBIO: u64 = 0x5421;
const FIONCLEX: u64 = 0x5450;
const FIOCLEX: u64 = 0x5451;

/// `ioctl`: a few requests apply to any open file; the rest go to the inode.
pub fn ioctl(fd: u64, request: u64, argument: u64) -> Result<u64> {
    let file = file(fd)?;
    match request {
        FIONBIO => {
            let on: i32 = user::read_value(argument)?;
            let flags = file.flags() & !fs::O_NONBLOCK;
            file.set_status_flags(if on != 0 { flags | fs::O_NONBLOCK } else { flags });
            Ok(0)
        }
        FIOCLEX | FIONCLEX => {
            process::current().files.lock().set_close_on_exec(fd as i32 as usize, request == FIOCLEX)?;
            Ok(0)
        }
        _ => file.inode.ioctl(request, argument),
    }
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
        links: u64::from(metadata.links),
        mode: metadata.kind.mode_bits() | metadata.permissions,
        uid: metadata.uid,
        gid: metadata.gid,
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

/// `access` and `faccessat`: whether the caller could read, write or run
/// (`mode` bits 4, 2, 1; 0 just checks the file exists). Checked as the real
/// user, so a setuid program can ask what its caller could do, unless
/// `AT_EACCESS` asks for the effective one.
pub fn access_at(dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let current = credentials::current();
    let credentials = if flags & AT_EACCESS != 0 { (*current).clone() } else { current.as_real() };
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let inode = fs::lookup_as(&credentials, &base_directory(dirfd)?, &path, follow)?;
    let want = mode as u32 & (MAY_READ | MAY_WRITE | MAY_EXEC);
    if want != 0 {
        fs::check_access(&credentials, &inode.metadata(), want)?;
    }
    Ok(0)
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
    let metadata = inode.metadata();
    if metadata.kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    fs::check(&metadata, MAY_EXEC)?;
    *process.cwd.lock() = fs::normalize(&cwd, &path);
    Ok(0)
}

pub fn fchdir(fd: u64) -> Result<u64> {
    let file = file(fd)?;
    let metadata = file.inode.metadata();
    if metadata.kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }
    fs::check(&metadata, MAY_EXEC)?;
    *process::current().cwd.lock() = file.path.clone();
    Ok(0)
}

pub fn mkdir_at(dirfd: u64, path: u64, mode: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let (parent, name) = fs::lookup_parent(&base_directory(dirfd)?, &path)?;
    if parent.lookup(&name).is_ok() {
        return Err(Errno::EEXIST);
    }
    fs::check_writable_directory(&parent)?;
    let attributes = fs::new_file_attributes(&parent, FileKind::Directory, mode as u32);
    parent.create(&name, FileKind::Directory, attributes).map(|_| 0)
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
    fs::check_removable(&parent, &inode)?;
    parent.unlink(&name).map(|_| 0)
}

pub fn rename_at(old_dirfd: u64, old_path: u64, new_dirfd: u64, new_path: u64) -> Result<u64> {
    let old_path = user::read_string(old_path)?;
    let new_path = user::read_string(new_path)?;
    let (old_parent, old_name) = fs::lookup_parent(&base_directory(old_dirfd)?, &old_path)?;
    let (new_parent, new_name) = fs::lookup_parent(&base_directory(new_dirfd)?, &new_path)?;
    fs::check_removable(&old_parent, &old_parent.lookup(&old_name)?)?;
    match new_parent.lookup(&new_name) {
        Ok(existing) => fs::check_removable(&new_parent, &existing)?,
        Err(_) => fs::check_writable_directory(&new_parent)?,
    }
    old_parent.rename(&old_name, &new_parent, &new_name).map(|_| 0)
}

pub fn symlink_at(target: u64, dirfd: u64, path: u64) -> Result<u64> {
    let target = user::read_string(target)?;
    let path = user::read_string(path)?;
    let (parent, name) = fs::lookup_parent(&base_directory(dirfd)?, &path)?;
    fs::check_writable_directory(&parent)?;
    let attributes = fs::new_file_attributes(&parent, FileKind::Symlink, 0o777);
    let link = parent.create(&name, FileKind::Symlink, fs::Attributes { permissions: 0o777, ..attributes })?;
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
    let inode = fs::lookup(&cwd, &path, true)?;
    fs::check(&inode.metadata(), MAY_WRITE)?;
    inode.truncate(len).map(|_| 0)
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

/// The inode a `*at` call on (`dirfd`, `path`) means. An empty path with
/// `AT_EMPTY_PATH`, or a null one (for `utimensat`), means the open file
/// `dirfd` itself. Symbolic links are followed by default, unless
/// `follow_by_default` is false (for `linkat`, where `AT_SYMLINK_FOLLOW`
/// asks for it).
fn inode_at(dirfd: u64, path: u64, flags: u64, follow_by_default: bool) -> Result<Arc<dyn Inode>> {
    if path == 0 {
        return Ok(file(dirfd)?.inode.clone());
    }
    let path = user::read_string(path)?;
    if path.is_empty() {
        return if flags & AT_EMPTY_PATH != 0 { Ok(file(dirfd)?.inode.clone()) } else { Err(Errno::ENOENT) };
    }
    let follow = if follow_by_default { flags & AT_SYMLINK_NOFOLLOW == 0 } else { flags & AT_SYMLINK_FOLLOW != 0 };
    fs::lookup(&base_directory(dirfd)?, &path, follow)
}

/// Changes a file's permission bits: only its owner (or root) may. Someone
/// not in the file's group can't make it setgid.
fn change_permissions(inode: &Arc<dyn Inode>, permissions: u32) -> Result<u64> {
    let credentials = credentials::current();
    let metadata = inode.metadata();
    if metadata.kind == FileKind::Symlink {
        return Err(Errno::EOPNOTSUPP);
    }
    if !credentials.is_root() && credentials.euid != metadata.uid {
        return Err(Errno::EPERM);
    }
    let mut permissions = permissions & 0o7777;
    if !credentials.is_root() && !credentials.in_group(metadata.gid) {
        permissions &= !SET_GROUP_ID;
    }
    inode.set_permissions(permissions).map(|_| 0)
}

/// `chmod`, `fchmodat` and `fchmodat2`.
pub fn chmod_at(dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<u64> {
    change_permissions(&inode_at(dirfd, path, flags, true)?, mode as u32)
}

pub fn fchmod(fd: u64, mode: u64) -> Result<u64> {
    change_permissions(&file(fd)?.inode, mode as u32)
}

/// Changes a file's owner and group (`None` leaves one alone). Only root
/// may give a file away; its owner may change its group to another of their
/// own. A changed regular file loses its setuid and setgid bits, so giving
/// a program to someone never hands out their privileges.
fn change_owner(inode: &Arc<dyn Inode>, uid: Option<Uid>, gid: Option<Gid>) -> Result<u64> {
    let credentials = credentials::current();
    let metadata = inode.metadata();
    if !credentials.is_root() {
        if uid.is_some_and(|uid| uid != metadata.uid) {
            return Err(Errno::EPERM);
        }
        if gid.is_some_and(|gid| gid != metadata.gid)
            && (credentials.euid != metadata.uid || !gid.is_some_and(|gid| credentials.in_group(gid)))
        {
            return Err(Errno::EPERM);
        }
    }
    if uid.is_none() && gid.is_none() {
        return Ok(0);
    }
    inode.set_owner(uid.unwrap_or(metadata.uid), gid.unwrap_or(metadata.gid))?;
    if metadata.kind != FileKind::Directory && metadata.permissions & (SET_USER_ID | SET_GROUP_ID) != 0 {
        let mut permissions = metadata.permissions & !SET_USER_ID;
        // Setgid without group execute isn't about privileges (it marks
        // mandatory locking), so it stays.
        if permissions & 0o010 != 0 {
            permissions &= !SET_GROUP_ID;
        }
        let _ = inode.set_permissions(permissions);
    }
    Ok(0)
}

fn optional_id(value: u64) -> Option<u32> {
    let id = value as u32;
    (id != u32::MAX).then_some(id)
}

/// `chown`, `lchown` and `fchownat`.
pub fn chown_at(dirfd: u64, path: u64, uid: u64, gid: u64, flags: u64) -> Result<u64> {
    change_owner(&inode_at(dirfd, path, flags, true)?, optional_id(uid), optional_id(gid))
}

pub fn fchown(fd: u64, uid: u64, gid: u64) -> Result<u64> {
    change_owner(&file(fd)?.inode, optional_id(uid), optional_id(gid))
}

const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

/// Sets a file's modification time: to now, for its owner or anyone who can
/// write it; to a chosen time, only for its owner. (eliOS keeps one time
/// per file, so access times are ignored.)
fn set_modified(inode: &Arc<dyn Inode>, time: Option<u64>) -> Result<u64> {
    let credentials = credentials::current();
    let metadata = inode.metadata();
    let owner = credentials.is_root() || credentials.euid == metadata.uid;
    match time {
        Some(_) if !owner => return Err(Errno::EPERM),
        None if !owner => fs::check_access(&credentials, &metadata, MAY_WRITE)?,
        _ => {}
    }
    inode.set_modified(time.unwrap_or_else(crate::time::unix_time)).map(|_| 0)
}

/// `utimensat(dirfd, path, times, flags)`: `times` is two `timespec`s
/// (access, modification), either of which may be `UTIME_NOW` or
/// `UTIME_OMIT`; null means both now. A null path means `dirfd` itself.
pub fn utimensat(dirfd: u64, path: u64, times: u64, flags: u64) -> Result<u64> {
    let inode = inode_at(dirfd, path, flags, true)?;
    if times == 0 {
        return set_modified(&inode, None);
    }
    let [_, _, seconds, nanoseconds]: [i64; 4] = user::read_value(times)?;
    match nanoseconds {
        UTIME_OMIT => Ok(0),
        UTIME_NOW => set_modified(&inode, None),
        _ => set_modified(&inode, Some(seconds.max(0) as u64)),
    }
}

/// `utimes(path, times)`: two `timeval`s, or null for now.
pub fn utimes(path: u64, times: u64) -> Result<u64> {
    let inode = inode_at(AT_FDCWD as u64, path, 0, true)?;
    if times == 0 {
        return set_modified(&inode, None);
    }
    let [_, _, seconds, _]: [i64; 4] = user::read_value(times)?;
    set_modified(&inode, Some(seconds.max(0) as u64))
}

/// `link` and `linkat`: another name for an existing file, in the same file
/// system. Directories can't be linked.
pub fn link_at(old_dirfd: u64, old_path: u64, new_dirfd: u64, new_path: u64, flags: u64) -> Result<u64> {
    let inode = inode_at(old_dirfd, old_path, flags, false)?;
    if inode.metadata().kind == FileKind::Directory {
        return Err(Errno::EPERM);
    }
    let new_path = user::read_string(new_path)?;
    let (parent, name) = fs::lookup_parent(&base_directory(new_dirfd)?, &new_path)?;
    if parent.lookup(&name).is_ok() {
        return Err(Errno::EEXIST);
    }
    fs::check_writable_directory(&parent)?;
    if parent.filesystem() != inode.filesystem() {
        return Err(Errno::EXDEV);
    }
    parent.link(&name, inode).map(|_| 0)
}
