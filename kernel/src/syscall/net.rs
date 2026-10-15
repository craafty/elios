//! Socket system calls: the Linux API over eliOS's TCP/IP stack, and Unix
//! domain sockets (`net::unix`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{Inode, O_NONBLOCK, O_RDWR, OpenFile};
use crate::net::socket::{self, AF_INET, Endpoint, SocketFile};
use crate::net::unix::{self, AF_UNIX};
use crate::net::{self, Ipv4};
use crate::process::{self, user};

const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITALL: u64 = 0x100;
const MSG_NOSIGNAL: u64 = 0x4000;

/// The Unix socket `fd` refers to, if it is one.
fn unix_socket(fd: u64) -> Result<Option<(Arc<OpenFile>, Arc<unix::Socket>)>> {
    let file = process::current().files.lock().get(fd as i32 as usize)?;
    let socket = file.inode.as_any().and_then(|any| any.downcast_ref::<unix::SocketFile>()).map(|f| f.socket.clone());
    Ok(socket.map(|socket| (file, socket)))
}

/// Reads a `struct sockaddr_un`.
fn read_unix_address(address: u64, length: u64) -> Result<unix::Address> {
    if !(2..=110).contains(&length) {
        return Err(Errno::EINVAL);
    }
    unix::Address::parse(&user::read_bytes(address, length as usize)?)
}

/// Writes a socket address (as bytes) to a buffer whose size is at
/// `length` (and becomes the full size).
fn write_address_bytes(address: u64, length: u64, bytes: &[u8]) -> Result<()> {
    if address == 0 || length == 0 {
        return Ok(());
    }
    let room: u32 = user::read_value(length)?;
    user::write_bytes(address, &bytes[..bytes.len().min(room as usize)])?;
    user::write_value(length, &(bytes.len() as u32))
}

/// A send on a Unix socket, raising `SIGPIPE` for a broken connection as
/// Linux does (unless `MSG_NOSIGNAL`).
fn unix_send(
    file: &OpenFile,
    socket: &unix::Socket,
    data: &[u8],
    files: Vec<Arc<OpenFile>>,
    destination: Option<&unix::Address>,
    flags: u64,
) -> Result<u64> {
    match socket.send(data, files, destination, nonblocking(file, flags)) {
        Err(Errno::EPIPE) if flags & MSG_NOSIGNAL == 0 => {
            process::send_signal_to_self(process::SIGPIPE);
            Err(Errno::EPIPE)
        }
        result => result.map(|count| count as u64),
    }
}

/// Runs `f` with the socket `fd` refers to.
fn with_socket<R>(fd: u64, f: impl FnOnce(&OpenFile, &SocketFile) -> Result<R>) -> Result<R> {
    let file = process::current().files.lock().get(fd as i32 as usize)?;
    let socket = file.inode.as_any().and_then(|any| any.downcast_ref::<SocketFile>()).ok_or(Errno::ENOTSOCK)?;
    f(&file, socket)
}

fn nonblocking(file: &OpenFile, flags: u64) -> bool {
    file.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0
}

/// Installs a socket as a new file descriptor.
fn install(socket: Arc<dyn Inode>, flags: u64) -> Result<u64> {
    let status = O_RDWR | if flags & SOCK_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(socket, "socket:".into(), status);
    process::current().files.lock().insert(file, flags & SOCK_CLOEXEC != 0, 0).map(|fd| fd as u64)
}

/// Reads a `struct sockaddr_in`.
fn read_address(address: u64, length: u64) -> Result<Endpoint> {
    if length < 8 {
        return Err(Errno::EINVAL);
    }
    let bytes: [u8; 8] = user::read_value(address)?;
    if u16::from_le_bytes([bytes[0], bytes[1]]) != AF_INET as u16 {
        return Err(Errno::EAFNOSUPPORT);
    }
    Ok(Endpoint {
        address: Ipv4([bytes[4], bytes[5], bytes[6], bytes[7]]),
        port: u16::from_be_bytes([bytes[2], bytes[3]]),
    })
}

fn address_bytes(endpoint: Endpoint) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&endpoint.address.0);
    bytes
}

/// Writes a `struct sockaddr_in` to a buffer whose size is at `length`
/// (and becomes the full size), as `accept` and `getsockname` do.
fn write_address(address: u64, length: u64, endpoint: Endpoint) -> Result<()> {
    if address == 0 || length == 0 {
        return Ok(());
    }
    let room: u32 = user::read_value(length)?;
    let bytes = address_bytes(endpoint);
    user::write_bytes(address, &bytes[..bytes.len().min(room as usize)])?;
    user::write_value(length, &(bytes.len() as u32))
}

pub fn socket(domain: u64, kind: u64, protocol: u64) -> Result<u64> {
    if domain == AF_UNIX {
        if protocol != 0 {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let socket = unix::Socket::new(unix::Kind::from_type(kind & 0xF)?);
        return install(unix::SocketFile::new(socket), kind);
    }
    let socket = socket::create(domain, kind & 0xF, protocol)?;
    install(socket, kind)
}

/// `socketpair`: two connected Unix sockets.
pub fn socketpair(domain: u64, kind: u64, protocol: u64, fds: u64) -> Result<u64> {
    if domain != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let (one, two) = unix::Socket::pair(unix::Kind::from_type(kind & 0xF)?);
    let first = install(unix::SocketFile::new(one), kind)?;
    let second = match install(unix::SocketFile::new(two), kind) {
        Ok(fd) => fd,
        Err(error) => {
            let _ = process::current().files.lock().close(first as usize);
            return Err(error);
        }
    };
    user::write_value(fds, &[first as i32, second as i32])?;
    Ok(0)
}

pub fn bind(fd: u64, address: u64, length: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        socket.bind(read_unix_address(address, length)?)?;
        return Ok(0);
    }
    let endpoint = read_address(address, length)?;
    with_socket(fd, |_, socket| socket::bind(socket, endpoint)).map(|_| 0)
}

pub fn connect(fd: u64, address: u64, length: u64) -> Result<u64> {
    if let Some((file, socket)) = unix_socket(fd)? {
        socket.connect(&read_unix_address(address, length)?, nonblocking(&file, 0))?;
        return Ok(0);
    }
    let endpoint = read_address(address, length)?;
    with_socket(fd, |file, socket| socket::connect(socket, endpoint, nonblocking(file, 0))).map(|_| 0)
}

pub fn listen(fd: u64, backlog: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        return socket.listen(backlog as i32 as usize).map(|()| 0);
    }
    with_socket(fd, |_, socket| socket::listen(socket, backlog as i32 as usize)).map(|_| 0)
}

pub fn accept(fd: u64, address: u64, length: u64, flags: u64) -> Result<u64> {
    if let Some((file, socket)) = unix_socket(fd)? {
        let connection = socket.accept(nonblocking(&file, 0))?;
        let peer = connection.peer_address().unwrap_or(unix::Address::Unnamed);
        write_address_bytes(address, length, &peer.encode())?;
        return install(unix::SocketFile::new(connection), flags);
    }
    let (connection, remote) = with_socket(fd, |file, socket| socket::accept(socket, nonblocking(file, 0)))?;
    write_address(address, length, remote)?;
    install(connection, flags)
}

pub fn send_to(fd: u64, buffer: u64, length: u64, flags: u64, address: u64, address_length: u64) -> Result<u64> {
    let data = user::buffer(buffer, length as usize)?;
    if let Some((file, socket)) = unix_socket(fd)? {
        let destination = if address == 0 { None } else { Some(read_unix_address(address, address_length)?) };
        return unix_send(&file, &socket, data, Vec::new(), destination.as_ref(), flags);
    }
    let destination = if address == 0 { None } else { Some(read_address(address, address_length)?) };
    with_socket(fd, |file, socket| {
        socket::send(socket, data, destination, nonblocking(file, flags), flags & MSG_NOSIGNAL != 0)
    })
    .map(|count| count as u64)
}

pub fn receive_from(fd: u64, buffer: u64, length: u64, flags: u64, address: u64, address_length: u64) -> Result<u64> {
    let buf = user::buffer_mut(buffer, length as usize)?;
    if let Some((file, socket)) = unix_socket(fd)? {
        let received = socket.receive(buf, nonblocking(&file, flags), flags & MSG_PEEK != 0)?;
        if address != 0 {
            write_address_bytes(address, address_length, &received.from.encode())?;
        }
        // Files sent along are lost on a plain receive, as on Linux.
        return Ok(if flags & MSG_TRUNC as u64 != 0 && received.truncated { length } else { received.count as u64 });
    }
    let (count, from) = with_socket(fd, |file, socket| receive_all(file, socket, buf, flags))?;
    if let Some(from) = from {
        write_address(address, address_length, from)?;
    }
    Ok(count as u64)
}

/// Receives, and with `MSG_WAITALL` keeps receiving until `buf` is full
/// (or the stream ends).
fn receive_all(file: &OpenFile, socket: &SocketFile, buf: &mut [u8], flags: u64) -> Result<(usize, Option<Endpoint>)> {
    let nonblocking = nonblocking(file, flags);
    let peek = flags & MSG_PEEK != 0;
    let (mut count, from) = socket::receive(socket, buf, nonblocking, peek)?;
    if flags & MSG_WAITALL != 0 && socket.kind == socket::SOCK_STREAM && !peek {
        while count > 0 && count < buf.len() {
            match socket::receive(socket, &mut buf[count..], nonblocking, false) {
                Ok((0, _)) | Err(_) => break,
                Ok((more, _)) => count += more,
            }
        }
    }
    Ok((count, from))
}

/// `struct msghdr`.
#[repr(C)]
#[derive(Clone, Copy)]
struct MessageHeader {
    name: u64,
    name_length: u32,
    _pad: u32,
    iov: u64,
    iov_length: u64,
    control: u64,
    control_length: u64,
    flags: u32,
}

fn iovecs(header: &MessageHeader) -> Result<Vec<(u64, usize)>> {
    if header.iov_length > 1024 {
        return Err(Errno::EMSGSIZE);
    }
    (0..header.iov_length)
        .map(|index| {
            let [base, length]: [u64; 2] = user::read_value(header.iov + index * 16)?;
            Ok((base, length as usize))
        })
        .collect()
}

const SOL_SOCKET_LEVEL: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;

/// The files a `sendmsg`'s control messages carry (`SCM_RIGHTS`).
fn control_files(header: &MessageHeader) -> Result<Vec<Arc<OpenFile>>> {
    let mut files = Vec::new();
    if header.control == 0 || header.control_length < 16 {
        return Ok(files);
    }
    let control = user::read_bytes(header.control, header.control_length.min(64 * 1024) as usize)?;
    let mut offset = 0;
    while offset + 16 <= control.len() {
        let length = u64::from_le_bytes(control[offset..offset + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(control[offset + 8..offset + 12].try_into().unwrap());
        let kind = i32::from_le_bytes(control[offset + 12..offset + 16].try_into().unwrap());
        if length < 16 || offset + length > control.len() {
            return Err(Errno::EINVAL);
        }
        if level == SOL_SOCKET_LEVEL && kind == SCM_RIGHTS {
            let process = process::current();
            let table = process.files.lock();
            for fd in control[offset + 16..offset + length].as_chunks::<4>().0 {
                files.push(table.get(i32::from_le_bytes(*fd) as usize)?);
            }
        }
        offset += length.next_multiple_of(8);
    }
    if files.len() > unix::MAX_FILES_PER_MESSAGE {
        return Err(Errno::EINVAL);
    }
    Ok(files)
}

/// Writes a `recvmsg`'s control messages: received files (installed as
/// new descriptors) and the sender's credentials if asked for. Returns
/// the length written and whether some didn't fit.
fn write_control(
    header: &MessageHeader,
    received: &unix::Received,
    pass_credentials: bool,
    flags: u64,
) -> Result<(u64, bool)> {
    let room = if header.control == 0 { 0 } else { header.control_length as usize };
    let mut control = Vec::new();
    let mut truncated = false;
    if !received.files.is_empty() {
        let fitting = room.saturating_sub(16) / 4;
        truncated |= fitting < received.files.len();
        let mut fds = Vec::new();
        {
            let process = process::current();
            let mut table = process.files.lock();
            for file in received.files.iter().take(fitting) {
                fds.push(table.insert(file.clone(), flags & MSG_CMSG_CLOEXEC != 0, 0)? as i32);
            }
        }
        if !fds.is_empty() {
            let length = 16 + 4 * fds.len();
            control.extend_from_slice(&(length as u64).to_le_bytes());
            control.extend_from_slice(&SOL_SOCKET_LEVEL.to_le_bytes());
            control.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
            fds.iter().for_each(|fd| control.extend_from_slice(&fd.to_le_bytes()));
            control.resize(length.next_multiple_of(8), 0);
        }
    }
    if pass_credentials {
        if room >= control.len() + 28 {
            control.extend_from_slice(&28u64.to_le_bytes());
            control.extend_from_slice(&SOL_SOCKET_LEVEL.to_le_bytes());
            control.extend_from_slice(&SCM_CREDENTIALS.to_le_bytes());
            control.extend_from_slice(&received.sender.bytes());
            control.resize(control.len().next_multiple_of(8), 0);
        } else {
            truncated = true;
        }
    }
    let length = control.len().min(room);
    user::write_bytes(header.control, &control[..length])?;
    Ok((length as u64, truncated))
}

pub fn send_message(fd: u64, message: u64, flags: u64) -> Result<u64> {
    let header: MessageHeader = user::read_value(message)?;
    let mut data = Vec::new();
    for (base, length) in iovecs(&header)? {
        data.extend_from_slice(user::buffer(base, length)?);
    }
    if let Some((file, socket)) = unix_socket(fd)? {
        let files = control_files(&header)?;
        let destination =
            if header.name == 0 { None } else { Some(read_unix_address(header.name, u64::from(header.name_length))?) };
        return unix_send(&file, &socket, &data, files, destination.as_ref(), flags);
    }
    let destination =
        if header.name == 0 { None } else { Some(read_address(header.name, u64::from(header.name_length))?) };
    with_socket(fd, |file, socket| {
        socket::send(socket, &data, destination, nonblocking(file, flags), flags & MSG_NOSIGNAL != 0)
    })
    .map(|count| count as u64)
}

pub fn receive_message(fd: u64, message: u64, flags: u64) -> Result<u64> {
    let mut header: MessageHeader = user::read_value(message)?;
    let vectors = iovecs(&header)?;
    let total: usize = vectors.iter().map(|&(_, length)| length).sum();
    if let Some((file, socket)) = unix_socket(fd)? {
        let mut data = alloc::vec![0; total];
        let received = socket.receive(&mut data, nonblocking(&file, flags), flags & MSG_PEEK != 0)?;
        let mut copied = 0;
        for (base, length) in vectors {
            let chunk = length.min(received.count - copied);
            user::write_bytes(base, &data[copied..copied + chunk])?;
            copied += chunk;
        }
        if header.name != 0 {
            let bytes = received.from.encode();
            let room = (header.name_length as usize).min(bytes.len());
            user::write_bytes(header.name, &bytes[..room])?;
            header.name_length = bytes.len() as u32;
        }
        let pass = socket.pass_credentials.load(core::sync::atomic::Ordering::Relaxed);
        let (control_length, control_truncated) = write_control(&header, &received, pass, flags)?;
        header.control_length = control_length;
        header.flags = if received.truncated { MSG_TRUNC } else { 0 } | if control_truncated { MSG_CTRUNC } else { 0 };
        user::write_value(message, &header)?;
        return Ok(copied as u64);
    }
    // One byte extra tells whether a datagram was cut short.
    let mut data = alloc::vec![0; total + 1];
    let (count, from) = with_socket(fd, |file, socket| receive_all(file, socket, &mut data, flags))?;
    let truncated = count > total;
    let mut copied = 0;
    for (base, length) in vectors {
        let chunk = length.min(count.min(total) - copied);
        user::write_bytes(base, &data[copied..copied + chunk])?;
        copied += chunk;
    }
    if header.name != 0 {
        match from {
            Some(from) => {
                let bytes = address_bytes(from);
                let room = (header.name_length as usize).min(bytes.len());
                user::write_bytes(header.name, &bytes[..room])?;
                header.name_length = bytes.len() as u32;
            }
            None => header.name_length = 0,
        }
    }
    header.control_length = 0;
    header.flags = if truncated { MSG_TRUNC } else { 0 };
    user::write_value(message, &header)?;
    Ok(copied as u64)
}

pub fn shutdown(fd: u64, how: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        return socket.shutdown(how).map(|()| 0);
    }
    with_socket(fd, |_, socket| socket::shutdown(socket, how)).map(|_| 0)
}

pub fn get_socket_name(fd: u64, address: u64, length: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        return write_address_bytes(address, length, &socket.local_address().encode()).map(|()| 0);
    }
    let endpoint = with_socket(fd, |_, socket| socket::local_endpoint(socket))?;
    write_address(address, length, endpoint).map(|_| 0)
}

pub fn get_peer_name(fd: u64, address: u64, length: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        return write_address_bytes(address, length, &socket.peer_address()?.encode()).map(|()| 0);
    }
    let endpoint = with_socket(fd, |_, socket| socket::remote_endpoint(socket))?;
    write_address(address, length, endpoint).map(|_| 0)
}

const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_RCVTIMEO: u64 = 20;
const SO_SNDTIMEO: u64 = 21;
const SO_PASSCRED: u64 = 16;
const SO_PEERCRED: u64 = 17;
const SO_ACCEPTCONN: u64 = 30;
const SO_PROTOCOL: u64 = 38;
const SO_DOMAIN: u64 = 39;
const IPPROTO_TCP: u64 = 6;
const TCP_NODELAY: u64 = 1;
const TCP_MAXSEG: u64 = 2;

/// `setsockopt`: timeouts are honored; other options are accepted and have
/// no effect (eliOS has no Nagle delay, fixed buffers, and so on).
pub fn set_option(fd: u64, level: u64, name: u64, value: u64, length: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        if level == SOL_SOCKET && name == SO_PASSCRED && length >= 4 {
            let on: i32 = user::read_value(value)?;
            socket.pass_credentials.store(on != 0, core::sync::atomic::Ordering::Relaxed);
        }
        return Ok(0);
    }
    with_socket(fd, |_, socket| {
        if level == SOL_SOCKET && (name == SO_RCVTIMEO || name == SO_SNDTIMEO) {
            if length < 16 {
                return Err(Errno::EINVAL);
            }
            let [seconds, microseconds]: [i64; 2] = user::read_value(value)?;
            let ms = (seconds.max(0) as u64) * 1000 + (microseconds.max(0) as u64).div_ceil(1000);
            let slot = if name == SO_RCVTIMEO { &socket.receive_timeout } else { &socket.send_timeout };
            slot.store(ms, core::sync::atomic::Ordering::Relaxed);
        }
        Ok(0)
    })
}

pub fn get_option(fd: u64, level: u64, name: u64, value: u64, length: u64) -> Result<u64> {
    if let Some((_, socket)) = unix_socket(fd)? {
        let room: u32 = user::read_value(length)?;
        if level == SOL_SOCKET && name == SO_PEERCRED {
            let credentials = socket.peer_credentials().ok_or(Errno::ENOTCONN)?;
            let bytes = credentials.bytes();
            let count = (room as usize).min(bytes.len());
            user::write_bytes(value, &bytes[..count])?;
            user::write_value(length, &(count as u32))?;
            return Ok(0);
        }
        let result: i32 = match (level, name) {
            (SOL_SOCKET, SO_TYPE) => socket.kind.as_type(),
            (SOL_SOCKET, SO_DOMAIN) => AF_UNIX as i32,
            (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF) => 256 * 1024,
            (SOL_SOCKET, SO_ACCEPTCONN) => i32::from(socket.is_listening()),
            (SOL_SOCKET, SO_PASSCRED) => i32::from(socket.pass_credentials.load(core::sync::atomic::Ordering::Relaxed)),
            _ => 0,
        };
        if room < 4 {
            return Err(Errno::EINVAL);
        }
        user::write_value(value, &result)?;
        user::write_value(length, &4u32)?;
        return Ok(0);
    }
    with_socket(fd, |_, socket| {
        let result: i32 = match (level, name) {
            (SOL_SOCKET, SO_ERROR) => socket::take_error(socket).map_or(0, |error| error as i32),
            (SOL_SOCKET, SO_TYPE) => socket.kind as i32,
            (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF) => 64 * 1024,
            (SOL_SOCKET, SO_ACCEPTCONN) => i32::from(socket::is_listening(socket)),
            (SOL_SOCKET, SO_DOMAIN) => AF_INET as i32,
            (SOL_SOCKET, SO_PROTOCOL) => {
                if socket.kind == socket::SOCK_STREAM {
                    6
                } else {
                    17
                }
            }
            (IPPROTO_TCP, TCP_NODELAY) => 1,
            (IPPROTO_TCP, TCP_MAXSEG) => 1460,
            _ => 0,
        };
        let room: u32 = user::read_value(length)?;
        if room < 4 {
            return Err(Errno::EINVAL);
        }
        user::write_value(value, &result)?;
        user::write_value(length, &4u32)?;
        Ok(0)
    })
}

// Socket ioctls.
const FIONREAD: u64 = 0x541B;
const SIOCGIFNAME: u64 = 0x8910;
const SIOCGIFCONF: u64 = 0x8912;
const SIOCGIFFLAGS: u64 = 0x8913;
const SIOCGIFADDR: u64 = 0x8915;
const SIOCGIFBRDADDR: u64 = 0x8919;
const SIOCGIFNETMASK: u64 = 0x891B;
const SIOCGIFMETRIC: u64 = 0x891D;
const SIOCGIFMTU: u64 = 0x8921;
const SIOCGIFHWADDR: u64 = 0x8927;
const SIOCGIFINDEX: u64 = 0x8933;
const SIOCGIFTXQLEN: u64 = 0x8942;

const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_LOOPBACK: u16 = 0x8;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;

/// `struct ifreq` is a 16-byte interface name and a 24-byte union.
const IFREQ_SIZE: u64 = 40;

/// ioctls on sockets: bytes waiting, and the interface queries `ifconfig`
/// uses.
pub fn net_ioctl(socket: &SocketFile, request: u64, argument: u64) -> Result<u64> {
    if request == FIONREAD {
        user::write_value(argument, &(socket::available(socket) as i32))?;
        return Ok(0);
    }
    let interfaces = net::interfaces();
    if request == SIOCGIFCONF {
        // struct ifconf: the buffer's length (an int, padded), then the
        // buffer.
        let [length, buffer]: [u64; 2] = user::read_value(argument)?;
        let length = length & 0xFFFF_FFFF;
        let mut written = 0u64;
        for interface in interfaces.iter().filter(|i| i.config.is_some()) {
            if buffer != 0 {
                if written + IFREQ_SIZE > length {
                    break;
                }
                let mut request = [0u8; 40];
                request[..interface.name.len()].copy_from_slice(interface.name.as_bytes());
                let address = interface.config.as_ref().map_or(Ipv4::ANY, |config| config.address);
                request[16..32].copy_from_slice(&address_bytes(Endpoint { address, port: 0 }));
                user::write_value(buffer + written, &request)?;
            }
            written += IFREQ_SIZE;
        }
        user::write_value(argument, &(written as i32))?;
        return Ok(0);
    }

    let mut request_bytes: [u8; 40] = user::read_value(argument)?;
    let index = if request == SIOCGIFNAME {
        i32::from_le_bytes(request_bytes[16..20].try_into().unwrap()) as usize
    } else {
        let name = &request_bytes[..16];
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(16)];
        interfaces.iter().position(|i| i.name.as_bytes() == name).map(|i| i + 1).ok_or(Errno::ENODEV)?
    };
    let interface = interfaces.get(index.wrapping_sub(1)).ok_or(Errno::ENODEV)?;
    let loopback = interface.name == "lo";
    let config = interface.config.as_ref();
    let union = &mut request_bytes[16..40];
    union.fill(0);
    let address = |address: Ipv4| address_bytes(Endpoint { address, port: 0 });
    match request {
        SIOCGIFNAME => {
            request_bytes[..16].fill(0);
            request_bytes[..interface.name.len()].copy_from_slice(interface.name.as_bytes());
        }
        SIOCGIFFLAGS => {
            let mut flags = if interface.up { IFF_UP | IFF_RUNNING } else { IFF_UP };
            flags |= if loopback { IFF_LOOPBACK } else { IFF_BROADCAST | IFF_MULTICAST };
            union[..2].copy_from_slice(&flags.to_le_bytes());
        }
        SIOCGIFADDR => union[..16].copy_from_slice(&address(config.ok_or(Errno::EADDRNOTAVAIL)?.address)),
        SIOCGIFNETMASK => union[..16].copy_from_slice(&address(config.ok_or(Errno::EADDRNOTAVAIL)?.netmask())),
        SIOCGIFBRDADDR => {
            let config = config.ok_or(Errno::EADDRNOTAVAIL)?;
            let broadcast = Ipv4::from_u32(config.address.to_u32() | !config.netmask().to_u32());
            union[..16].copy_from_slice(&address(broadcast));
        }
        SIOCGIFMTU => union[..4].copy_from_slice(&(interface.mtu as i32).to_le_bytes()),
        SIOCGIFMETRIC => {}
        SIOCGIFTXQLEN => union[..4].copy_from_slice(&1000i32.to_le_bytes()),
        SIOCGIFINDEX => union[..4].copy_from_slice(&(index as i32).to_le_bytes()),
        SIOCGIFHWADDR => {
            // ARPHRD_ETHER, or ARPHRD_LOOPBACK.
            let family: u16 = if loopback { 772 } else { 1 };
            union[..2].copy_from_slice(&family.to_le_bytes());
            union[2..8].copy_from_slice(&interface.mac);
        }
        _ => return Err(Errno::ENOTTY),
    }
    user::write_value(argument, &request_bytes)?;
    Ok(0)
}
