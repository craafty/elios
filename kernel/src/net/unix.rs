//! Unix domain sockets (`AF_UNIX`): connections between programs on the
//! same machine, named by a path in the file system, by an "abstract" name
//! (a leading NUL byte; not in the file system), or by nothing at all
//! (`socketpair`). The Wayland protocol, D-Bus, and Firefox's processes
//! talking to each other all run over them.
//!
//! Three kinds: streams (bytes, like TCP), sequenced packets (connected,
//! with message boundaries) and datagrams (messages, connected or sent to
//! an address). Besides data they carry **file descriptors**
//! (`SCM_RIGHTS`): the sender's open files arrive in the receiver's file
//! table, which is how a Wayland client hands the compositor the shared
//! memory it draws into. Each side can also ask who the other is
//! (`SO_PEERCRED`).
//!
//! Each socket keeps a queue of messages it has received. A stream's
//! messages are read as one run of bytes, except that a read stops before
//! a message carrying files, so the files arrive with the bytes sent with
//! them, as on Linux. Sending blocks while the receiver's queue is full.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, OpenFile, POLLERR, POLLHUP, POLLIN, POLLOUT, PollSource, ramfs};
use crate::process::credentials::{Gid, Uid};
use crate::process::{self};
use crate::sync::SpinLock;

pub const AF_UNIX: u64 = 1;
const POLLRDHUP: u16 = 0x2000;

/// How much a socket's receive queue holds before senders wait.
const QUEUE_LIMIT: usize = 256 * 1024;
/// Most files one message can carry (Linux's `SCM_MAX_FD`).
pub const MAX_FILES_PER_MESSAGE: usize = 253;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stream,
    Datagram,
    SeqPacket,
}

impl Kind {
    pub fn from_type(kind: u64) -> Result<Self> {
        match kind {
            1 => Ok(Kind::Stream),
            2 => Ok(Kind::Datagram),
            5 => Ok(Kind::SeqPacket),
            _ => Err(Errno::ESOCKTNOSUPPORT),
        }
    }

    pub fn as_type(self) -> i32 {
        match self {
            Kind::Stream => 1,
            Kind::Datagram => 2,
            Kind::SeqPacket => 5,
        }
    }

    fn connected(self) -> bool {
        self != Kind::Datagram
    }
}

/// A socket's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Unnamed,
    Path(String),
    Abstract(Vec<u8>),
}

impl Address {
    /// Parses a `struct sockaddr_un` (family, then the path).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 2 || u16::from_le_bytes([bytes[0], bytes[1]]) != AF_UNIX as u16 {
            return Err(Errno::EINVAL);
        }
        let path = &bytes[2..];
        match path.first() {
            None => Ok(Address::Unnamed),
            Some(0) => Ok(Address::Abstract(Vec::from(&path[1..]))),
            Some(_) => {
                let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                String::from_utf8(Vec::from(&path[..end])).map(Address::Path).map_err(|_| Errno::EINVAL)
            }
        }
    }

    /// As a `struct sockaddr_un`, as long as it needs to be.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::from((AF_UNIX as u16).to_le_bytes());
        match self {
            Address::Unnamed => {}
            Address::Path(path) => {
                bytes.extend_from_slice(path.as_bytes());
                bytes.push(0);
            }
            Address::Abstract(name) => {
                bytes.push(0);
                bytes.extend_from_slice(name);
            }
        }
        bytes
    }
}

/// Who a process is, as `SO_PEERCRED` reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: Uid,
    pub gid: Gid,
}

impl PeerCredentials {
    fn current() -> Self {
        let credentials = process::credentials::current();
        let pid = crate::task::current_process().map_or(0, |process| process.pid);
        Self { pid, uid: credentials.euid, gid: credentials.egid }
    }

    pub fn bytes(self) -> [u8; 12] {
        let mut bytes = [0; 12];
        bytes[..4].copy_from_slice(&self.pid.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.uid.to_le_bytes());
        bytes[8..].copy_from_slice(&self.gid.to_le_bytes());
        bytes
    }
}

struct Message {
    data: Vec<u8>,
    /// How much of `data` has been read (streams read messages piecemeal).
    read: usize,
    files: Vec<Arc<OpenFile>>,
    from: Address,
    sender: PeerCredentials,
}

/// What a receive got.
pub struct Received {
    pub count: usize,
    /// The whole message was longer (datagrams and packets).
    pub truncated: bool,
    pub files: Vec<Arc<OpenFile>>,
    pub from: Address,
    pub sender: PeerCredentials,
}

#[derive(Default)]
struct State {
    address: Option<Address>,
    /// The other end of a connection (or a datagram socket's default
    /// destination).
    peer: Option<Weak<Socket>>,
    peer_address: Option<Address>,
    peer_credentials: Option<PeerCredentials>,
    connected: bool,
    listening: bool,
    backlog: VecDeque<Arc<Socket>>,
    backlog_limit: usize,
    queue: VecDeque<Message>,
    queued_bytes: usize,
    /// No more reading (shut down, or closed).
    read_shut: bool,
    /// No more writing.
    write_shut: bool,
    /// The peer won't send any more (it closed or shut down writing).
    peer_done: bool,
    closed: bool,
}

pub struct Socket {
    pub kind: Kind,
    state: SpinLock<State>,
    /// Who created it, for the peer's `SO_PEERCRED`.
    owner: PeerCredentials,
    pub pass_credentials: AtomicBool,
    /// Notified when its queue, backlog or connection changes: blocked
    /// calls on it wait here, as do `poll` and `epoll`.
    source: Arc<PollSource>,
}

/// Sockets bound to abstract names.
static ABSTRACT: SpinLock<BTreeMap<Vec<u8>, Weak<Socket>>> = SpinLock::new(BTreeMap::new());

impl Socket {
    pub fn new(kind: Kind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            state: SpinLock::new(State::default()),
            owner: PeerCredentials::current(),
            pass_credentials: AtomicBool::new(false),
            source: Arc::new(PollSource::new()),
        })
    }

    /// Two sockets connected to each other (`socketpair`).
    pub fn pair(kind: Kind) -> (Arc<Self>, Arc<Self>) {
        let (one, two) = (Self::new(kind), Self::new(kind));
        for (me, other) in [(&one, &two), (&two, &one)] {
            let mut state = me.state.lock();
            state.peer = Some(Arc::downgrade(other));
            state.peer_address = Some(Address::Unnamed);
            state.peer_credentials = Some(other.owner);
            state.connected = true;
            state.address = Some(Address::Unnamed);
        }
        (one, two)
    }

    pub fn local_address(&self) -> Address {
        self.state.lock().address.clone().unwrap_or(Address::Unnamed)
    }

    pub fn peer_address(&self) -> Result<Address> {
        self.state.lock().peer_address.clone().ok_or(Errno::ENOTCONN)
    }

    pub fn peer_credentials(&self) -> Option<PeerCredentials> {
        self.state.lock().peer_credentials
    }

    pub fn is_listening(&self) -> bool {
        self.state.lock().listening
    }

    /// Bytes waiting to be read (`FIONREAD`).
    pub fn available(&self) -> usize {
        let state = self.state.lock();
        match self.kind {
            Kind::Stream => state.queue.iter().map(|message| message.data.len() - message.read).sum(),
            _ => state.queue.front().map_or(0, |message| message.data.len()),
        }
    }

    pub fn bind(self: &Arc<Self>, address: Address) -> Result<()> {
        if self.state.lock().address.is_some() {
            return Err(Errno::EINVAL);
        }
        match &address {
            Address::Unnamed => return Err(Errno::EINVAL),
            Address::Abstract(name) => {
                let mut names = ABSTRACT.lock();
                names.retain(|_, socket| socket.strong_count() > 0);
                if names.contains_key(name) {
                    return Err(Errno::EADDRINUSE);
                }
                names.insert(name.clone(), Arc::downgrade(self));
            }
            Address::Path(path) => {
                let cwd = process::current().cwd.lock().clone();
                let (parent, name) = fs::lookup_parent(&cwd, path)?;
                fs::check_writable_directory(&parent)?;
                if parent.lookup(&name).is_ok() {
                    return Err(Errno::EADDRINUSE);
                }
                let attributes = fs::new_file_attributes(&parent, FileKind::Socket, 0o777);
                let node = Arc::new(SocketNode {
                    inode: ramfs::next_inode_number(),
                    attributes: SpinLock::new(attributes),
                    socket: Arc::downgrade(self),
                });
                // Only in-memory directories can hold one (where they're
                // made in practice: /tmp, /run).
                parent
                    .link(&name, node)
                    .map_err(|error| if error == Errno::EPERM { Errno::EOPNOTSUPP } else { error })?;
            }
        }
        self.state.lock().address = Some(address);
        Ok(())
    }

    pub fn listen(&self, backlog: usize) -> Result<()> {
        if !self.kind.connected() {
            return Err(Errno::EOPNOTSUPP);
        }
        let mut state = self.state.lock();
        if state.connected {
            return Err(Errno::EINVAL);
        }
        state.listening = true;
        state.backlog_limit = backlog.clamp(1, 4096);
        Ok(())
    }

    /// The socket bound to `address`.
    fn find(address: &Address) -> Result<Arc<Socket>> {
        match address {
            Address::Unnamed => Err(Errno::EINVAL),
            Address::Abstract(name) => ABSTRACT.lock().get(name).and_then(Weak::upgrade).ok_or(Errno::ECONNREFUSED),
            Address::Path(path) => {
                let cwd = process::current().cwd.lock().clone();
                let inode = fs::lookup(&cwd, path, true)?;
                fs::check(&inode.metadata(), fs::MAY_WRITE)?;
                let node =
                    inode.as_any().and_then(|any| any.downcast_ref::<SocketNode>()).ok_or(Errno::ECONNREFUSED)?;
                node.socket.upgrade().ok_or(Errno::ECONNREFUSED)
            }
        }
    }

    pub fn connect(self: &Arc<Self>, address: &Address, nonblocking: bool) -> Result<()> {
        let target = Self::find(address)?;
        if target.kind != self.kind {
            return Err(Errno::EPROTOTYPE);
        }
        if !self.kind.connected() {
            // A datagram socket's default destination.
            let mut state = self.state.lock();
            state.peer = Some(Arc::downgrade(&target));
            state.peer_address = Some(address.clone());
            state.peer_credentials = Some(target.owner);
            return Ok(());
        }
        if self.state.lock().connected {
            return Err(Errno::EISCONN);
        }
        // The listener's end of the new connection, which `accept` hands out.
        let server = Arc::new(Socket {
            kind: self.kind,
            state: SpinLock::new(State::default()),
            owner: target.owner,
            pass_credentials: AtomicBool::new(target.pass_credentials.load(Ordering::Relaxed)),
            source: Arc::new(PollSource::new()),
        });
        let me = PeerCredentials::current();
        {
            let mut state = server.state.lock();
            state.address = Some(address.clone());
            state.peer = Some(Arc::downgrade(self));
            state.peer_address = Some(self.local_address());
            state.peer_credentials = Some(me);
            state.connected = true;
        }
        target.source.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut listener = target.state.lock();
            if !listener.listening || listener.closed {
                return Some(Err(Errno::ECONNREFUSED));
            }
            if listener.backlog.len() >= listener.backlog_limit {
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            listener.backlog.push_back(server.clone());
            Some(Ok(()))
        })?;
        {
            let mut state = self.state.lock();
            state.peer = Some(Arc::downgrade(&server));
            state.peer_address = Some(address.clone());
            state.peer_credentials = Some(target.owner);
            state.connected = true;
        }
        target.source.notify();
        self.source.notify();
        Ok(())
    }

    pub fn accept(&self, nonblocking: bool) -> Result<Arc<Socket>> {
        let connection = self.source.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut state = self.state.lock();
            if !state.listening {
                return Some(Err(Errno::EINVAL));
            }
            match state.backlog.pop_front() {
                Some(connection) => Some(Ok(connection)),
                None => nonblocking.then_some(Err(Errno::EAGAIN)),
            }
        })?;
        // Room in the backlog for connections waiting to get in.
        self.source.notify();
        Ok(connection)
    }

    /// Wakes whoever waits on this socket or its peer (whose readiness
    /// depends on this one's queue).
    fn notify_both(&self) {
        self.source.notify();
        let peer = self.state.lock().peer.as_ref().and_then(Weak::upgrade);
        if let Some(peer) = peer {
            peer.source.notify();
        }
    }

    /// Sends `data` (with `files` attached to its first byte) to the peer,
    /// or to `destination` (datagrams). Returns how much was sent.
    pub fn send(
        &self,
        data: &[u8],
        mut files: Vec<Arc<OpenFile>>,
        destination: Option<&Address>,
        nonblocking: bool,
    ) -> Result<usize> {
        let target = match destination {
            Some(address) if self.kind == Kind::Datagram => Self::find(address)?,
            Some(_) if self.state.lock().connected => return Err(Errno::EISCONN),
            _ => {
                let state = self.state.lock();
                if state.write_shut {
                    return Err(Errno::EPIPE);
                }
                match state.peer.as_ref().map(Weak::upgrade) {
                    Some(Some(peer)) => peer,
                    Some(None) => {
                        return Err(if self.kind == Kind::Datagram { Errno::ECONNREFUSED } else { Errno::EPIPE });
                    }
                    None => return Err(if self.kind.connected() { Errno::ENOTCONN } else { Errno::EDESTADDRREQ }),
                }
            }
        };
        if self.kind != Kind::Stream && data.len() > QUEUE_LIMIT {
            return Err(Errno::EMSGSIZE);
        }
        let from = self.local_address();
        let sender = PeerCredentials::current();
        let mut sent = 0;
        loop {
            let progress = target.source.queue().wait_until(|| {
                if process::signal_pending() {
                    return Some(Err(Errno::EINTR));
                }
                let mut state = target.state.lock();
                if state.closed || state.read_shut {
                    return Some(Err(if self.kind == Kind::Datagram { Errno::ECONNREFUSED } else { Errno::EPIPE }));
                }
                let room = QUEUE_LIMIT.saturating_sub(state.queued_bytes);
                let wanted = data.len() - sent;
                let fits = if self.kind == Kind::Stream { room > 0 || wanted == 0 } else { room >= wanted };
                if !fits {
                    return nonblocking.then_some(Err(Errno::EAGAIN));
                }
                let chunk = if self.kind == Kind::Stream { wanted.min(room) } else { wanted };
                state.queue.push_back(Message {
                    data: Vec::from(&data[sent..sent + chunk]),
                    read: 0,
                    files: core::mem::take(&mut files),
                    from: from.clone(),
                    sender,
                });
                state.queued_bytes += chunk;
                Some(Ok(chunk))
            });
            match progress {
                Ok(chunk) => {
                    sent += chunk;
                    target.source.notify();
                    if sent == data.len() {
                        return Ok(sent);
                    }
                }
                // Part of a stream went: report that.
                Err(Errno::EAGAIN | Errno::EINTR) if sent > 0 => return Ok(sent),
                Err(error) => return Err(error),
            }
        }
    }

    /// Receives into `buf`. A stream reads as much as is there (stopping
    /// before a message that carries files); the others read one message.
    pub fn receive(&self, buf: &mut [u8], nonblocking: bool, peek: bool) -> Result<Received> {
        let received = self.source.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut state = self.state.lock();
            if state.queue.is_empty() {
                if state.read_shut || state.peer_done {
                    return Some(Ok(Received {
                        count: 0,
                        truncated: false,
                        files: Vec::new(),
                        from: Address::Unnamed,
                        sender: PeerCredentials::default(),
                    }));
                }
                if self.kind.connected() && !state.connected {
                    return Some(Err(Errno::ENOTCONN));
                }
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            Some(Ok(match self.kind {
                Kind::Stream => Self::take_stream(&mut state, buf, peek),
                _ => Self::take_message(&mut state, buf, peek),
            }))
        })?;
        // Room for senders, who wait on this socket and poll their own.
        self.notify_both();
        Ok(received)
    }

    fn take_stream(state: &mut State, buf: &mut [u8], peek: bool) -> Received {
        let first = state.queue.front().expect("checked by the caller");
        let mut received =
            Received { count: 0, truncated: false, files: Vec::new(), from: first.from.clone(), sender: first.sender };
        let mut index = 0;
        while received.count < buf.len() && index < state.queue.len() {
            let message = &mut state.queue[index];
            if message.read == 0 && !message.files.is_empty() {
                if received.count > 0 {
                    break;
                }
                if !peek {
                    received.files = core::mem::take(&mut message.files);
                }
            }
            let chunk = (message.data.len() - message.read).min(buf.len() - received.count);
            buf[received.count..received.count + chunk]
                .copy_from_slice(&message.data[message.read..message.read + chunk]);
            received.count += chunk;
            if peek {
                index += 1;
                continue;
            }
            message.read += chunk;
            if message.read == message.data.len() {
                state.queue.pop_front();
            } else {
                break;
            }
        }
        if !peek {
            state.queued_bytes -= received.count;
        }
        received
    }

    fn take_message(state: &mut State, buf: &mut [u8], peek: bool) -> Received {
        let message = if peek {
            let front = state.queue.front().expect("checked by the caller");
            Message {
                data: front.data.clone(),
                read: 0,
                files: Vec::new(),
                from: front.from.clone(),
                sender: front.sender,
            }
        } else {
            let message = state.queue.pop_front().expect("checked by the caller");
            state.queued_bytes -= message.data.len();
            message
        };
        let count = message.data.len().min(buf.len());
        buf[..count].copy_from_slice(&message.data[..count]);
        Received {
            count,
            truncated: message.data.len() > buf.len(),
            files: message.files,
            from: message.from,
            sender: message.sender,
        }
    }

    pub fn shutdown(&self, how: u64) -> Result<()> {
        let (read, write) = match how {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => return Err(Errno::EINVAL),
        };
        let peer = {
            let mut state = self.state.lock();
            if self.kind.connected() && !state.connected {
                return Err(Errno::ENOTCONN);
            }
            state.read_shut |= read;
            state.write_shut |= write;
            state.peer.as_ref().and_then(Weak::upgrade)
        };
        if write && let Some(peer) = &peer {
            peer.state.lock().peer_done = true;
            peer.source.notify();
        }
        self.source.notify();
        Ok(())
    }

    /// The last descriptor was closed: the peer sees end of file, and files
    /// in flight to us are dropped.
    fn close(&self) {
        let (peer, pending, queue) = {
            let mut state = self.state.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.read_shut = true;
            state.write_shut = true;
            state.listening = false;
            (
                state.peer.take().and_then(|peer| peer.upgrade()),
                core::mem::take(&mut state.backlog),
                core::mem::take(&mut state.queue),
            )
        };
        if let Some(peer) = peer {
            peer.state.lock().peer_done = true;
            peer.source.notify();
        }
        // Not-yet-accepted connections go too (outside the lock: dropping
        // them closes them).
        drop(pending);
        drop(queue);
        self.source.notify();
    }

    pub fn poll(&self) -> u16 {
        let state = self.state.lock();
        if state.listening {
            return if state.backlog.is_empty() { 0 } else { POLLIN };
        }
        let mut ready = 0;
        if !state.queue.is_empty() || state.read_shut || state.peer_done {
            ready |= POLLIN;
        }
        if state.peer_done {
            ready |= POLLRDHUP;
        }
        if self.kind.connected() && !state.connected {
            // Not connected: nothing to wait for (Linux says the same).
            return ready | POLLOUT | POLLHUP;
        }
        if state.peer_done && state.write_shut || state.closed {
            ready |= POLLHUP;
        }
        let peer = state.peer.as_ref().map(Weak::upgrade);
        drop(state);
        match peer {
            Some(Some(peer)) => {
                let peer_state = peer.state.lock();
                if peer_state.closed {
                    ready |= POLLHUP | POLLERR;
                } else if peer_state.queued_bytes < QUEUE_LIMIT {
                    ready |= POLLOUT;
                }
            }
            Some(None) if self.kind.connected() => ready |= POLLHUP,
            // Unconnected datagram sockets can always try to send.
            _ => ready |= POLLOUT,
        }
        ready
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.close();
    }
}

/// A socket as an open file.
pub struct SocketFile {
    inode: u64,
    pub socket: Arc<Socket>,
}

impl SocketFile {
    pub fn new(socket: Arc<Socket>) -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number(), socket })
    }
}

impl Inode for SocketFile {
    fn metadata(&self) -> Metadata {
        Metadata::virtual_file(FileKind::Socket, self.inode, 0o777)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.socket.receive(buf, nonblocking, false).map(|received| received.count)
    }

    fn write_stream(&self, buf: &[u8], nonblocking: bool) -> Result<usize> {
        match self.socket.send(buf, Vec::new(), None, nonblocking) {
            Err(Errno::EPIPE) => {
                process::send_signal_to_self(process::SIGPIPE);
                Err(Errno::EPIPE)
            }
            result => result,
        }
    }

    fn poll(&self, _flags: u32) -> u16 {
        self.socket.poll()
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.socket.source.clone());
        true
    }

    fn on_close(&self, _flags: u32) {
        self.socket.close();
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        const FIONREAD: u64 = 0x541B;
        if request == FIONREAD {
            process::user::write_value(argument, &(self.socket.available() as i32))?;
            return Ok(0);
        }
        Err(Errno::ENOTTY)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

/// The file a socket bound to a path leaves in its directory, through
/// which `connect` finds it.
pub struct SocketNode {
    inode: u64,
    attributes: SpinLock<fs::Attributes>,
    socket: Weak<Socket>,
}

impl Inode for SocketNode {
    fn metadata(&self) -> Metadata {
        let attributes = *self.attributes.lock();
        Metadata {
            uid: attributes.uid,
            gid: attributes.gid,
            ..Metadata::virtual_file(FileKind::Socket, self.inode, attributes.permissions)
        }
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.attributes.lock().permissions = permissions & 0o7777;
        Ok(())
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        let mut attributes = self.attributes.lock();
        attributes.uid = uid;
        attributes.gid = gid;
        Ok(())
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        // Sockets are connected to, not opened.
        Some(Err(Errno::ENXIO))
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}
