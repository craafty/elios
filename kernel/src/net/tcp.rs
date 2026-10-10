//! TCP: reliable, ordered byte streams over IP (RFC 9293).
//!
//! Each byte sent has a 32-bit sequence number. The sender keeps what it
//! sent until the receiver acknowledges it, and sends it again if no
//! acknowledgment comes in time; the receiver puts bytes back in order and
//! says how much more it can take (its window). Opening takes a three-way
//! handshake (SYN, SYN-ACK, ACK); closing, a FIN from each side.
//!
//! Choices made for simplicity:
//! - Segments that arrive out of order are dropped, and the sender's
//!   retransmission fills the gap.
//! - Retransmission is "go back N": on a timeout, everything unacknowledged
//!   is sent again. The timeout starts at one second and doubles on each
//!   retry; there's no round-trip time estimation or congestion control,
//!   which matter on busy networks more than on a LAN.
//! - Acknowledgments go out immediately, not delayed.
//! - No window scaling, so windows are at most 64 KiB.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::socket::{Endpoint, Socket, SocketId};
use super::{Ipv4, PROTOCOL_TCP, Stack, checksum, pseudo_header_sum, read_u16, read_u32};
use crate::errno::Errno;
use crate::time;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

const RECEIVE_CAPACITY: usize = 64 * 1024;
const SEND_CAPACITY: usize = 64 * 1024;
/// The segment size we accept: an Ethernet frame's 1500 bytes, less the IP
/// and TCP headers.
const OUR_MSS: u16 = 1460;
/// The segment size to assume if the other side doesn't say.
const DEFAULT_MSS: usize = 536;
const INITIAL_RTO_MS: u64 = 1000;
const MAX_RTO_MS: u64 = 30_000;
const MAX_RETRIES: u32 = 8;
const SYN_RETRIES: u32 = 5;
/// How long a closed connection lingers to catch stray segments. RFC 9293
/// says two minutes; this is short, so ports free up quickly.
const TIME_WAIT_MS: u64 = 2000;
/// How long a connection nobody holds may wait for the other side's FIN.
const ORPHAN_TIMEOUT_MS: u64 = 60_000;

/// Sequence number comparisons, which wrap around.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn after(a: u32, b: u32) -> bool {
    before(b, a)
}

pub struct Connection {
    pub state: State,
    pub local: Endpoint,
    pub remote: Endpoint,
    /// Whether `local` has been chosen (by `bind`, `connect` or `listen`).
    pub bound: bool,
    /// For listening sockets: established connections not yet accepted.
    pub accept_queue: VecDeque<SocketId>,
    backlog: usize,
    /// For connections made to a listening socket, until accepted.
    pub parent: Option<SocketId>,

    /// Our initial sequence number.
    iss: u32,
    /// The oldest unacknowledged sequence number, and the next to send.
    send_unacknowledged: u32,
    send_next: u32,
    /// How much the other side can take, from `send_unacknowledged`.
    send_window: u32,
    /// The sequence number of `send_buffer`'s first byte.
    send_base: u32,
    /// Bytes not yet acknowledged, sent or not.
    send_buffer: VecDeque<u8>,
    /// The program is done writing: a FIN follows the data.
    fin_queued: bool,
    fin_sent: bool,

    /// The next sequence number expected.
    receive_next: u32,
    /// Bytes received in order, not yet read by the program.
    receive_buffer: VecDeque<u8>,
    fin_received: bool,
    read_shutdown: bool,
    /// The window in the last segment we sent.
    advertised: u32,
    /// The largest segment the other side accepts.
    mss: usize,

    rto: u64,
    retransmit_at: Option<u64>,
    retries: u32,
    /// When to give up waiting (TIME-WAIT, or an orphan waiting for FIN).
    close_at: Option<u64>,

    /// Why the connection failed, for the next call to report.
    pub error: Option<Errno>,
    /// No file refers to the connection any more; it's removed once it's
    /// fully closed.
    pub orphan: bool,
}

/// A TCP segment as received.
struct Segment<'a> {
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    acknowledgment: u32,
    flags: u8,
    window: u32,
    mss: Option<u16>,
    data: &'a [u8],
}

impl Segment<'_> {
    /// Sequence space used: the data, plus one each for SYN and FIN.
    fn length(&self) -> u32 {
        self.data.len() as u32 + u32::from(self.flags & SYN != 0) + u32::from(self.flags & FIN != 0)
    }
}

fn parse(source: Ipv4, destination: Ipv4, bytes: &[u8]) -> Option<Segment<'_>> {
    if bytes.len() < 20 {
        return None;
    }
    let header_length = usize::from(bytes[12] >> 4) * 4;
    if header_length < 20 || header_length > bytes.len() {
        return None;
    }
    if checksum(bytes, pseudo_header_sum(source, destination, PROTOCOL_TCP, bytes.len())) != 0 {
        return None;
    }
    // Options: we only care about the maximum segment size (kind 2).
    let mut mss = None;
    let mut offset = 20;
    while offset < header_length {
        match bytes[offset] {
            0 => break,
            1 => offset += 1,
            kind => {
                let length = usize::from(*bytes.get(offset + 1)?);
                if length < 2 || offset + length > header_length {
                    break;
                }
                if kind == 2 && length == 4 {
                    mss = Some(read_u16(bytes, offset + 2));
                }
                offset += length;
            }
        }
    }
    Some(Segment {
        source_port: read_u16(bytes, 0),
        destination_port: read_u16(bytes, 2),
        sequence: read_u32(bytes, 4),
        acknowledgment: read_u32(bytes, 8),
        flags: bytes[13],
        window: u32::from(read_u16(bytes, 14)),
        mss,
        data: &bytes[header_length..],
    })
}

/// Builds a segment, checksum included. SYN segments carry our maximum
/// segment size.
fn build(
    local: Endpoint,
    remote: Endpoint,
    sequence: u32,
    acknowledgment: u32,
    flags: u8,
    window: u32,
    data: &[u8],
) -> Vec<u8> {
    let mss = flags & SYN != 0;
    let header_length = if mss { 24 } else { 20 };
    let mut segment = Vec::with_capacity(header_length + data.len());
    segment.extend_from_slice(&local.port.to_be_bytes());
    segment.extend_from_slice(&remote.port.to_be_bytes());
    segment.extend_from_slice(&sequence.to_be_bytes());
    segment.extend_from_slice(&acknowledgment.to_be_bytes());
    segment.push((header_length as u8 / 4) << 4);
    segment.push(flags);
    segment.extend_from_slice(&(window.min(0xFFFF) as u16).to_be_bytes());
    segment.extend_from_slice(&[0, 0, 0, 0]);
    if mss {
        segment.extend_from_slice(&[2, 4]);
        segment.extend_from_slice(&OUR_MSS.to_be_bytes());
    }
    segment.extend_from_slice(data);
    let sum = checksum(&segment, pseudo_header_sum(local.address, remote.address, PROTOCOL_TCP, segment.len()));
    segment[16..18].copy_from_slice(&sum.to_be_bytes());
    segment
}

fn random_u32() -> u32 {
    let mut bytes = [0; 4];
    crate::random::fill(&mut bytes);
    u32::from_le_bytes(bytes)
}

impl Connection {
    pub fn new() -> Self {
        let iss = random_u32();
        Self {
            state: State::Closed,
            local: Endpoint::default(),
            remote: Endpoint::default(),
            bound: false,
            accept_queue: VecDeque::new(),
            backlog: 0,
            parent: None,
            iss,
            send_unacknowledged: iss,
            send_next: iss,
            send_window: 0,
            send_base: iss.wrapping_add(1),
            send_buffer: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            receive_next: 0,
            receive_buffer: VecDeque::new(),
            fin_received: false,
            read_shutdown: false,
            advertised: 0,
            mss: DEFAULT_MSS,
            rto: INITIAL_RTO_MS,
            retransmit_at: None,
            retries: 0,
            close_at: None,
            error: None,
            orphan: false,
        }
    }

    /// Starts an active open (`connect`): the SYN goes out with the next
    /// output.
    pub fn open(&mut self, remote: Endpoint, _now: u64) {
        self.remote = remote;
        self.state = State::SynSent;
    }

    pub fn listen(&mut self, backlog: usize) {
        self.state = State::Listen;
        self.backlog = backlog.clamp(1, 128);
    }

    pub fn connected(&self) -> bool {
        !matches!(self.state, State::Closed | State::Listen | State::SynSent)
    }

    pub fn write_shutdown(&self) -> bool {
        self.fin_queued
    }

    pub fn shutdown_read(&mut self) {
        self.read_shutdown = true;
        self.receive_buffer.clear();
    }

    /// Stops sending: a FIN follows whatever is queued.
    pub fn shutdown_write(&mut self) {
        if self.fin_queued {
            return;
        }
        let next = match self.state {
            State::SynReceived | State::Established => State::FinWait1,
            State::CloseWait => State::LastAck,
            _ => return,
        };
        self.state = next;
        self.fin_queued = true;
    }

    /// Queues as much of `data` as fits; returns how much.
    pub fn queue_data(&mut self, data: &[u8]) -> usize {
        let count = data.len().min(SEND_CAPACITY - self.send_buffer.len());
        self.send_buffer.extend(&data[..count]);
        count
    }

    pub fn read(&mut self, buf: &mut [u8], peek: bool) -> usize {
        let count = buf.len().min(self.receive_buffer.len());
        for (slot, byte) in buf.iter_mut().zip(self.receive_buffer.iter()) {
            *slot = *byte;
        }
        if !peek {
            self.receive_buffer.drain(..count);
        }
        count
    }

    pub fn available(&self) -> usize {
        self.receive_buffer.len()
    }

    /// Bytes waiting to be acknowledged, and to be read.
    pub fn queued(&self) -> (usize, usize) {
        (self.send_buffer.len(), self.receive_buffer.len())
    }

    /// Nothing more will arrive to read.
    pub fn at_end(&self) -> bool {
        self.fin_received || self.read_shutdown || (self.state == State::Closed && self.connected_once())
    }

    fn connected_once(&self) -> bool {
        self.receive_next != 0 || after(self.send_unacknowledged, self.iss)
    }

    pub fn readable(&self) -> bool {
        match self.state {
            State::Listen => !self.accept_queue.is_empty(),
            State::SynSent => self.error.is_some(),
            _ => {
                !self.receive_buffer.is_empty() || self.at_end() || self.error.is_some() || self.state == State::Closed
            }
        }
    }

    pub fn writable(&self) -> bool {
        matches!(self.state, State::Established | State::CloseWait)
            && !self.fin_queued
            && self.send_buffer.len() < SEND_CAPACITY
    }

    pub fn hung_up(&self) -> bool {
        (self.state == State::Closed && self.connected_once())
            || matches!(self.state, State::TimeWait)
            || (self.fin_received && self.fin_queued)
    }

    pub fn next_deadline(&self) -> Option<u64> {
        [self.retransmit_at, self.close_at].into_iter().flatten().min()
    }

    fn window(&self) -> u32 {
        (RECEIVE_CAPACITY - self.receive_buffer.len()) as u32
    }

    fn segment(&mut self, sequence: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let window = self.window();
        self.advertised = window;
        let acknowledgment = if flags & ACK != 0 { self.receive_next } else { 0 };
        build(self.local, self.remote, sequence, acknowledgment, flags, window, data)
    }

    fn ack(&mut self) -> Vec<u8> {
        self.segment(self.send_next, ACK, &[])
    }

    /// The sequence number of our FIN.
    fn fin_sequence(&self) -> u32 {
        self.send_base.wrapping_add(self.send_buffer.len() as u32)
    }

    /// Segments to send now: the SYN, data the window allows, the FIN.
    fn output(&mut self, now: u64, probe: bool) -> Vec<Vec<u8>> {
        let mut segments = Vec::new();
        match self.state {
            State::SynSent if self.send_next == self.iss => {
                let syn = self.segment(self.iss, SYN, &[]);
                segments.push(syn);
                self.send_next = self.iss.wrapping_add(1);
            }
            State::SynReceived if self.send_next == self.iss => {
                let syn = self.segment(self.iss, SYN | ACK, &[]);
                segments.push(syn);
                self.send_next = self.iss.wrapping_add(1);
            }
            State::Established | State::CloseWait | State::FinWait1 | State::Closing | State::LastAck => {
                let end = self.fin_sequence();
                // A window probe sends one byte into a closed window.
                let window = if probe { self.send_window.max(1) } else { self.send_window };
                let window_end = self.send_unacknowledged.wrapping_add(window);
                while before(self.send_next, end) && before(self.send_next, window_end) {
                    let offset = self.send_next.wrapping_sub(self.send_base) as usize;
                    let size = (end.wrapping_sub(self.send_next) as usize)
                        .min(window_end.wrapping_sub(self.send_next) as usize)
                        .min(self.mss);
                    let last = offset + size == self.send_buffer.len();
                    let flags = ACK | if last { PSH } else { 0 };
                    let sequence = self.send_next;
                    let data: Vec<u8> = self.send_buffer.range(offset..offset + size).copied().collect();
                    let segment = self.segment(sequence, flags, &data);
                    segments.push(segment);
                    self.send_next = self.send_next.wrapping_add(size as u32);
                }
                if self.fin_queued && !self.fin_sent && self.send_next == end {
                    let fin = self.segment(end, FIN | ACK, &[]);
                    segments.push(fin);
                    self.send_next = end.wrapping_add(1);
                    self.fin_sent = true;
                }
            }
            _ => {}
        }
        let waiting = self.send_unacknowledged != self.send_next
            || (self.send_window == 0 && !self.send_buffer.is_empty() && self.connected());
        if waiting && self.retransmit_at.is_none() {
            self.retransmit_at = Some(now + self.rto);
        }
        segments
    }

    /// The retransmission timer fired: send everything unacknowledged again.
    fn retransmit(&mut self, now: u64) -> Vec<Vec<u8>> {
        self.retransmit_at = None;
        self.retries += 1;
        let limit = if matches!(self.state, State::SynSent | State::SynReceived) { SYN_RETRIES } else { MAX_RETRIES };
        if self.retries > limit {
            self.fail(Errno::ETIMEDOUT);
            return Vec::new();
        }
        self.rto = (self.rto * 2).min(MAX_RTO_MS);
        let probe = self.send_window == 0 && self.send_unacknowledged == self.send_next;
        self.send_next = self.send_unacknowledged;
        if self.fin_sent && !after(self.send_unacknowledged, self.fin_sequence()) {
            self.fin_sent = false;
        }
        self.output(now, probe)
    }

    fn fail(&mut self, error: Errno) {
        self.error = Some(error);
        self.state = State::Closed;
        self.retransmit_at = None;
        self.close_at = None;
    }

    /// Handles a segment for this connection (not a listening socket).
    /// Returns segments to send, and whether the connection just became
    /// established (for the listener's accept queue).
    fn on_segment(&mut self, segment: &Segment, now: u64) -> (Vec<Vec<u8>>, bool) {
        let mut out = Vec::new();
        if self.state == State::SynSent {
            let acceptable = segment.flags & ACK != 0 && segment.acknowledgment == self.iss.wrapping_add(1);
            if segment.flags & ACK != 0 && !acceptable {
                if segment.flags & RST == 0 {
                    out.push(build(self.local, self.remote, segment.acknowledgment, 0, RST, 0, &[]));
                }
                return (out, false);
            }
            if segment.flags & RST != 0 {
                if acceptable {
                    self.fail(Errno::ECONNREFUSED);
                }
                return (out, false);
            }
            if segment.flags & SYN != 0 && acceptable {
                self.receive_next = segment.sequence.wrapping_add(1);
                self.send_unacknowledged = segment.acknowledgment;
                self.send_window = segment.window;
                self.mss = segment.mss.map_or(DEFAULT_MSS, usize::from).clamp(64, usize::from(OUR_MSS));
                self.state = State::Established;
                self.retransmit_at = None;
                self.retries = 0;
                self.rto = INITIAL_RTO_MS;
                out.push(self.ack());
                out.extend(self.output(now, false));
            }
            return (out, false);
        }

        // Is the segment within our receive window?
        let window = self.window().max(1);
        let in_window = |sequence: u32| {
            !before(sequence, self.receive_next) && before(sequence, self.receive_next.wrapping_add(window))
        };
        let length = segment.length();
        let acceptable = if length == 0 {
            segment.sequence == self.receive_next || in_window(segment.sequence)
        } else {
            in_window(segment.sequence) || in_window(segment.sequence.wrapping_add(length - 1))
        };
        if !acceptable {
            if segment.flags & RST == 0 {
                out.push(self.ack());
            }
            return (out, false);
        }

        if segment.flags & RST != 0 {
            match self.state {
                State::SynReceived if self.parent.is_some() => self.state = State::Closed,
                State::Closing | State::LastAck | State::TimeWait => self.state = State::Closed,
                _ => self.fail(Errno::ECONNRESET),
            }
            return (out, false);
        }
        if segment.flags & SYN != 0 {
            // A SYN in the window of a synchronized connection is an error.
            out.push(build(self.local, self.remote, self.send_next, 0, RST, 0, &[]));
            self.fail(Errno::ECONNRESET);
            return (out, false);
        }
        if segment.flags & ACK == 0 {
            return (out, false);
        }

        let mut established = false;
        if self.state == State::SynReceived {
            if after(segment.acknowledgment, self.send_unacknowledged) && !after(segment.acknowledgment, self.send_next)
            {
                self.state = State::Established;
                self.send_window = segment.window;
                established = true;
            } else {
                out.push(build(self.local, self.remote, segment.acknowledgment, 0, RST, 0, &[]));
                return (out, false);
            }
        }

        // Acknowledgment of what we sent.
        if after(segment.acknowledgment, self.send_next) {
            out.push(self.ack());
            return (out, established);
        }
        if after(segment.acknowledgment, self.send_unacknowledged) {
            let acked_data = segment.acknowledgment.wrapping_sub(self.send_base).min(self.send_buffer.len() as u32);
            if after(segment.acknowledgment, self.send_base) {
                self.send_buffer.drain(..acked_data as usize);
                self.send_base = self.send_base.wrapping_add(acked_data);
            }
            self.send_unacknowledged = segment.acknowledgment;
            self.retries = 0;
            self.rto = INITIAL_RTO_MS;
            self.retransmit_at = None;
        }
        self.send_window = segment.window;
        let fin_acknowledged = self.fin_sent && segment.acknowledgment == self.fin_sequence().wrapping_add(1);
        match self.state {
            State::FinWait1 if fin_acknowledged => {
                self.state = State::FinWait2;
                if self.orphan {
                    self.close_at = Some(now + ORPHAN_TIMEOUT_MS);
                }
            }
            State::Closing if fin_acknowledged => {
                self.state = State::TimeWait;
                self.close_at = Some(now + TIME_WAIT_MS);
            }
            State::LastAck if fin_acknowledged => {
                self.state = State::Closed;
                return (out, established);
            }
            _ => {}
        }

        // Data, in order only.
        let mut need_ack = false;
        let mut fin_reached = segment.flags & FIN != 0;
        if !segment.data.is_empty() && matches!(self.state, State::Established | State::FinWait1 | State::FinWait2) {
            let skip = self.receive_next.wrapping_sub(segment.sequence) as usize;
            if !after(segment.sequence, self.receive_next) && skip < segment.data.len() {
                let data = &segment.data[skip..];
                let room = RECEIVE_CAPACITY - self.receive_buffer.len();
                let taken = data.len().min(room);
                if !self.read_shutdown {
                    self.receive_buffer.extend(&data[..taken]);
                }
                self.receive_next = self.receive_next.wrapping_add(taken as u32);
                // A FIN after data we couldn't take hasn't been reached.
                fin_reached &= taken == data.len();
            } else {
                fin_reached = false;
            }
            need_ack = true;
        }

        if fin_reached && segment.sequence.wrapping_add(segment.data.len() as u32) == self.receive_next {
            self.receive_next = self.receive_next.wrapping_add(1);
            self.fin_received = true;
            need_ack = true;
            match self.state {
                State::SynReceived | State::Established => self.state = State::CloseWait,
                State::FinWait1 if fin_acknowledged => {
                    self.state = State::TimeWait;
                    self.close_at = Some(now + TIME_WAIT_MS);
                }
                State::FinWait1 => self.state = State::Closing,
                State::FinWait2 | State::TimeWait => {
                    self.state = State::TimeWait;
                    self.close_at = Some(now + TIME_WAIT_MS);
                }
                _ => {}
            }
        }

        let more = self.output(now, false);
        if need_ack && more.is_empty() {
            out.push(self.ack());
        }
        out.extend(more);
        (out, established)
    }
}

impl Stack {
    /// Sends what a connection has ready.
    pub(super) fn tcp_output(&mut self, id: SocketId) {
        let now = time::ticks();
        let Some(connection) = self.sockets.connection(id) else { return };
        let (local, remote) = (connection.local, connection.remote);
        let segments = connection.output(now, false);
        self.send_segments(local, remote, segments);
    }

    /// After the program read data: if the window had been nearly closed,
    /// tell the other side it's open again.
    pub(super) fn tcp_window_update(&mut self, id: SocketId) {
        let Some(connection) = self.sockets.connection(id) else { return };
        let small = connection.advertised < (connection.mss as u32) * 2;
        if !connection.connected() || !small || connection.window() < (connection.mss as u32) * 2 {
            return;
        }
        let (local, remote) = (connection.local, connection.remote);
        let ack = connection.ack();
        self.send_segments(local, remote, alloc::vec![ack]);
    }

    fn send_segments(&mut self, local: Endpoint, remote: Endpoint, segments: Vec<Vec<u8>>) {
        for segment in segments {
            let _ = self.send_ip(local.address, remote.address, PROTOCOL_TCP, &segment);
        }
    }

    /// The program closed its last file for the socket.
    pub(super) fn tcp_close(&mut self, id: SocketId) {
        let now = time::ticks();
        let Some(connection) = self.sockets.connection(id) else { return };
        connection.orphan = true;
        match connection.state {
            State::Listen => {
                // Connections nobody accepted are reset.
                let children: Vec<SocketId> = self
                    .sockets
                    .iter()
                    .filter_map(|(&child, socket)| match socket {
                        Socket::Stream(c) if c.parent == Some(id) => Some(child),
                        _ => None,
                    })
                    .collect();
                for child in children {
                    self.tcp_reset(child);
                }
                self.sockets.remove(id);
            }
            State::Closed | State::SynSent => {
                self.sockets.remove(id);
            }
            _ => {
                connection.shutdown_write();
                if connection.state == State::FinWait2 {
                    connection.close_at = Some(now + ORPHAN_TIMEOUT_MS);
                }
                self.tcp_output(id);
            }
        }
    }

    /// Aborts a connection with a reset, and forgets it.
    fn tcp_reset(&mut self, id: SocketId) {
        let Some(connection) = self.sockets.connection(id) else { return };
        let reset = connection.segment(connection.send_next, RST | ACK, &[]);
        let (local, remote) = (connection.local, connection.remote);
        self.sockets.remove(id);
        self.send_segments(local, remote, alloc::vec![reset]);
    }

    pub(super) fn run_tcp_timers(&mut self, now: u64) {
        for id in self.sockets.ids() {
            let Some(connection) = self.sockets.connection(id) else { continue };
            if connection.retransmit_at.is_some_and(|at| at <= now) {
                let (local, remote) = (connection.local, connection.remote);
                let segments = connection.retransmit(now);
                let failed = connection.state == State::Closed;
                self.send_segments(local, remote, segments);
                if failed {
                    // Tell the other side we gave up.
                    let reset = build(local, remote, 0, 0, RST, 0, &[]);
                    self.send_segments(local, remote, alloc::vec![reset]);
                }
            }
            let Some(connection) = self.sockets.connection(id) else { continue };
            if connection.close_at.is_some_and(|at| at <= now) {
                connection.close_at = None;
                connection.state = State::Closed;
            }
        }
        self.remove_finished();
    }

    /// Forgets orphaned connections that have finished closing.
    fn remove_finished(&mut self) {
        let finished: Vec<SocketId> = self
            .sockets
            .iter()
            .filter_map(|(&id, socket)| match socket {
                Socket::Stream(c) if c.state == State::Closed && (c.orphan || c.parent.is_some()) => Some(id),
                _ => None,
            })
            .collect();
        for id in finished {
            self.sockets.remove(id);
        }
    }

    pub(super) fn receive_tcp(&mut self, source: Ipv4, destination: Ipv4, bytes: &[u8]) {
        let Some(segment) = parse(source, destination, bytes) else { return };
        let now = time::ticks();
        let local = Endpoint { address: destination, port: segment.destination_port };
        let remote = Endpoint { address: source, port: segment.source_port };

        let connection = self.sockets.iter().find_map(|(&id, socket)| match socket {
            Socket::Stream(c)
                if !matches!(c.state, State::Listen | State::Closed) && c.local == local && c.remote == remote =>
            {
                Some(id)
            }
            _ => None,
        });
        if let Some(id) = connection {
            let connection = self.sockets.connection(id).unwrap();
            let (out, established) = connection.on_segment(&segment, now);
            let parent = connection.parent;
            self.send_segments(local, remote, out);
            if established && let Some(parent) = parent {
                match self.sockets.connection(parent) {
                    Some(listener) if listener.state == State::Listen => listener.accept_queue.push_back(id),
                    _ => self.tcp_reset(id),
                }
            }
            self.remove_finished();
            return;
        }

        let listener = self.sockets.iter().find_map(|(&id, socket)| match socket {
            Socket::Stream(c)
                if c.state == State::Listen
                    && c.local.port == local.port
                    && (c.local.address == Ipv4::ANY || c.local.address == local.address) =>
            {
                Some(id)
            }
            _ => None,
        });
        match listener {
            _ if segment.flags & RST != 0 => {}
            Some(listener) if segment.flags & ACK == 0 && segment.flags & SYN != 0 => {
                self.accept_syn(listener, local, remote, &segment, now);
            }
            _ => {
                // Nobody's listening: refuse with a reset.
                let reset = if segment.flags & ACK != 0 {
                    build(local, remote, segment.acknowledgment, 0, RST, 0, &[])
                } else {
                    let acknowledgment = segment.sequence.wrapping_add(segment.length());
                    build(local, remote, 0, acknowledgment, RST | ACK, 0, &[])
                };
                self.send_segments(local, remote, alloc::vec![reset]);
            }
        }
    }

    /// A SYN for a listening socket: starts a connection that `accept`
    /// returns once the handshake completes.
    fn accept_syn(&mut self, listener: SocketId, local: Endpoint, remote: Endpoint, segment: &Segment, now: u64) {
        let pending = self
            .sockets
            .iter()
            .filter(|(_, socket)| matches!(socket, Socket::Stream(c) if c.parent == Some(listener)))
            .count();
        let Some(listening) = self.sockets.connection(listener) else { return };
        if pending >= listening.backlog {
            // Full: drop the SYN; the other side will try again.
            return;
        }
        let mut connection = Connection::new();
        connection.state = State::SynReceived;
        connection.local = local;
        connection.remote = remote;
        connection.bound = true;
        connection.parent = Some(listener);
        connection.receive_next = segment.sequence.wrapping_add(1);
        connection.send_window = segment.window;
        connection.mss = segment.mss.map_or(DEFAULT_MSS, usize::from).clamp(64, usize::from(OUR_MSS));
        let id = self.sockets.insert(Socket::Stream(alloc::boxed::Box::new(connection)));
        let connection = self.sockets.connection(id).unwrap();
        let segments = connection.output(now, false);
        self.send_segments(local, remote, segments);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn sequence_numbers_compare_across_the_wrap() {
            assert!(before(1, 2));
            assert!(before(u32::MAX, 0));
            assert!(after(5, u32::MAX - 5));
            assert!(!before(7, 7));
        }

        fn segments_round_trip() {
            let local = Endpoint { address: Ipv4([10, 0, 2, 15]), port: 40000 };
            let remote = Endpoint { address: Ipv4([10, 0, 2, 2]), port: 80 };
            let bytes = build(local, remote, 1000, 2000, SYN | ACK, 4096, b"hi");
            let segment = parse(local.address, remote.address, &bytes).expect("bad checksum or header");
            assert_eq!((segment.source_port, segment.destination_port), (40000, 80));
            assert_eq!((segment.sequence, segment.acknowledgment, segment.flags), (1000, 2000, SYN | ACK));
            assert_eq!((segment.window, segment.mss, segment.data), (4096, Some(OUR_MSS), &b"hi"[..]));
            assert_eq!(segment.length(), 3);
            // A changed byte breaks the checksum.
            let mut corrupt = bytes.clone();
            corrupt[25] ^= 1;
            assert!(parse(local.address, remote.address, &corrupt).is_none());
        }
    }
}
