//! Sockets: how programs use the network.
//!
//! A socket is an entry in the stack's socket table, and a file (a
//! [`SocketFile`] inode) that refers to it, so `read`, `write`, `poll` and
//! `close` work on sockets as on any file. The socket system calls
//! (`syscall/net.rs`) call the functions here, which take the stack's lock,
//! and wait on [`SOCKET_EVENTS`] when they must.
//!
//! Datagram sockets are UDP sockets and ICMP "ping" sockets (and raw ICMP
//! sockets); stream sockets are TCP connections (see `tcp.rs`).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::tcp::{Connection, State};
use super::{Ipv4, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP, SOCKET_EVENTS, STACK, Stack};
use crate::errno::{Errno, Result};
use crate::fs::{FileKind, Inode, Metadata, POLLERR, POLLHUP, POLLIN, POLLOUT};
use crate::process;

pub type SocketId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Endpoint {
    pub address: Ipv4,
    pub port: u16,
}

pub enum Socket {
    Datagram(Datagram),
    Stream(Box<Connection>),
}

pub struct Datagram {
    /// `PROTOCOL_UDP` or `PROTOCOL_ICMP`.
    pub protocol: u8,
    /// For ICMP: raw sockets see whole IP packets, of every ICMP type.
    pub raw: bool,
    /// The bound address and port (for ping sockets, the echo identifier).
    pub local: Option<Endpoint>,
    /// Set by `connect`: the default destination, and the only source
    /// accepted.
    pub remote: Option<Endpoint>,
    pub queue: VecDeque<(Endpoint, Vec<u8>)>,
}

/// Datagrams kept per socket before new ones are dropped.
pub const DATAGRAM_QUEUE: usize = 64;
const EPHEMERAL_PORTS: core::ops::RangeInclusive<u16> = 49152..=65535;

pub struct Sockets {
    map: BTreeMap<SocketId, Socket>,
    next_id: SocketId,
    next_port: u16,
}

impl Sockets {
    pub const fn new() -> Self {
        Self { map: BTreeMap::new(), next_id: 1, next_port: 49152 }
    }

    pub fn insert(&mut self, socket: Socket) -> SocketId {
        let id = self.next_id;
        self.next_id += 1;
        self.map.insert(id, socket);
        id
    }

    pub fn get_mut(&mut self, id: SocketId) -> Option<&mut Socket> {
        self.map.get_mut(&id)
    }

    pub fn remove(&mut self, id: SocketId) -> Option<Socket> {
        self.map.remove(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SocketId, &Socket)> {
        self.map.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&SocketId, &mut Socket)> {
        self.map.iter_mut()
    }

    pub fn ids(&self) -> Vec<SocketId> {
        self.map.keys().copied().collect()
    }

    pub fn connection(&mut self, id: SocketId) -> Option<&mut Connection> {
        match self.map.get_mut(&id)? {
            Socket::Stream(connection) => Some(connection),
            Socket::Datagram(_) => None,
        }
    }

    /// Whether a socket of `protocol` is bound to `port` (and, for TCP, not
    /// just a connection that happens to use it).
    fn port_in_use(&self, protocol: u8, port: u16) -> bool {
        self.map.values().any(|socket| match socket {
            Socket::Datagram(datagram) => {
                datagram.protocol == protocol && datagram.local.is_some_and(|l| l.port == port)
            }
            Socket::Stream(connection) => {
                protocol == PROTOCOL_TCP && connection.local.port == port && connection.state != State::Closed
            }
        })
    }

    fn ephemeral_port(&mut self, protocol: u8) -> Result<u16> {
        for _ in EPHEMERAL_PORTS {
            let port = self.next_port;
            self.next_port = if port == *EPHEMERAL_PORTS.end() { *EPHEMERAL_PORTS.start() } else { port + 1 };
            if !self.port_in_use(protocol, port) {
                return Ok(port);
            }
        }
        Err(Errno::EADDRINUSE)
    }

    pub fn next_tcp_deadline(&self) -> Option<u64> {
        self.map
            .values()
            .filter_map(|socket| match socket {
                Socket::Stream(connection) => connection.next_deadline(),
                Socket::Datagram(_) => None,
            })
            .min()
    }
}

// Socket types and options (Linux values).
pub const AF_INET: u64 = 2;
pub const SOCK_STREAM: u64 = 1;
pub const SOCK_DGRAM: u64 = 2;
pub const SOCK_RAW: u64 = 3;

/// The file a socket is reached through.
pub struct SocketFile {
    pub id: SocketId,
    pub kind: u64,
    /// `SO_RCVTIMEO` and `SO_SNDTIMEO`, in milliseconds (0: wait forever).
    pub receive_timeout: AtomicU64,
    pub send_timeout: AtomicU64,
    inode: u64,
}

impl Inode for SocketFile {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Socket, self.inode, 0o777)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, false)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        self.write_stream(buf, false)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        receive(self, buf, nonblocking, false).map(|(count, _)| count)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
        send(self, buf, None, nonblocking, false)
    }

    fn poll(&self, _flags: u32) -> u16 {
        poll(self.id)
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<crate::fs::PollSource>>, _depth: usize) -> bool {
        sources.push(super::sockets_changed().clone());
        true
    }

    fn on_close(&self, _flags: u32) {
        close(self.id);
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        crate::syscall::net_ioctl(self, request, argument)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

static NEXT_INODE: AtomicU64 = AtomicU64::new(1);

/// `socket()`: a new, unbound socket.
pub fn create(domain: u64, kind: u64, protocol: u64) -> Result<Arc<SocketFile>> {
    if domain != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let socket = match (kind, protocol) {
        (SOCK_STREAM, 0 | 6) => Socket::Stream(Box::new(Connection::new())),
        (SOCK_DGRAM, 0 | 17) => Socket::Datagram(Datagram::new(PROTOCOL_UDP, false)),
        (SOCK_DGRAM, 1) => Socket::Datagram(Datagram::new(PROTOCOL_ICMP, false)),
        (SOCK_RAW, 1) => Socket::Datagram(Datagram::new(PROTOCOL_ICMP, true)),
        (SOCK_STREAM | SOCK_DGRAM | SOCK_RAW, _) => return Err(Errno::EPROTONOSUPPORT),
        _ => return Err(Errno::EINVAL),
    };
    let id = STACK.lock().sockets.insert(socket);
    Ok(Arc::new(SocketFile {
        id,
        kind,
        receive_timeout: AtomicU64::new(0),
        send_timeout: AtomicU64::new(0),
        inode: NEXT_INODE.fetch_add(1, Ordering::Relaxed),
    }))
}

impl Datagram {
    fn new(protocol: u8, raw: bool) -> Self {
        Self { protocol, raw, local: None, remote: None, queue: VecDeque::new() }
    }
}

/// Waits until `attempt` gives a result. `attempt` runs with the stack
/// locked; it returns `None` to keep waiting.
fn wait<T>(nonblocking: bool, timeout_ms: u64, mut attempt: impl FnMut(&mut Stack) -> Option<Result<T>>) -> Result<T> {
    let deadline = (timeout_ms != 0).then(|| crate::time::ticks() + timeout_ms);
    let result = SOCKET_EVENTS.wait_until_deadline(deadline, || {
        if let Some(result) = attempt(&mut STACK.lock()) {
            return Some(result);
        }
        if nonblocking {
            return Some(Err(Errno::EAGAIN));
        }
        process::signal_pending().then_some(Err(Errno::EINTR))
    });
    // A timeout reads as "try again", as on Linux.
    result.unwrap_or(Err(Errno::EAGAIN))
}

/// `bind()`. Port 0 picks a free port.
pub fn bind(file: &SocketFile, endpoint: Endpoint) -> Result<()> {
    let mut stack = STACK.lock();
    if endpoint.address != Ipv4::ANY && !stack.is_local(endpoint.address) {
        return Err(Errno::EADDRNOTAVAIL);
    }
    let protocol = match stack.sockets.get_mut(file.id).ok_or(Errno::EBADF)? {
        Socket::Datagram(datagram) if datagram.local.is_some() => return Err(Errno::EINVAL),
        Socket::Datagram(datagram) => datagram.protocol,
        Socket::Stream(connection) if connection.bound => return Err(Errno::EINVAL),
        Socket::Stream(_) => PROTOCOL_TCP,
    };
    let port = match endpoint.port {
        0 => stack.sockets.ephemeral_port(protocol)?,
        port if stack.sockets.port_in_use(protocol, port) => return Err(Errno::EADDRINUSE),
        port => port,
    };
    let endpoint = Endpoint { port, ..endpoint };
    match stack.sockets.get_mut(file.id).ok_or(Errno::EBADF)? {
        Socket::Datagram(datagram) => datagram.local = Some(endpoint),
        Socket::Stream(connection) => {
            connection.local = endpoint;
            connection.bound = true;
        }
    }
    Ok(())
}

/// Binds a datagram socket to a free port, if it isn't bound yet.
fn bind_datagram_if_needed(stack: &mut Stack, id: SocketId) -> Result<Endpoint> {
    let (protocol, local) = match stack.sockets.get_mut(id) {
        Some(Socket::Datagram(datagram)) => (datagram.protocol, datagram.local),
        _ => return Err(Errno::EBADF),
    };
    if let Some(local) = local {
        return Ok(local);
    }
    let port = stack.sockets.ephemeral_port(protocol)?;
    let local = Endpoint { address: Ipv4::ANY, port };
    if let Some(Socket::Datagram(datagram)) = stack.sockets.get_mut(id) {
        datagram.local = Some(local);
    }
    Ok(local)
}

/// `connect()`: for TCP, opens a connection; for datagram sockets, sets the
/// default destination.
pub fn connect(file: &SocketFile, remote: Endpoint, nonblocking: bool) -> Result<()> {
    {
        let mut stack = STACK.lock();
        let stack = &mut *stack;
        let source = stack.source_for(remote.address).ok_or(Errno::ENETUNREACH)?;
        match stack.sockets.get_mut(file.id).ok_or(Errno::EBADF)? {
            Socket::Datagram(_) => {
                bind_datagram_if_needed(stack, file.id)?;
                if let Some(Socket::Datagram(datagram)) = stack.sockets.get_mut(file.id) {
                    datagram.remote = Some(remote);
                    // As on Linux, a connected socket bound to any address
                    // gets the one its packets will come from, which
                    // `getsockname` then shows: that's how programs (Firefox's
                    // WebRTC, for one) learn the default route's address.
                    if let Some(local) = &mut datagram.local
                        && local.address == Ipv4::ANY
                    {
                        local.address = source;
                    }
                }
                return Ok(());
            }
            Socket::Stream(connection) => match connection.state {
                State::Closed if connection.error.is_none() => {}
                State::SynSent => return Err(Errno::EALREADY),
                State::Listen => return Err(Errno::EINVAL),
                _ => return Err(Errno::EISCONN),
            },
        }
        let port = match stack.sockets.connection(file.id) {
            Some(connection) if connection.bound => connection.local.port,
            _ => stack.sockets.ephemeral_port(PROTOCOL_TCP)?,
        };
        let connection = stack.sockets.connection(file.id).ok_or(Errno::EBADF)?;
        connection.local = Endpoint { address: source, port };
        connection.bound = true;
        connection.open(remote, crate::time::ticks());
        stack.tcp_output(file.id);
    }
    if nonblocking {
        return Err(Errno::EINPROGRESS);
    }
    wait(false, 0, |stack| {
        let connection = stack.sockets.connection(file.id)?;
        match connection.state {
            State::SynSent => None,
            State::Closed => Some(Err(connection.error.take().unwrap_or(Errno::ECONNREFUSED))),
            _ => Some(Ok(())),
        }
    })
}

/// `listen()`.
pub fn listen(file: &SocketFile, backlog: usize) -> Result<()> {
    let mut stack = STACK.lock();
    let stack = &mut *stack;
    let bound = match stack.sockets.connection(file.id) {
        None => return Err(Errno::EOPNOTSUPP),
        Some(connection) if !matches!(connection.state, State::Closed | State::Listen) => return Err(Errno::EISCONN),
        Some(connection) => connection.bound,
    };
    if !bound {
        let port = stack.sockets.ephemeral_port(PROTOCOL_TCP)?;
        let connection = stack.sockets.connection(file.id).ok_or(Errno::EBADF)?;
        connection.local = Endpoint { address: Ipv4::ANY, port };
        connection.bound = true;
    }
    stack.sockets.connection(file.id).ok_or(Errno::EBADF)?.listen(backlog);
    Ok(())
}

/// `accept()`: the next connection made to a listening socket.
pub fn accept(file: &SocketFile, nonblocking: bool) -> Result<(Arc<SocketFile>, Endpoint)> {
    let id = wait(nonblocking, file.receive_timeout.load(Ordering::Relaxed), |stack| {
        let listener = stack.sockets.connection(file.id)?;
        if listener.state != State::Listen {
            return Some(Err(Errno::EINVAL));
        }
        listener.accept_queue.pop_front().map(Ok)
    })?;
    let mut stack = STACK.lock();
    let connection = stack.sockets.connection(id).ok_or(Errno::ECONNABORTED)?;
    connection.parent = None;
    let remote = connection.remote;
    Ok((
        Arc::new(SocketFile {
            id,
            kind: SOCK_STREAM,
            receive_timeout: AtomicU64::new(0),
            send_timeout: AtomicU64::new(0),
            inode: NEXT_INODE.fetch_add(1, Ordering::Relaxed),
        }),
        remote,
    ))
}

/// `send`/`sendto`/`write`: sends data, to `destination` for unconnected
/// datagram sockets. Stream sockets wait until everything is queued (unless
/// nonblocking); `no_signal` is `MSG_NOSIGNAL`.
pub fn send(
    file: &SocketFile,
    data: &[u8],
    destination: Option<Endpoint>,
    nonblocking: bool,
    no_signal: bool,
) -> Result<usize> {
    let timeout = file.send_timeout.load(Ordering::Relaxed);
    let datagram = matches!(STACK.lock().sockets.get_mut(file.id), Some(Socket::Datagram(_)));
    if datagram {
        let mut stack = STACK.lock();
        return stack.send_datagram(file.id, data, destination);
    }
    let mut sent = 0;
    loop {
        let result = wait(nonblocking, timeout, |stack| {
            let connection = stack.sockets.connection(file.id)?;
            if let Some(error) = connection.error.take() {
                return Some(Err(error));
            }
            match connection.state {
                State::SynSent | State::SynReceived => return None,
                State::Established | State::CloseWait if !connection.write_shutdown() => {}
                _ => return Some(Err(Errno::EPIPE)),
            }
            let count = connection.queue_data(&data[sent..]);
            if count == 0 {
                return None;
            }
            stack.tcp_output(file.id);
            Some(Ok(count))
        });
        match result {
            Ok(count) => sent += count,
            Err(_) if sent > 0 => return Ok(sent),
            Err(error) => {
                if error == Errno::EPIPE && !no_signal {
                    process::send_signal_to_self(process::SIGPIPE);
                }
                return Err(error);
            }
        }
        if sent == data.len() {
            return Ok(sent);
        }
    }
}

/// `recv`/`recvfrom`/`read`: returns how much was read and, for datagram
/// sockets, who sent it. `peek` (`MSG_PEEK`) leaves the data queued.
pub fn receive(file: &SocketFile, buf: &mut [u8], nonblocking: bool, peek: bool) -> Result<(usize, Option<Endpoint>)> {
    let timeout = file.receive_timeout.load(Ordering::Relaxed);
    wait(nonblocking, timeout, |stack| match stack.sockets.get_mut(file.id)? {
        Socket::Datagram(datagram) => {
            let (from, data) = if peek { datagram.queue.front()?.clone() } else { datagram.queue.pop_front()? };
            let count = data.len().min(buf.len());
            buf[..count].copy_from_slice(&data[..count]);
            Some(Ok((count, Some(from))))
        }
        Socket::Stream(connection) => {
            if buf.is_empty() {
                return Some(Ok((0, None)));
            }
            let count = connection.read(buf, peek);
            if count > 0 {
                if !peek {
                    stack.tcp_window_update(file.id);
                }
                return Some(Ok((count, None)));
            }
            let connection = stack.sockets.connection(file.id)?;
            if let Some(error) = connection.error.take() {
                return Some(Err(error));
            }
            if connection.at_end() {
                return Some(Ok((0, None)));
            }
            if connection.state == State::Closed || connection.state == State::Listen {
                return Some(Err(Errno::ENOTCONN));
            }
            None
        }
    })
}

/// `shutdown()`: 0 stops reading, 1 writing, 2 both.
pub fn shutdown(file: &SocketFile, how: u64) -> Result<()> {
    let mut stack = STACK.lock();
    let connection = stack.sockets.connection(file.id).ok_or(Errno::EOPNOTSUPP)?;
    if matches!(connection.state, State::Closed | State::Listen | State::SynSent) {
        return Err(Errno::ENOTCONN);
    }
    if how == 0 || how == 2 {
        connection.shutdown_read();
    }
    if how == 1 || how == 2 {
        connection.shutdown_write();
        stack.tcp_output(file.id);
    }
    drop(stack);
    super::notify();
    Ok(())
}

pub fn local_endpoint(file: &SocketFile) -> Result<Endpoint> {
    let mut stack = STACK.lock();
    match stack.sockets.get_mut(file.id).ok_or(Errno::EBADF)? {
        Socket::Datagram(datagram) => Ok(datagram.local.unwrap_or_default()),
        Socket::Stream(connection) => Ok(connection.local),
    }
}

pub fn remote_endpoint(file: &SocketFile) -> Result<Endpoint> {
    let mut stack = STACK.lock();
    match stack.sockets.get_mut(file.id).ok_or(Errno::EBADF)? {
        Socket::Datagram(datagram) => datagram.remote.ok_or(Errno::ENOTCONN),
        Socket::Stream(connection) if connection.connected() => Ok(connection.remote),
        Socket::Stream(_) => Err(Errno::ENOTCONN),
    }
}

/// `SO_ERROR`: the pending error (after a nonblocking connect, say), once.
pub fn take_error(file: &SocketFile) -> Option<Errno> {
    STACK.lock().sockets.connection(file.id)?.error.take()
}

/// A socket as /proc/net lists it.
pub struct SocketInfo {
    pub protocol: u8,
    pub local: Endpoint,
    pub remote: Endpoint,
    /// For TCP.
    pub state: Option<State>,
    /// Bytes queued to send, and received but unread.
    pub queued: (usize, usize),
}

pub fn table() -> Vec<SocketInfo> {
    STACK
        .lock()
        .sockets
        .iter()
        .map(|(_, socket)| match socket {
            Socket::Datagram(datagram) => SocketInfo {
                protocol: datagram.protocol,
                local: datagram.local.unwrap_or_default(),
                remote: datagram.remote.unwrap_or_default(),
                state: None,
                queued: (0, datagram.queue.iter().map(|(_, data)| data.len()).sum()),
            },
            Socket::Stream(connection) => SocketInfo {
                protocol: PROTOCOL_TCP,
                local: connection.local,
                remote: connection.remote,
                state: Some(connection.state),
                queued: connection.queued(),
            },
        })
        .collect()
}

pub fn is_listening(file: &SocketFile) -> bool {
    STACK.lock().sockets.connection(file.id).is_some_and(|connection| connection.state == State::Listen)
}

/// Bytes waiting to be read (`FIONREAD`).
pub fn available(file: &SocketFile) -> usize {
    match STACK.lock().sockets.get_mut(file.id) {
        Some(Socket::Datagram(datagram)) => datagram.queue.front().map_or(0, |(_, data)| data.len()),
        Some(Socket::Stream(connection)) => connection.available(),
        None => 0,
    }
}

pub fn poll(id: SocketId) -> u16 {
    let mut stack = STACK.lock();
    match stack.sockets.get_mut(id) {
        None => POLLERR,
        Some(Socket::Datagram(datagram)) => POLLOUT | if datagram.queue.is_empty() { 0 } else { POLLIN },
        Some(Socket::Stream(connection)) => {
            let mut events = 0;
            if connection.readable() {
                events |= POLLIN;
            }
            if connection.writable() {
                events |= POLLOUT;
            }
            if connection.error.is_some() {
                events |= POLLERR;
            }
            if connection.hung_up() {
                events |= POLLHUP;
            }
            events
        }
    }
}

/// The last file referring to a socket was closed.
pub fn close(id: SocketId) {
    let mut stack = STACK.lock();
    let is_stream = matches!(stack.sockets.get_mut(id), Some(Socket::Stream(_)));
    if is_stream {
        stack.tcp_close(id);
    } else {
        stack.sockets.remove(id);
    }
    drop(stack);
    super::notify();
}

impl Stack {
    fn send_datagram(&mut self, id: SocketId, data: &[u8], destination: Option<Endpoint>) -> Result<usize> {
        let local = bind_datagram_if_needed(self, id)?;
        let Some(Socket::Datagram(datagram)) = self.sockets.get_mut(id) else { return Err(Errno::EBADF) };
        let (protocol, raw) = (datagram.protocol, datagram.raw);
        let remote = destination.or(datagram.remote).ok_or(Errno::EDESTADDRREQ)?;
        let source = if local.address == Ipv4::ANY {
            self.source_for(remote.address).ok_or(Errno::ENETUNREACH)?
        } else {
            local.address
        };
        if protocol == PROTOCOL_UDP {
            if data.len() > 65507 {
                return Err(Errno::EMSGSIZE);
            }
            self.send_udp(Endpoint { address: source, port: local.port }, remote, data)?;
        } else {
            self.send_icmp_from_socket(source, remote.address, local.port, raw, data)?;
        }
        Ok(data.len())
    }

    /// Delivers a datagram to the sockets that want it; returns whether any
    /// did.
    pub(super) fn deliver_datagram(
        &mut self,
        protocol: u8,
        local: Endpoint,
        from: Endpoint,
        data: &[u8],
        raw: bool,
    ) -> bool {
        let mut delivered = false;
        for (_, socket) in self.sockets.iter_mut() {
            let Socket::Datagram(datagram) = socket else { continue };
            if datagram.protocol != protocol || datagram.raw != raw {
                continue;
            }
            let wanted = raw
                || datagram.local.is_some_and(|bound| {
                    bound.port == local.port && (bound.address == Ipv4::ANY || bound.address == local.address)
                }) && datagram.remote.is_none_or(|remote| {
                    remote.address == from.address && (remote.port == from.port || protocol == PROTOCOL_ICMP)
                });
            if wanted && datagram.queue.len() < DATAGRAM_QUEUE {
                datagram.queue.push_back((from, data.to_vec()));
                delivered = true;
                if !raw {
                    break;
                }
            }
        }
        delivered
    }
}
