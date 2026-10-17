//! The system calls the desktop and its programs need beyond libeli's:
//! memory mapping, ioctls, polling, timers, and sockets that carry file
//! descriptors.

use alloc::vec::Vec;

use eli::sys::{Result, call};

const POLL: usize = 7;
const MMAP: usize = 9;
const MUNMAP: usize = 11;
const IOCTL: usize = 16;
const SOCKET: usize = 41;
const SENDMSG: usize = 46;
const RECVMSG: usize = 47;
const CONNECT: usize = 42;
const BIND: usize = 49;
const LISTEN: usize = 50;
const FTRUNCATE: usize = 77;
const EPOLL_WAIT: usize = 232;
const EPOLL_CTL: usize = 233;
const OPENAT: usize = 257;
const TIMERFD_CREATE: usize = 283;
const TIMERFD_SETTIME: usize = 286;
const ACCEPT4: usize = 288;
const EPOLL_CREATE1: usize = 291;
const MEMFD_CREATE: usize = 319;
const UNLINKAT: usize = 263;
const MKDIRAT: usize = 258;
const AT_FDCWD: usize = -100isize as usize;

pub const O_RDONLY: usize = 0;
pub const O_RDWR: usize = 2;
pub const O_NONBLOCK: usize = 0o4000;
pub const O_CLOEXEC: usize = 0o2000000;

pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 1;

pub const EPOLLIN: u32 = 0x1;

/// A path as a NUL-terminated string.
fn c_string(text: &str) -> Vec<u8> {
    let mut bytes = Vec::from(text.as_bytes());
    bytes.push(0);
    bytes
}

pub fn open(path: &str, flags: usize) -> Result<i32> {
    let path = c_string(path);
    // SAFETY: a valid NUL-terminated path.
    unsafe { call(OPENAT, [AT_FDCWD, path.as_ptr() as usize, flags | O_CLOEXEC, 0, 0, 0]) }.map(|fd| fd as i32)
}

pub fn unlink(path: &str) {
    let path = c_string(path);
    // SAFETY: as for `open`.
    let _ = unsafe { call(UNLINKAT, [AT_FDCWD, path.as_ptr() as usize, 0, 0, 0, 0]) };
}

pub fn make_directory(path: &str, mode: usize) -> Result<()> {
    let path = c_string(path);
    // SAFETY: as for `open`.
    unsafe { call(MKDIRAT, [AT_FDCWD, path.as_ptr() as usize, mode, 0, 0, 0]) }.map(|_| ())
}

/// # Safety
/// `argument` must be what the request expects.
pub unsafe fn ioctl(fd: i32, request: usize, argument: usize) -> Result<usize> {
    unsafe { call(IOCTL, [fd as usize, request, argument, 0, 0, 0]) }
}

/// Maps `len` bytes of a file, shared.
pub fn map(fd: i32, len: usize, writable: bool) -> Result<*mut u8> {
    let protection = if writable { PROT_READ | PROT_WRITE } else { PROT_READ };
    // SAFETY: a fresh mapping chosen by the kernel touches nothing of ours.
    unsafe { call(MMAP, [0, len, protection, MAP_SHARED, fd as usize, 0]) }.map(|address| address as *mut u8)
}

/// # Safety
/// Nothing may use the mapping afterwards.
pub unsafe fn unmap(address: *mut u8, len: usize) {
    let _ = unsafe { call(MUNMAP, [address as usize, len, 0, 0, 0, 0]) };
}

/// A memfd holding `data`, for handing to clients.
pub fn memory_file(name: &str, data: &[u8]) -> Result<i32> {
    let fd = shared_memory(name, data.len())?;
    eli::io::write_all(fd, data)?;
    Ok(fd)
}

/// A memfd of `len` zero bytes, to map and share.
pub fn shared_memory(name: &str, len: usize) -> Result<i32> {
    const MFD_CLOEXEC: usize = 1;
    let name = c_string(name);
    // SAFETY: a valid name.
    let fd = unsafe { call(MEMFD_CREATE, [name.as_ptr() as usize, MFD_CLOEXEC, 0, 0, 0, 0]) }? as i32;
    // SAFETY: plain calls on our own descriptor.
    if let Err(error) = unsafe { call(FTRUNCATE, [fd as usize, len, 0, 0, 0, 0]) } {
        let _ = eli::io::close(fd);
        return Err(error);
    }
    Ok(fd)
}

/// A listening Unix socket at `path`.
pub fn listen_unix(path: &str) -> Result<i32> {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_NONBLOCK: usize = 0o4000;
    const SOCK_CLOEXEC: usize = 0o2000000;
    // SAFETY: plain socket creation.
    let fd = unsafe { call(SOCKET, [AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, 0, 0, 0]) }? as i32;
    let mut address = [0u8; 110];
    address[..2].copy_from_slice(&(AF_UNIX as u16).to_le_bytes());
    address[2..2 + path.len()].copy_from_slice(path.as_bytes());
    // SAFETY: a valid sockaddr_un of the length given.
    unsafe {
        call(BIND, [fd as usize, address.as_ptr() as usize, 2 + path.len() + 1, 0, 0, 0])?;
        call(LISTEN, [fd as usize, 64, 0, 0, 0, 0])?;
    }
    Ok(fd)
}

/// A connection to the Unix socket at `path`.
pub fn connect_unix(path: &str) -> Result<i32> {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_CLOEXEC: usize = 0o2000000;
    if path.len() >= 108 {
        return Err(eli::Error(36));
    }
    // SAFETY: plain socket creation.
    let fd = unsafe { call(SOCKET, [AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, 0, 0, 0]) }? as i32;
    let mut address = [0u8; 110];
    address[..2].copy_from_slice(&(AF_UNIX as u16).to_le_bytes());
    address[2..2 + path.len()].copy_from_slice(path.as_bytes());
    // SAFETY: a valid sockaddr_un of the length given.
    if let Err(error) = unsafe { call(CONNECT, [fd as usize, address.as_ptr() as usize, 2 + path.len() + 1, 0, 0, 0]) }
    {
        let _ = eli::io::close(fd);
        return Err(error);
    }
    Ok(fd)
}

pub fn accept(fd: i32) -> Result<i32> {
    const SOCK_NONBLOCK: usize = 0o4000;
    const SOCK_CLOEXEC: usize = 0o2000000;
    // SAFETY: no address wanted.
    unsafe { call(ACCEPT4, [fd as usize, 0, 0, SOCK_NONBLOCK | SOCK_CLOEXEC, 0, 0]) }.map(|fd| fd as i32)
}

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_DONTWAIT: usize = 0x40;
const MSG_NOSIGNAL: usize = 0x4000;
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

#[repr(C)]
struct MessageHeader {
    name: usize,
    name_length: u32,
    iov: *const [usize; 2],
    iov_length: usize,
    control: *mut u8,
    control_length: usize,
    flags: i32,
}

/// Sends `data` with `fds` attached; returns how much was sent.
pub fn send_with_fds(socket: i32, data: &[u8], fds: &[i32]) -> Result<usize> {
    let vector = [data.as_ptr() as usize, data.len()];
    let mut control = Vec::new();
    if !fds.is_empty() {
        let length = 16 + 4 * fds.len();
        control.extend_from_slice(&(length as u64).to_le_bytes());
        control.extend_from_slice(&SOL_SOCKET.to_le_bytes());
        control.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
        fds.iter().for_each(|fd| control.extend_from_slice(&fd.to_le_bytes()));
        control.resize(length.next_multiple_of(8), 0);
    }
    let header = MessageHeader {
        name: 0,
        name_length: 0,
        iov: &vector,
        iov_length: 1,
        control: if control.is_empty() { core::ptr::null_mut() } else { control.as_mut_ptr() },
        control_length: control.len(),
        flags: 0,
    };
    // SAFETY: the header and what it points at live through the call.
    unsafe { call(SENDMSG, [socket as usize, &header as *const _ as usize, MSG_DONTWAIT | MSG_NOSIGNAL, 0, 0, 0]) }
}

/// Receives into `buffer`, appending any descriptors that came to `fds`.
pub fn receive_with_fds(socket: i32, buffer: &mut [u8], fds: &mut Vec<i32>) -> Result<usize> {
    let vector = [buffer.as_mut_ptr() as usize, buffer.len()];
    let mut control = [0u8; 512];
    let mut header = MessageHeader {
        name: 0,
        name_length: 0,
        iov: &vector,
        iov_length: 1,
        control: control.as_mut_ptr(),
        control_length: control.len(),
        flags: 0,
    };
    // SAFETY: as for `send_with_fds`.
    let count = unsafe {
        call(RECVMSG, [socket as usize, &mut header as *mut _ as usize, MSG_DONTWAIT | MSG_CMSG_CLOEXEC, 0, 0, 0])
    }?;
    let mut offset = 0;
    while offset + 16 <= header.control_length {
        let length = u64::from_le_bytes(control[offset..offset + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(control[offset + 8..offset + 12].try_into().unwrap());
        let kind = i32::from_le_bytes(control[offset + 12..offset + 16].try_into().unwrap());
        if length < 16 {
            break;
        }
        if level == SOL_SOCKET && kind == SCM_RIGHTS {
            for fd in control[offset + 16..(offset + length).min(control.len())].as_chunks::<4>().0 {
                fds.push(i32::from_le_bytes(*fd));
            }
        }
        offset += length.next_multiple_of(8);
    }
    Ok(count)
}

pub struct Epoll(i32);

impl Epoll {
    pub fn new() -> Result<Self> {
        // SAFETY: no pointers.
        unsafe { call(EPOLL_CREATE1, [O_CLOEXEC, 0, 0, 0, 0, 0]) }.map(|fd| Self(fd as i32))
    }

    /// Watches `fd` for input, tagging its events with `token`.
    pub fn add(&self, fd: i32, token: u64) -> Result<()> {
        let mut event = [0u8; 12];
        event[..4].copy_from_slice(&EPOLLIN.to_le_bytes());
        event[4..].copy_from_slice(&token.to_le_bytes());
        // SAFETY: a valid epoll_event.
        unsafe { call(EPOLL_CTL, [self.0 as usize, 1, fd as usize, event.as_ptr() as usize, 0, 0]) }.map(|_| ())
    }

    pub fn remove(&self, fd: i32) {
        // SAFETY: no event needed for deletion.
        let _ = unsafe { call(EPOLL_CTL, [self.0 as usize, 2, fd as usize, 0, 0, 0]) };
    }

    /// Waits up to `timeout_ms` (-1: forever); returns (events, token)s.
    pub fn wait(&self, timeout_ms: i32) -> Vec<(u32, u64)> {
        let mut events = [0u8; 12 * 64];
        // SAFETY: room for 64 packed epoll_events.
        let count = unsafe {
            call(EPOLL_WAIT, [self.0 as usize, events.as_mut_ptr() as usize, 64, timeout_ms as isize as usize, 0, 0])
        }
        .unwrap_or(0);
        events[..count * 12]
            .as_chunks::<12>()
            .0
            .iter()
            .map(|event| {
                (u32::from_le_bytes(event[..4].try_into().unwrap()), u64::from_le_bytes(event[4..].try_into().unwrap()))
            })
            .collect()
    }
}

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

/// Waits until one of `fds` (descriptor, events wanted) is ready or
/// `timeout_ms` passes (-1: forever); returns what happened to each.
pub fn poll(fds: &[(i32, i16)], timeout_ms: i32) -> Vec<i16> {
    let mut entries: Vec<[i32; 2]> = fds.iter().map(|&(fd, events)| [fd, i32::from(events as u16)]).collect();
    // SAFETY: an array of `struct pollfd` (int fd; short events, revents).
    let result =
        unsafe { call(POLL, [entries.as_mut_ptr() as usize, entries.len(), timeout_ms as isize as usize, 0, 0, 0]) };
    if result.is_err() {
        return alloc::vec![0; fds.len()];
    }
    entries.iter().map(|entry| (entry[1] >> 16) as i16).collect()
}

/// A repeating timer, readable on each tick.
pub fn interval_timer(period_ms: u64) -> Result<i32> {
    const CLOCK_MONOTONIC: usize = 1;
    // SAFETY: no pointers.
    let fd = unsafe { call(TIMERFD_CREATE, [CLOCK_MONOTONIC, O_NONBLOCK | O_CLOEXEC, 0, 0, 0, 0]) }? as i32;
    let nanos = period_ms * 1_000_000;
    let spec = [nanos / 1_000_000_000, nanos % 1_000_000_000, nanos / 1_000_000_000, nanos % 1_000_000_000];
    // SAFETY: a valid itimerspec.
    unsafe { call(TIMERFD_SETTIME, [fd as usize, 0, spec.as_ptr() as usize, 0, 0, 0]) }?;
    Ok(fd)
}
