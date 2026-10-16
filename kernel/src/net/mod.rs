//! eliOS's TCP/IP stack.
//!
//! Layers, bottom up: a network card (`NetDevice`) sends and receives
//! Ethernet frames; ARP finds the Ethernet address for an IP address; IPv4
//! carries packets between hosts; ICMP (ping), UDP and TCP run on top; and
//! sockets (`socket.rs`) are how programs use UDP and TCP, through the same
//! system calls as on Linux. A DHCP client configures the interface at
//! boot.
//!
//! One kernel thread runs the stack: it takes frames the card received,
//! processes them, and runs timers (TCP retransmissions, ARP retries,
//! DHCP). Programs' system calls also work on the stack directly, sending
//! packets as they go. Everything is under one lock, held briefly.
//!
//! Loopback (127.0.0.1, and the machine's own address) works by handing
//! the packet back to the stack thread instead of to a card.

mod arp;
mod dhcp;
mod icmp;
pub mod socket;
mod tcp;
mod udp;
pub mod unix;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use self::socket::Sockets;
use crate::sync::SpinLock;
use crate::task::{self, WaitQueue};
use crate::time;

/// A network card.
pub trait NetDevice: Send + Sync {
    fn mac(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
    /// Sends an Ethernet frame (without the checksum, which the card adds).
    /// May drop it if the card is busy, as a network may.
    fn transmit(&self, frame: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const ANY: Self = Self([0; 4]);
    pub const BROADCAST: Self = Self([255; 4]);
    pub const LOOPBACK: Self = Self([127, 0, 0, 1]);

    pub fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    pub fn from_u32(value: u32) -> Self {
        Self(value.to_be_bytes())
    }

    pub fn is_loopback(self) -> bool {
        self.0[0] == 127
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// An interface's IPv4 settings.
#[derive(Debug, Clone)]
pub struct Config {
    pub address: Ipv4,
    pub prefix: u8,
    pub gateway: Option<Ipv4>,
}

impl Config {
    pub fn netmask(&self) -> Ipv4 {
        Ipv4::from_u32(if self.prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(self.prefix)) })
    }

    fn on_link(&self, address: Ipv4) -> bool {
        let mask = self.netmask().to_u32();
        address.to_u32() & mask == self.address.to_u32() & mask
    }

    fn broadcast(&self) -> Ipv4 {
        Ipv4::from_u32(self.address.to_u32() | !self.netmask().to_u32())
    }
}

/// Packet and byte counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    pub received_packets: u64,
    pub received_bytes: u64,
    pub sent_packets: u64,
    pub sent_bytes: u64,
}

struct Interface {
    device: Arc<dyn NetDevice>,
    mac: [u8; 6],
    config: Option<Config>,
    counters: Counters,
}

pub struct Stack {
    interface: Option<Interface>,
    loopback_counters: Counters,
    arp: arp::Cache,
    dhcp: dhcp::Client,
    pub sockets: Sockets,
    /// IP packets addressed to this machine, to process on the stack thread.
    loopback: VecDeque<Vec<u8>>,
    next_ip_id: u16,
}

pub static STACK: SpinLock<Stack> = SpinLock::new(Stack {
    interface: None,
    loopback_counters: Counters { received_packets: 0, received_bytes: 0, sent_packets: 0, sent_bytes: 0 },
    arp: arp::Cache::new(),
    dhcp: dhcp::Client::new(),
    sockets: Sockets::new(),
    loopback: VecDeque::new(),
    next_ip_id: 1,
});

/// Frames received by cards, waiting for the stack thread.
static RECEIVED: SpinLock<VecDeque<Vec<u8>>> = SpinLock::new(VecDeque::new());
/// Wakes the stack thread.
static WORK: WaitQueue = WaitQueue::new();
/// Woken whenever a socket's state changes; blocked socket calls wait here.
pub static SOCKET_EVENTS: WaitQueue = WaitQueue::new();

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTOCOL_ICMP: u8 = 1;
pub const PROTOCOL_TCP: u8 = 6;
pub const PROTOCOL_UDP: u8 = 17;
const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

/// Starts the stack thread. Cards can be added before or after.
pub fn init() {
    task::spawn("net", run);
}

/// Adds a network card as `eth0` and starts configuring it with DHCP.
pub fn add_interface(device: Arc<dyn NetDevice>) {
    let mut stack = STACK.lock();
    if stack.interface.is_some() {
        warn!("net: only one network card is supported; ignoring another");
        return;
    }
    let mac = device.mac();
    stack.interface = Some(Interface { device, mac, config: None, counters: Counters::default() });
    stack.dhcp.start(&mac);
    drop(stack);
    WORK.wake_all();
}

/// Waits (up to `timeout_ms`) for the network card, if there is one, to be
/// configured by DHCP, so programs started at boot find the network ready.
pub fn wait_until_configured(timeout_ms: u64) {
    let deadline = time::ticks() + timeout_ms;
    while time::ticks() < deadline {
        match &STACK.lock().interface {
            Some(interface) if interface.config.is_none() => {}
            _ => return,
        }
        task::sleep_ms(10);
    }
}

/// Called by card drivers with each frame they receive.
pub fn receive(frame: Vec<u8>) {
    let mut queue = RECEIVED.lock();
    // A bound, so a flood can't eat all memory.
    if queue.len() < 512 {
        queue.push_back(frame);
    }
    drop(queue);
    WORK.wake_all();
}

/// What `poll` and `epoll` wait on for network sockets: one source shared
/// by all of them, since the stack doesn't track which socket a change
/// was for. Programs waiting on other files aren't woken by traffic.
pub fn sockets_changed() -> &'static alloc::sync::Arc<crate::fs::PollSource> {
    static SOURCE: crate::sync::Once<alloc::sync::Arc<crate::fs::PollSource>> = crate::sync::Once::new();
    SOURCE.get_or_init(|| alloc::sync::Arc::new(crate::fs::PollSource::new()))
}

/// Tells blocked socket calls and `poll` that something changed.
pub fn notify() {
    SOCKET_EVENTS.wake_all();
    sockets_changed().notify();
}

/// The stack thread: processes received frames and loopback packets, and
/// runs timers.
fn run() {
    loop {
        let mut changed = false;
        loop {
            let frame = RECEIVED.lock().pop_front();
            let Some(frame) = frame else { break };
            STACK.lock().receive_frame(&frame);
            changed = true;
        }
        loop {
            let mut stack = STACK.lock();
            let Some(packet) = stack.loopback.pop_front() else { break };
            stack.loopback_counters.received_packets += 1;
            stack.loopback_counters.received_bytes += packet.len() as u64;
            stack.receive_ip(&packet, true);
            changed = true;
        }
        let (deadline, dns) = {
            let mut stack = STACK.lock();
            stack.run_timers(time::ticks());
            (stack.next_deadline(), stack.dhcp.new_dns.take())
        };
        if let Some(dns) = dns {
            write_resolv_conf(&dns);
        }
        if changed {
            notify();
        }
        // Wake for new work or the next timer, and at least every 100 ms.
        let deadline = deadline.unwrap_or(u64::MAX).min(time::ticks() + 100);
        WORK.wait_until_deadline(Some(deadline), || {
            let busy = !RECEIVED.lock().is_empty() || !STACK.lock().loopback.is_empty();
            busy.then_some(())
        });
    }
}

/// The DNS servers DHCP gave, for /etc/resolv.conf.
static DNS_SERVERS: SpinLock<Vec<Ipv4>> = SpinLock::new(Vec::new());

/// Writes the DNS servers where C libraries look for them. DHCP may finish
/// before the root file system is mounted; then it's written once it is
/// (see [`write_dns_settings`]).
fn write_resolv_conf(servers: &[Ipv4]) {
    *DNS_SERVERS.lock() = servers.to_vec();
    write_dns_settings();
}

/// Writes /etc/resolv.conf from the DNS servers DHCP gave, if any.
pub fn write_dns_settings() {
    let servers = DNS_SERVERS.lock().clone();
    if servers.is_empty() {
        return;
    }
    let mut text = String::from("# Written by eliOS from DHCP\n");
    for server in &servers {
        text.push_str(&format!("nameserver {server}\n"));
    }
    let Ok(etc) = crate::fs::lookup("/", "/etc", true) else { return };
    let file = etc
        .lookup("resolv.conf")
        .or_else(|_| etc.create("resolv.conf", crate::fs::FileKind::Regular, crate::fs::Attributes::root(0o644)));
    if let Ok(file) = file {
        let _ = file.truncate(0);
        let _ = file.write_at(0, text.as_bytes());
    }
}

/// The number Linux uses for a TCP state in /proc/net/tcp.
pub fn tcp_state_code(state: tcp::State) -> u8 {
    use tcp::State::*;
    match state {
        Established => 1,
        SynSent => 2,
        SynReceived => 3,
        FinWait1 => 4,
        FinWait2 => 5,
        TimeWait => 6,
        Closed => 7,
        CloseWait => 8,
        LastAck => 9,
        Listen => 10,
        Closing => 11,
    }
}

/// What a program sees about the interfaces, for `ifconfig` and /proc.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub name: &'static str,
    pub mac: [u8; 6],
    pub config: Option<Config>,
    pub up: bool,
    pub counters: Counters,
    pub mtu: u32,
}

pub fn interfaces() -> Vec<InterfaceInfo> {
    let stack = STACK.lock();
    let loopback = Config { address: Ipv4::LOOPBACK, prefix: 8, gateway: None };
    let mut list = alloc::vec![InterfaceInfo {
        name: "lo",
        mac: [0; 6],
        config: Some(loopback),
        up: true,
        counters: stack.loopback_counters,
        mtu: 65536,
    }];
    if let Some(interface) = &stack.interface {
        list.push(InterfaceInfo {
            name: "eth0",
            mac: interface.mac,
            config: interface.config.clone(),
            up: interface.device.link_up(),
            counters: interface.counters,
            mtu: 1500,
        });
    }
    list
}

/// The ARP cache, as (address, MAC) pairs.
pub fn neighbors() -> Vec<(Ipv4, [u8; 6])> {
    STACK.lock().arp.entries()
}

/// The ones' complement sum used by IP, ICMP, UDP and TCP checksums, over
/// `data` with `initial` added in.
pub fn checksum(data: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    let (pairs, rest) = data.as_chunks::<2>();
    for pair in pairs {
        sum += u32::from(u16::from_be_bytes(*pair));
    }
    if let [last] = rest {
        sum += u32::from(*last) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The sum of the "pseudo-header" that UDP and TCP checksums cover.
fn pseudo_header_sum(source: Ipv4, destination: Ipv4, protocol: u8, length: usize) -> u32 {
    let words = |address: Ipv4| {
        u32::from(u16::from_be_bytes([address.0[0], address.0[1]]))
            + u32::from(u16::from_be_bytes([address.0[2], address.0[3]]))
    };
    words(source) + words(destination) + u32::from(protocol) + length as u32
}

pub fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

pub fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

impl Stack {
    /// This machine's address for talking to `destination`.
    pub fn source_for(&self, destination: Ipv4) -> Option<Ipv4> {
        if destination.is_loopback() {
            return Some(Ipv4::LOOPBACK);
        }
        self.config().map(|config| config.address)
    }

    fn config(&self) -> Option<&Config> {
        self.interface.as_ref()?.config.as_ref()
    }

    /// Whether `address` is one of this machine's.
    pub fn is_local(&self, address: Ipv4) -> bool {
        address.is_loopback() || self.config().is_some_and(|config| config.address == address)
    }

    fn receive_frame(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        if let Some(interface) = &mut self.interface {
            interface.counters.received_packets += 1;
            interface.counters.received_bytes += frame.len() as u64;
        }
        let payload = &frame[14..];
        match read_u16(frame, 12) {
            ETHERTYPE_ARP => self.receive_arp(payload),
            ETHERTYPE_IPV4 => self.receive_ip(payload, false),
            _ => {}
        }
    }

    fn send_frame(&mut self, destination: [u8; 6], ethertype: u16, payload: &[u8]) {
        let Some(interface) = &mut self.interface else { return };
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&destination);
        frame.extend_from_slice(&interface.mac);
        frame.extend_from_slice(&ethertype.to_be_bytes());
        frame.extend_from_slice(payload);
        // The minimum Ethernet payload is 46 bytes.
        frame.resize(frame.len().max(60), 0);
        interface.counters.sent_packets += 1;
        interface.counters.sent_bytes += frame.len() as u64;
        interface.device.transmit(&frame);
    }

    fn receive_ip(&mut self, packet: &[u8], looped: bool) {
        if packet.len() < 20 || packet[0] >> 4 != 4 {
            return;
        }
        let header_length = usize::from(packet[0] & 0xF) * 4;
        let total_length = usize::from(read_u16(packet, 2));
        if header_length < 20 || total_length < header_length || total_length > packet.len() {
            return;
        }
        if !looped && checksum(&packet[..header_length], 0) != 0 {
            return;
        }
        // Fragments aren't reassembled; senders here set "don't fragment".
        let fragment = read_u16(packet, 6);
        if fragment & 0x3FFF != 0 {
            return;
        }
        let source = Ipv4(packet[12..16].try_into().unwrap());
        let destination = Ipv4(packet[16..20].try_into().unwrap());
        let ours = self.is_local(destination)
            || destination == Ipv4::BROADCAST
            || self.config().is_some_and(|config| config.broadcast() == destination)
            // Before DHCP finishes, the offer comes to the address on offer.
            || self.config().is_none();
        if !ours {
            return;
        }
        let payload = &packet[header_length..total_length];
        match packet[9] {
            PROTOCOL_ICMP => self.receive_icmp(source, destination, payload, &packet[..total_length]),
            PROTOCOL_UDP => self.receive_udp(source, destination, payload),
            PROTOCOL_TCP => self.receive_tcp(source, destination, payload),
            _ => {}
        }
    }

    /// Sends an IP packet. Fails if there's no route to `destination`.
    pub fn send_ip(
        &mut self,
        source: Ipv4,
        destination: Ipv4,
        protocol: u8,
        payload: &[u8],
    ) -> crate::errno::Result<()> {
        use crate::errno::Errno;
        let mut packet = Vec::with_capacity(20 + payload.len());
        let id = self.next_ip_id;
        self.next_ip_id = self.next_ip_id.wrapping_add(1);
        packet.extend_from_slice(&[0x45, 0]);
        packet.extend_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&id.to_be_bytes());
        // Don't fragment, time to live 64.
        packet.extend_from_slice(&[0x40, 0, 64, protocol, 0, 0]);
        packet.extend_from_slice(&source.0);
        packet.extend_from_slice(&destination.0);
        let sum = checksum(&packet, 0);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        packet.extend_from_slice(payload);

        if self.is_local(destination) {
            self.loopback_counters.sent_packets += 1;
            self.loopback_counters.sent_bytes += packet.len() as u64;
            self.loopback.push_back(packet);
            WORK.wake_all();
            return Ok(());
        }
        if destination == Ipv4::BROADCAST || self.config().is_some_and(|config| config.broadcast() == destination) {
            self.send_frame(BROADCAST_MAC, ETHERTYPE_IPV4, &packet);
            return Ok(());
        }
        let config = self.config().ok_or(Errno::ENETUNREACH)?;
        let next_hop =
            if config.on_link(destination) { destination } else { config.gateway.ok_or(Errno::ENETUNREACH)? };
        match self.arp.lookup(next_hop) {
            Some(mac) => self.send_frame(mac, ETHERTYPE_IPV4, &packet),
            None => {
                let request = self.arp.queue(next_hop, packet, time::ticks());
                if request {
                    self.send_arp_request(next_hop);
                }
            }
        }
        Ok(())
    }

    fn run_timers(&mut self, now: u64) {
        for target in self.arp.retries(now) {
            self.send_arp_request(target);
        }
        self.run_dhcp(now);
        self.run_tcp_timers(now);
    }

    fn next_deadline(&self) -> Option<u64> {
        [self.arp.next_deadline(), self.dhcp.next_deadline(), self.sockets.next_tcp_deadline()]
            .into_iter()
            .flatten()
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn checksums_match_known_values() {
            // A real IPv4 header: summing it, checksum field included,
            // gives zero.
            let header = [
                0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0xB8, 0x61, 0xC0, 0xA8, 0x00, 0x01, 0xC0,
                0xA8, 0x00, 0xC7,
            ];
            assert_eq!(checksum(&header, 0), 0);
            let mut zeroed = header;
            zeroed[10] = 0;
            zeroed[11] = 0;
            assert_eq!(checksum(&zeroed, 0), 0xB861);
            // Odd lengths pad with a zero byte.
            assert_eq!(checksum(&[0x01], 0), !0x0100);
        }

        fn netmasks_and_subnets() {
            let config = Config { address: Ipv4([10, 0, 2, 15]), prefix: 24, gateway: None };
            assert_eq!(config.netmask(), Ipv4([255, 255, 255, 0]));
            assert_eq!(config.broadcast(), Ipv4([10, 0, 2, 255]));
            assert!(config.on_link(Ipv4([10, 0, 2, 2])));
            assert!(!config.on_link(Ipv4([10, 0, 3, 2])));
        }
    }
}
