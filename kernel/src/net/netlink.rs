//! Route netlink sockets (`AF_NETLINK`, `NETLINK_ROUTE`): how Linux programs
//! list network interfaces, their addresses and the routes.
//!
//! musl's `getifaddrs` and `if_nameindex` use them, and so do `ip` and
//! Firefox's WebRTC, which gathers its ICE candidates from the
//! addresses. Without them a video call has no candidates and fails.
//!
//! A program sends requests (a `struct nlmsghdr` and a request body) and
//! reads back datagrams of replies. The requests here are the *dumps*:
//! `RTM_GETLINK`, `RTM_GETADDR` and `RTM_GETROUTE`, answered with one
//! `RTM_NEW...` message per item and an `NLMSG_DONE`. Anything else gets
//! an `NLMSG_ERROR` saying `EOPNOTSUPP`. Programs can also subscribe to
//! change notifications (by binding to groups); eliOS accepts that and
//! sends none, since addresses rarely change after DHCP.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{FileKind, Inode, Metadata, POLLIN, POLLOUT, PollSource, ramfs};
use crate::net::{self, InterfaceInfo};
use crate::process;
use crate::sync::SpinLock;

pub const AF_NETLINK: u64 = 16;
const NETLINK_ROUTE: u64 = 0;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;

// Message types.
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;

// Header flags.
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;

const HEADER: usize = 16;
/// Replies are packed into datagrams of up to this size, like Linux's
/// page-sized dump buffers (programs read with buffers of 4 KiB or more).
const DATAGRAM: usize = 4096;

/// Port ids in use: a socket takes its process's pid if that's free, as
/// on Linux, or else a number counting down from -4096.
static PORTS: SpinLock<BTreeSet<u32>> = SpinLock::new(BTreeSet::new());

fn take_port() -> u32 {
    let mut ports = PORTS.lock();
    // Kernel threads (tests) have no pid; they start at the counter.
    let mut next = (-4096i32) as u32;
    let mut port = crate::task::current_process().map_or(next, |process| process.pid);
    while ports.contains(&port) {
        port = next;
        next = next.wrapping_sub(1);
    }
    ports.insert(port);
    port
}

struct State {
    port: u32,
    groups: u32,
    /// Reply datagrams not yet read.
    replies: VecDeque<Vec<u8>>,
}

pub struct NetlinkSocket {
    inode: u64,
    kind: u64,
    state: SpinLock<State>,
    source: Arc<PollSource>,
}

/// `socket(AF_NETLINK, ...)`.
pub fn create(kind: u64, protocol: u64) -> Result<Arc<NetlinkSocket>> {
    if kind != SOCK_RAW && kind != SOCK_DGRAM {
        return Err(Errno::ESOCKTNOSUPPORT);
    }
    if protocol != NETLINK_ROUTE {
        return Err(Errno::EPROTONOSUPPORT);
    }
    Ok(Arc::new(NetlinkSocket {
        inode: ramfs::next_inode_number(),
        kind,
        state: SpinLock::new(State { port: take_port(), groups: 0, replies: VecDeque::new() }),
        source: Arc::new(PollSource::new()),
    }))
}

/// A `struct sockaddr_nl`: family, padding, port id, groups.
fn address_bytes(port: u32, groups: u32) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
    bytes[4..8].copy_from_slice(&port.to_le_bytes());
    bytes[8..12].copy_from_slice(&groups.to_le_bytes());
    bytes
}

impl NetlinkSocket {
    pub fn kind(&self) -> u64 {
        self.kind
    }

    /// `bind`: chooses the groups (accepted, never notified). A port id
    /// other than 0 or the one already held is refused: eliOS's programs
    /// don't pick their own.
    pub fn bind(&self, address: &[u8]) -> Result<()> {
        if address.len() < 12 || u16::from_le_bytes([address[0], address[1]]) != AF_NETLINK as u16 {
            return Err(Errno::EINVAL);
        }
        let port = u32::from_le_bytes(address[4..8].try_into().unwrap());
        let mut state = self.state.lock();
        if port != 0 && port != state.port {
            return Err(Errno::EADDRINUSE);
        }
        state.groups = u32::from_le_bytes(address[8..12].try_into().unwrap());
        Ok(())
    }

    pub fn local_address(&self) -> [u8; 12] {
        let state = self.state.lock();
        address_bytes(state.port, state.groups)
    }

    /// The kernel's address, the source of every reply.
    pub fn kernel_address() -> [u8; 12] {
        address_bytes(0, 0)
    }

    /// Takes a datagram of requests and queues the replies.
    pub fn send(&self, data: &[u8]) -> Result<usize> {
        let port = self.state.lock().port;
        let mut replies = Replies::new(port);
        let mut offset = 0;
        while offset + HEADER <= data.len() {
            let length = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            if length < HEADER || offset + length > data.len() {
                break;
            }
            let message = &data[offset..offset + length];
            answer(message, &mut replies);
            offset += align(length);
        }
        let datagrams = replies.finish();
        if !datagrams.is_empty() {
            self.state.lock().replies.extend(datagrams);
            self.source.notify();
        }
        Ok(data.len())
    }

    /// Reads one reply datagram. Returns the bytes copied and the
    /// datagram's full length (more means it was cut short; the rest is
    /// lost, as with any datagram).
    pub fn receive(&self, buf: &mut [u8], nonblocking: bool, peek: bool) -> Result<(usize, usize)> {
        let take = || {
            let mut state = self.state.lock();
            if peek { state.replies.front().cloned() } else { state.replies.pop_front() }
        };
        let datagram = match take() {
            Some(datagram) => datagram,
            None if nonblocking => return Err(Errno::EAGAIN),
            None => self
                .source
                .queue()
                .wait_until(|| if process::signal_pending() { Some(None) } else { take().map(Some) })
                .ok_or(Errno::EINTR)?,
        };
        let count = buf.len().min(datagram.len());
        buf[..count].copy_from_slice(&datagram[..count]);
        Ok((count, datagram.len()))
    }

    pub fn available(&self) -> usize {
        self.state.lock().replies.front().map_or(0, Vec::len)
    }
}

const fn align(length: usize) -> usize {
    (length + 3) & !3
}

/// Reply messages, packed into datagrams.
struct Replies {
    port: u32,
    sequence: u32,
    datagrams: Vec<Vec<u8>>,
    current: Vec<u8>,
}

impl Replies {
    fn new(port: u32) -> Self {
        Self { port, sequence: 0, datagrams: Vec::new(), current: Vec::new() }
    }

    /// Adds one message: a header and a body (attributes included).
    fn push(&mut self, kind: u16, flags: u16, body: &[u8]) {
        let length = HEADER + body.len();
        if !self.current.is_empty() && self.current.len() + align(length) > DATAGRAM {
            self.datagrams.push(core::mem::take(&mut self.current));
        }
        self.current.extend_from_slice(&(length as u32).to_le_bytes());
        self.current.extend_from_slice(&kind.to_le_bytes());
        self.current.extend_from_slice(&flags.to_le_bytes());
        self.current.extend_from_slice(&self.sequence.to_le_bytes());
        self.current.extend_from_slice(&self.port.to_le_bytes());
        self.current.extend_from_slice(body);
        self.current.resize(self.current.len() + align(length) - length, 0);
    }

    /// Ends a dump. `NLMSG_DONE` goes in a datagram of its own, as Linux
    /// sends it.
    fn done(&mut self) {
        if !self.current.is_empty() {
            self.datagrams.push(core::mem::take(&mut self.current));
        }
        self.push(NLMSG_DONE, NLM_F_MULTI, &0i32.to_le_bytes());
        self.datagrams.push(core::mem::take(&mut self.current));
    }

    /// An `NLMSG_ERROR`: the error (0 for an acknowledgement) and the
    /// request's header.
    fn error(&mut self, error: i32, request: &[u8]) {
        let mut body = Vec::with_capacity(4 + HEADER);
        body.extend_from_slice(&(-error).to_le_bytes());
        body.extend_from_slice(&request[..HEADER]);
        self.push(NLMSG_ERROR, 0, &body);
    }

    fn finish(mut self) -> Vec<Vec<u8>> {
        if !self.current.is_empty() {
            self.datagrams.push(self.current);
        }
        self.datagrams
    }
}

/// Appends a route attribute (`struct rtattr`: length, type, value).
fn attribute(body: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let length = 4 + value.len();
    body.extend_from_slice(&(length as u16).to_le_bytes());
    body.extend_from_slice(&kind.to_le_bytes());
    body.extend_from_slice(value);
    body.resize(body.len() + align(length) - length, 0);
}

fn name_bytes(name: &str) -> Vec<u8> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn answer(message: &[u8], replies: &mut Replies) {
    let kind = u16::from_le_bytes([message[4], message[5]]);
    let flags = u16::from_le_bytes([message[6], message[7]]);
    replies.sequence = u32::from_le_bytes(message[8..12].try_into().unwrap());
    if flags & NLM_F_REQUEST == 0 {
        return;
    }
    let body = &message[HEADER..];
    // The request's family, the first byte of every request body here.
    let family = body.first().copied().unwrap_or(AF_UNSPEC);
    let interfaces = net::interfaces();
    let dump = flags & NLM_F_DUMP != 0;
    let multi = if dump { NLM_F_MULTI } else { 0 };
    match kind {
        RTM_GETLINK => {
            // Without the dump flag, the interface the request names (by
            // index, at offset 4 of `struct ifinfomsg`).
            let wanted = (!dump && body.len() >= 8).then(|| i32::from_le_bytes(body[4..8].try_into().unwrap()));
            let mut found = false;
            for (position, interface) in interfaces.iter().enumerate() {
                let index = position as i32 + 1;
                if wanted.is_some_and(|wanted| wanted != index) {
                    continue;
                }
                found = true;
                replies.push(RTM_NEWLINK, multi, &link(interface, index));
            }
            if dump {
                replies.done();
            } else if !found {
                replies.error(Errno::ENODEV as i32, message);
            }
        }
        RTM_GETADDR => {
            if family == AF_UNSPEC || family == AF_INET {
                for (position, interface) in interfaces.iter().enumerate() {
                    if let Some(body) = address(interface, position as u32 + 1) {
                        replies.push(RTM_NEWADDR, multi, &body);
                    }
                }
            }
            replies.done();
        }
        RTM_GETROUTE => {
            if family == AF_UNSPEC || family == AF_INET {
                for (position, interface) in interfaces.iter().enumerate() {
                    for body in routes(interface, position as u32 + 1) {
                        replies.push(RTM_NEWROUTE, multi, &body);
                    }
                }
            }
            replies.done();
        }
        _ => {
            replies.error(Errno::EOPNOTSUPP as i32, message);
            return;
        }
    }
    if flags & NLM_F_ACK != 0 && !dump {
        replies.error(0, message);
    }
}

// Interface flags (`IFF_...`).
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const IFF_LOWER_UP: u32 = 0x10000;

/// An `RTM_NEWLINK` body: `struct ifinfomsg` and the attributes.
fn link(interface: &InterfaceInfo, index: i32) -> Vec<u8> {
    let loopback = interface.name == "lo";
    let mut flags = IFF_UP;
    if interface.up {
        flags |= IFF_RUNNING | IFF_LOWER_UP;
    }
    flags |= if loopback { IFF_LOOPBACK } else { IFF_BROADCAST | IFF_MULTICAST };
    // ARPHRD_LOOPBACK or ARPHRD_ETHER.
    let hardware: u16 = if loopback { 772 } else { 1 };
    let mut body = Vec::with_capacity(128);
    body.extend_from_slice(&[AF_UNSPEC, 0]);
    body.extend_from_slice(&hardware.to_le_bytes());
    body.extend_from_slice(&index.to_le_bytes());
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    attribute(&mut body, 3, &name_bytes(interface.name)); // IFLA_IFNAME
    attribute(&mut body, 1, &interface.mac); // IFLA_ADDRESS
    let broadcast = if loopback { [0; 6] } else { [0xFF; 6] };
    attribute(&mut body, 2, &broadcast); // IFLA_BROADCAST
    attribute(&mut body, 4, &interface.mtu.to_le_bytes()); // IFLA_MTU
    attribute(&mut body, 13, &1000u32.to_le_bytes()); // IFLA_TXQLEN
    // IFLA_OPERSTATE: IF_OPER_UP (6), IF_OPER_DOWN (2), or unknown (0)
    // for loopback, as Linux reports it.
    let state: u8 = if loopback {
        0
    } else if interface.up {
        6
    } else {
        2
    };
    attribute(&mut body, 16, &[state]);
    body
}

/// An `RTM_NEWADDR` body (`struct ifaddrmsg` and attributes), if the
/// interface has an address.
fn address(interface: &InterfaceInfo, index: u32) -> Option<Vec<u8>> {
    let config = interface.config.as_ref()?;
    let loopback = interface.name == "lo";
    // RT_SCOPE_HOST for loopback, RT_SCOPE_UNIVERSE otherwise; the
    // address is "permanent" (IFA_F_PERMANENT).
    let scope = if loopback { 254 } else { 0 };
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&[AF_INET, config.prefix, 0x80, scope]);
    body.extend_from_slice(&index.to_le_bytes());
    attribute(&mut body, 1, &config.address.0); // IFA_ADDRESS
    attribute(&mut body, 2, &config.address.0); // IFA_LOCAL
    if !loopback {
        let broadcast = net::Ipv4::from_u32(config.address.to_u32() | !config.netmask().to_u32());
        attribute(&mut body, 4, &broadcast.0); // IFA_BROADCAST
    }
    attribute(&mut body, 3, &name_bytes(interface.name)); // IFA_LABEL
    Some(body)
}

/// `RTM_NEWROUTE` bodies (`struct rtmsg` and attributes): the interface's
/// subnet, and the default route through its gateway.
fn routes(interface: &InterfaceInfo, index: u32) -> Vec<Vec<u8>> {
    let Some(config) = interface.config.as_ref() else { return Vec::new() };
    if interface.name == "lo" {
        return Vec::new();
    }
    // The main table (254), "boot" protocol for DHCP's (3), unicast (1).
    let route = |destination: Option<(net::Ipv4, u8)>, gateway: Option<net::Ipv4>| {
        let (address, prefix) = destination.unwrap_or((net::Ipv4::ANY, 0));
        // Link scope (253) for the subnet, universe (0) through a gateway.
        let scope = if gateway.is_some() { 0 } else { 253 };
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&[AF_INET, prefix, 0, 0, 254, 3, scope, 1]);
        body.extend_from_slice(&0u32.to_le_bytes());
        attribute(&mut body, 15, &254u32.to_le_bytes()); // RTA_TABLE
        if prefix > 0 {
            attribute(&mut body, 1, &address.0); // RTA_DST
        }
        if let Some(gateway) = gateway {
            attribute(&mut body, 5, &gateway.0); // RTA_GATEWAY
        } else {
            attribute(&mut body, 7, &config.address.0); // RTA_PREFSRC
        }
        attribute(&mut body, 4, &index.to_le_bytes()); // RTA_OIF
        body
    };
    let mut list = Vec::new();
    if let Some(gateway) = config.gateway {
        list.push(route(None, Some(gateway)));
    }
    let subnet = net::Ipv4::from_u32(config.address.to_u32() & config.netmask().to_u32());
    list.push(route(Some((subnet, config.prefix)), None));
    list
}

/// The file a netlink socket is.
pub struct NetlinkFile {
    pub socket: Arc<NetlinkSocket>,
}

impl Inode for NetlinkFile {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Socket, self.socket.inode, 0o777)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.socket.receive(buf, nonblocking, false).map(|(count, _)| count)
    }

    fn write_stream(&self, buf: &[u8], _nonblocking: bool) -> Result<usize> {
        self.socket.send(buf)
    }

    fn poll(&self, _flags: u32) -> u16 {
        if self.socket.available() > 0 { POLLIN | POLLOUT } else { POLLOUT }
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.socket.source.clone());
        true
    }

    fn on_close(&self, _flags: u32) {
        let port = self.socket.state.lock().port;
        PORTS.lock().remove(&port);
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        const FIONREAD: u64 = 0x541B;
        if request == FIONREAD {
            process::user::write_value(argument, &(self.socket.available() as i32))?;
            return Ok(0);
        }
        crate::syscall::interface_ioctl(request, argument)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request: header and body.
    fn request(kind: u16, flags: u16, sequence: u32, body: &[u8]) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
        message.extend_from_slice(&kind.to_le_bytes());
        message.extend_from_slice(&flags.to_le_bytes());
        message.extend_from_slice(&sequence.to_le_bytes());
        message.extend_from_slice(&0u32.to_le_bytes());
        message.extend_from_slice(body);
        message
    }

    /// Reads datagrams until `NLMSG_DONE`, returning the messages'
    /// (type, sequence, body).
    fn dump(socket: &NetlinkSocket) -> Vec<(u16, u32, Vec<u8>)> {
        let mut messages = Vec::new();
        let mut buf = alloc::vec![0u8; 8192];
        loop {
            let (count, _) = socket.receive(&mut buf, true, false).expect("a reply");
            let mut offset = 0;
            while offset + HEADER <= count {
                let length = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
                let kind = u16::from_le_bytes([buf[offset + 4], buf[offset + 5]]);
                let sequence = u32::from_le_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
                if kind == NLMSG_DONE {
                    return messages;
                }
                messages.push((kind, sequence, buf[offset + HEADER..offset + length].to_vec()));
                offset += align(length);
            }
        }
    }

    crate::ktest! {
        fn netlink_lists_links_and_addresses() {
            let socket = create(SOCK_RAW, NETLINK_ROUTE).unwrap();
            socket.send(&request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 7, &[0; 16])).unwrap();
            let links = dump(&socket);
            assert!(!links.is_empty());
            assert!(links.iter().all(|(kind, sequence, _)| *kind == RTM_NEWLINK && *sequence == 7));
            // The first is loopback, index 1, named "lo".
            let (_, _, first) = &links[0];
            assert_eq!(i32::from_le_bytes(first[4..8].try_into().unwrap()), 1);
            assert_eq!(&first[16 + 4..16 + 7], b"lo\0");

            socket.send(&request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 8, &[AF_INET, 0, 0, 0, 0, 0, 0, 0])).unwrap();
            let addresses = dump(&socket);
            let (_, _, loopback) = &addresses[0];
            assert_eq!(loopback[..2], [AF_INET, 8]);
            assert_eq!(&loopback[8 + 4..8 + 8], &[127, 0, 0, 1]);

            // Something unknown is an error, and nothing is left behind.
            let unknown = request(99, NLM_F_REQUEST, 9, &[]);
            socket.send(&unknown).unwrap();
            let mut buf = [0u8; 64];
            let (count, _) = socket.receive(&mut buf, true, false).unwrap();
            assert_eq!(count, HEADER + 4 + HEADER);
            assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), NLMSG_ERROR);
            assert_eq!(i32::from_le_bytes(buf[16..20].try_into().unwrap()), -(Errno::EOPNOTSUPP as i32));
            assert_eq!(socket.receive(&mut buf, true, false), Err(Errno::EAGAIN));
            NetlinkFile { socket }.on_close(0);
        }
    }
}
