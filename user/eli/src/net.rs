//! Networking: IPv4 addresses, TCP and datagram sockets, and looking up
//! names (`/etc/hosts`, then DNS).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sys::{self, Error, Result};
use crate::{fs, io, time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);

    /// Parses dotted decimal (`10.0.2.2`).
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = [0; 4];
        let mut parts = text.split('.');
        for byte in &mut bytes {
            *byte = parts.next()?.parse().ok()?;
        }
        parts.next().is_none().then_some(Self(bytes))
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SocketAddr {
    pub ip: Ipv4Addr,
    pub port: u16,
}

impl SocketAddr {
    pub fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }

    /// As a `struct sockaddr_in`.
    fn to_raw(self) -> [u8; 16] {
        let mut raw = [0; 16];
        raw[..2].copy_from_slice(&AF_INET.to_le_bytes());
        raw[2..4].copy_from_slice(&self.port.to_be_bytes());
        raw[4..8].copy_from_slice(&self.ip.0);
        raw
    }

    fn from_raw(raw: &[u8; 16]) -> Self {
        Self { ip: Ipv4Addr([raw[4], raw[5], raw[6], raw[7]]), port: u16::from_be_bytes([raw[2], raw[3]]) }
    }
}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

const AF_INET: u16 = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_CLOEXEC: usize = 0o2000000;
const IPPROTO_ICMP: usize = 1;
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_RCVTIMEO: usize = 20;
const SHUT_WR: usize = 1;

/// An open socket, closed when dropped.
struct Socket(i32);

impl Socket {
    fn new(kind: usize, protocol: usize) -> Result<Self> {
        // SAFETY: takes no pointers.
        let fd = unsafe { sys::call(sys::SOCKET, [AF_INET as usize, kind | SOCK_CLOEXEC, protocol, 0, 0, 0]) }?;
        Ok(Self(fd as i32))
    }

    fn bind(&self, address: SocketAddr) -> Result<()> {
        let raw = address.to_raw();
        // SAFETY: the address is valid for its length.
        unsafe { sys::call(sys::BIND, [self.0 as usize, raw.as_ptr() as usize, raw.len(), 0, 0, 0]) }.map(|_| ())
    }

    fn set_option(&self, name: usize, value: &[u8]) -> Result<()> {
        // SAFETY: the value is valid for its length.
        unsafe {
            sys::call(sys::SETSOCKOPT, [self.0 as usize, SOL_SOCKET, name, value.as_ptr() as usize, value.len(), 0])
        }
        .map(|_| ())
    }

    /// Makes receiving give up (with "try again") after `ms` milliseconds.
    fn set_read_timeout(&self, ms: u64) -> Result<()> {
        let timeval = [(ms / 1000) as i64, (ms % 1000 * 1000) as i64];
        // SAFETY: plain integers viewed as bytes.
        self.set_option(SO_RCVTIMEO, unsafe { core::slice::from_raw_parts(timeval.as_ptr().cast(), 16) })
    }

    fn local_address(&self) -> Result<SocketAddr> {
        let mut raw = [0u8; 16];
        let mut length = raw.len() as u32;
        // SAFETY: both buffers are valid; the kernel writes at most `length`.
        unsafe {
            sys::call(sys::GETSOCKNAME, [self.0 as usize, raw.as_mut_ptr() as usize, &raw mut length as usize, 0, 0, 0])
        }?;
        Ok(SocketAddr::from_raw(&raw))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = io::close(self.0);
    }
}

/// A TCP connection.
pub struct TcpStream(Socket);

impl TcpStream {
    pub fn connect(address: SocketAddr) -> Result<Self> {
        let socket = Socket::new(SOCK_STREAM, 0)?;
        let raw = address.to_raw();
        // SAFETY: the address is valid for its length.
        unsafe { sys::call(sys::CONNECT, [socket.0 as usize, raw.as_ptr() as usize, raw.len(), 0, 0, 0]) }?;
        Ok(Self(socket))
    }

    pub fn fd(&self) -> i32 {
        self.0.0
    }

    pub fn read(&self, buffer: &mut [u8]) -> Result<usize> {
        io::read(self.0.0, buffer)
    }

    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        io::write_all(self.0.0, data)
    }

    pub fn read_to_end(&self) -> Result<Vec<u8>> {
        io::read_to_end(self.0.0)
    }

    /// Says we're done sending; the other side reads end of file.
    pub fn shutdown_write(&self) -> Result<()> {
        // SAFETY: takes no pointers.
        unsafe { sys::call(sys::SHUTDOWN, [self.0.0 as usize, SHUT_WR, 0, 0, 0, 0]) }.map(|_| ())
    }

    pub fn set_read_timeout(&self, ms: u64) -> Result<()> {
        self.0.set_read_timeout(ms)
    }
}

/// A socket waiting for TCP connections.
pub struct TcpListener(Socket);

impl TcpListener {
    pub fn bind(address: SocketAddr) -> Result<Self> {
        let socket = Socket::new(SOCK_STREAM, 0)?;
        socket.set_option(SO_REUSEADDR, &1i32.to_le_bytes())?;
        socket.bind(address)?;
        // SAFETY: takes no pointers.
        unsafe { sys::call(sys::LISTEN, [socket.0 as usize, 16, 0, 0, 0, 0]) }?;
        Ok(Self(socket))
    }

    pub fn accept(&self) -> Result<(TcpStream, SocketAddr)> {
        let mut raw = [0u8; 16];
        let mut length = raw.len() as u32;
        loop {
            let args = [self.0.0 as usize, raw.as_mut_ptr() as usize, &raw mut length as usize, SOCK_CLOEXEC, 0, 0];
            // SAFETY: both buffers are valid; the kernel writes at most
            // `length`.
            match unsafe { sys::call(sys::ACCEPT4, args) } {
                Ok(fd) => return Ok((TcpStream(Socket(fd as i32)), SocketAddr::from_raw(&raw))),
                Err(Error::INTERRUPTED) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub fn local_address(&self) -> Result<SocketAddr> {
        self.0.local_address()
    }
}

/// A UDP socket, or an ICMP "ping" socket.
pub struct DatagramSocket(Socket);

impl DatagramSocket {
    pub fn udp() -> Result<Self> {
        Socket::new(SOCK_DGRAM, 0).map(Self)
    }

    /// Sends ICMP echo requests and receives the replies; the kernel fills
    /// in the identifier.
    pub fn ping() -> Result<Self> {
        Socket::new(SOCK_DGRAM, IPPROTO_ICMP).map(Self)
    }

    pub fn fd(&self) -> i32 {
        self.0.0
    }

    pub fn bind(&self, address: SocketAddr) -> Result<()> {
        self.0.bind(address)
    }

    pub fn send_to(&self, data: &[u8], to: SocketAddr) -> Result<usize> {
        let raw = to.to_raw();
        let args = [self.0.0 as usize, data.as_ptr() as usize, data.len(), 0, raw.as_ptr() as usize, raw.len()];
        // SAFETY: both buffers are valid for their lengths.
        unsafe { sys::call(sys::SENDTO, args) }
    }

    pub fn receive_from(&self, buffer: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let mut raw = [0u8; 16];
        let mut length = raw.len() as u32;
        let args = [
            self.0.0 as usize,
            buffer.as_mut_ptr() as usize,
            buffer.len(),
            0,
            raw.as_mut_ptr() as usize,
            &raw mut length as usize,
        ];
        // SAFETY: all buffers are valid for their lengths.
        let count = unsafe { sys::call(sys::RECVFROM, args) }?;
        Ok((count, SocketAddr::from_raw(&raw)))
    }

    pub fn set_read_timeout(&self, ms: u64) -> Result<()> {
        self.0.set_read_timeout(ms)
    }

    /// An `ioctl` on the socket (for the interface queries `ifconfig` uses).
    ///
    /// # Safety
    /// `argument` must be what `request` expects.
    pub unsafe fn ioctl(&self, request: usize, argument: *mut u8) -> Result<usize> {
        unsafe { sys::call(sys::IOCTL, [self.0.0 as usize, request, argument as usize, 0, 0, 0]) }
    }
}

pub const POLLIN: i16 = 0x1;
pub const POLLHUP: i16 = 0x10;

/// Waits until one of `fds` is ready for reading (or hung up), for up to
/// `timeout_ms` (negative: forever). Returns which ones are.
pub fn poll_readable(fds: &[i32], timeout_ms: i32) -> Result<Vec<bool>> {
    let mut entries: Vec<[i32; 2]> = fds.iter().map(|&fd| [fd, i32::from(POLLIN)]).collect();
    loop {
        let args = [entries.as_mut_ptr() as usize, entries.len(), timeout_ms as usize, 0, 0, 0];
        // SAFETY: the entries have the layout of `struct pollfd`.
        match unsafe { sys::call(sys::POLL, args) } {
            Ok(_) => break,
            Err(Error::INTERRUPTED) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(entries.iter().map(|entry| (entry[1] >> 16) as i16 & (POLLIN | POLLHUP) != 0).collect())
}

/// Finds a host's address: a dotted address as is, then `/etc/hosts`, then
/// DNS through the servers in `/etc/resolv.conf`.
pub fn resolve(host: &str) -> Result<Ipv4Addr> {
    if let Some(address) = Ipv4Addr::parse(host) {
        return Ok(address);
    }
    if let Some(address) = lookup_hosts_file(host) {
        return Ok(address);
    }
    let servers = name_servers();
    if servers.is_empty() {
        return Err(Error::NOT_FOUND);
    }
    for server in servers {
        for _ in 0..2 {
            match query_dns(server, host) {
                Ok(Some(address)) => return Ok(address),
                Ok(None) => return Err(Error::NOT_FOUND),
                Err(_) => continue,
            }
        }
    }
    Err(Error::TIMED_OUT)
}

fn lookup_hosts_file(host: &str) -> Option<Ipv4Addr> {
    let text = fs::read_to_string("/etc/hosts").ok()?;
    text.lines().map(|line| line.split('#').next().unwrap_or("")).find_map(|line| {
        let mut words = line.split_whitespace();
        let address = Ipv4Addr::parse(words.next()?)?;
        words.any(|name| name.eq_ignore_ascii_case(host)).then_some(address)
    })
}

fn name_servers() -> Vec<Ipv4Addr> {
    let text = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|rest| Ipv4Addr::parse(rest.trim()))
        .collect()
}

/// Asks `server` for `host`'s A record. `Ok(None)` means it doesn't exist.
fn query_dns(server: Ipv4Addr, host: &str) -> Result<Option<Ipv4Addr>> {
    let socket = DatagramSocket::udp()?;
    socket.set_read_timeout(2000)?;
    let id = (time::monotonic_nanos() >> 10) as u16;
    // Header: id, "recursion desired", one question.
    let mut query = Vec::new();
    query.extend_from_slice(&id.to_be_bytes());
    query.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Ok(None);
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    // Type A, class IN.
    query.extend_from_slice(&[0, 0, 1, 0, 1]);
    socket.send_to(&query, SocketAddr::new(server, 53))?;

    let mut reply = [0u8; 512];
    loop {
        let (length, from) = socket.receive_from(&mut reply)?;
        let reply = &reply[..length];
        if from.ip != server || length < 12 || reply[..2] != id.to_be_bytes() {
            continue;
        }
        // Response code 3: no such name.
        if reply[3] & 0xF == 3 {
            return Ok(None);
        }
        return Ok(parse_answer(reply));
    }
}

/// The first A record in a DNS reply's answers.
fn parse_answer(reply: &[u8]) -> Option<Ipv4Addr> {
    let questions = u16::from_be_bytes([reply[4], reply[5]]);
    let answers = u16::from_be_bytes([reply[6], reply[7]]);
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(reply, offset)? + 4;
    }
    for _ in 0..answers {
        offset = skip_name(reply, offset)?;
        let record = reply.get(offset..offset + 10)?;
        let kind = u16::from_be_bytes([record[0], record[1]]);
        let length = usize::from(u16::from_be_bytes([record[8], record[9]]));
        offset += 10;
        let data = reply.get(offset..offset + length)?;
        if kind == 1 && length == 4 {
            return Some(Ipv4Addr([data[0], data[1], data[2], data[3]]));
        }
        offset += length;
    }
    None
}

/// Skips a (possibly compressed) name; returns the offset after it.
fn skip_name(message: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = *message.get(offset)?;
        match length {
            0 => return Some(offset + 1),
            // A pointer to a name elsewhere ends this one.
            _ if length & 0xC0 == 0xC0 => return Some(offset + 2),
            _ => offset += 1 + usize::from(length),
        }
    }
}

/// Splits `host:port`, with `default_port` if there's no port.
pub fn split_host_port(text: &str, default_port: u16) -> Option<(String, u16)> {
    match text.rsplit_once(':') {
        Some((host, port)) => Some((host.into(), port.parse().ok()?)),
        None => Some((text.into(), default_port)),
    }
}
