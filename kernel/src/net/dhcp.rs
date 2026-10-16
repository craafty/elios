//! DHCP: getting an address, netmask, router and DNS servers from the
//! network.
//!
//! The client broadcasts DISCOVER; servers answer with an OFFER; the client
//! REQUESTs the offered address; the server confirms with an ACK. All of it
//! goes over UDP broadcasts, since the client has no address yet.

use alloc::vec::Vec;

use super::socket::Endpoint;
use super::{Config, Ipv4, Stack, read_u32};

pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS: u8 = 6;
const OPTION_REQUESTED_ADDRESS: u8 = 50;
const OPTION_MESSAGE_TYPE: u8 = 53;
const OPTION_SERVER: u8 = 54;
const OPTION_PARAMETERS: u8 = 55;
const OPTION_END: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Off,
    Discovering,
    Requesting,
    Bound,
}

pub struct Client {
    state: State,
    mac: [u8; 6],
    transaction: u32,
    next_send: u64,
    attempts: u32,
    offered: Ipv4,
    server: Ipv4,
    /// DNS servers learned but not yet written to /etc/resolv.conf (which
    /// needs the file system, so it's done outside the stack's lock).
    pub new_dns: Option<Vec<Ipv4>>,
}

impl Client {
    pub const fn new() -> Self {
        Self {
            state: State::Off,
            mac: [0; 6],
            transaction: 0,
            next_send: 0,
            attempts: 0,
            offered: Ipv4::ANY,
            server: Ipv4::ANY,
            new_dns: None,
        }
    }

    pub fn start(&mut self, mac: &[u8; 6]) {
        let mut random = [0; 4];
        crate::random::fill(&mut random);
        self.mac = *mac;
        self.transaction = u32::from_le_bytes(random);
        self.state = State::Discovering;
        self.next_send = 0;
        self.attempts = 0;
    }

    pub fn next_deadline(&self) -> Option<u64> {
        matches!(self.state, State::Discovering | State::Requesting).then_some(self.next_send)
    }
}

impl Stack {
    pub(super) fn run_dhcp(&mut self, now: u64) {
        let client = &mut self.dhcp;
        if !matches!(client.state, State::Discovering | State::Requesting) || now < client.next_send {
            return;
        }
        // Back off: 1, 2, 4, then every 8 seconds.
        client.attempts += 1;
        client.next_send = now + 1000 * (1 << client.attempts.min(4)) / 2;
        if client.state == State::Requesting && client.attempts > 4 {
            client.state = State::Discovering;
        }
        let message = if client.state == State::Discovering { DISCOVER } else { REQUEST };
        // The first few attempts are logged, for machines whose network
        // can only be debugged from the log.
        if client.attempts <= 6 {
            let name = if message == DISCOVER { "discover" } else { "request" };
            debug!("net: DHCP: sending a {name} (attempt {})", client.attempts);
        }
        self.send_dhcp(message);
    }

    fn send_dhcp(&mut self, message: u8) {
        let client = &self.dhcp;
        let mut packet = Vec::with_capacity(300);
        // Request, Ethernet, 6-byte addresses, no hops.
        packet.extend_from_slice(&[1, 1, 6, 0]);
        packet.extend_from_slice(&client.transaction.to_be_bytes());
        // Seconds, and the broadcast flag: answer by broadcast, since we
        // can't receive unicast yet.
        packet.extend_from_slice(&[0, 0, 0x80, 0]);
        packet.extend_from_slice(&[0; 16]);
        packet.extend_from_slice(&client.mac);
        packet.resize(28 + 16 + 64 + 128, 0);
        packet.extend_from_slice(&MAGIC_COOKIE);
        packet.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, message]);
        if message == REQUEST {
            packet.extend_from_slice(&[OPTION_REQUESTED_ADDRESS, 4]);
            packet.extend_from_slice(&client.offered.0);
            packet.extend_from_slice(&[OPTION_SERVER, 4]);
            packet.extend_from_slice(&client.server.0);
        }
        packet.extend_from_slice(&[OPTION_PARAMETERS, 3, OPTION_SUBNET_MASK, OPTION_ROUTER, OPTION_DNS]);
        packet.push(OPTION_END);
        packet.resize(packet.len().max(300), 0);
        let from = Endpoint { address: Ipv4::ANY, port: CLIENT_PORT };
        let to = Endpoint { address: Ipv4::BROADCAST, port: SERVER_PORT };
        let _ = self.send_udp(from, to, &packet);
    }

    pub(super) fn receive_dhcp(&mut self, packet: &[u8]) {
        if packet.len() < 240
            || packet[0] != 2
            || read_u32(packet, 4) != self.dhcp.transaction
            || packet[236..240] != MAGIC_COOKIE
        {
            return;
        }
        let address = Ipv4(packet[16..20].try_into().unwrap());
        let mut message = 0;
        let mut mask = None;
        let mut router = None;
        let mut dns = Vec::new();
        let mut server = None;
        let mut offset = 240;
        while offset < packet.len() {
            let option = packet[offset];
            if option == OPTION_END {
                break;
            }
            if option == 0 {
                offset += 1;
                continue;
            }
            let Some(&length) = packet.get(offset + 1) else { break };
            let Some(value) = packet.get(offset + 2..offset + 2 + usize::from(length)) else { break };
            let address = |bytes: &[u8]| Ipv4(bytes[..4].try_into().unwrap());
            match option {
                OPTION_MESSAGE_TYPE if length == 1 => message = value[0],
                OPTION_SUBNET_MASK if length == 4 => mask = Some(address(value)),
                OPTION_ROUTER if length >= 4 => router = Some(address(value)),
                OPTION_DNS => dns.extend(value.as_chunks::<4>().0.iter().map(|bytes| Ipv4(*bytes))),
                OPTION_SERVER if length == 4 => server = Some(address(value)),
                _ => {}
            }
            offset += 2 + usize::from(length);
        }

        match (self.dhcp.state, message) {
            (State::Discovering, OFFER) => {
                debug!("net: DHCP: offered {address} by {}", server.unwrap_or(Ipv4::ANY));
                let client = &mut self.dhcp;
                client.offered = address;
                client.server = server.unwrap_or(Ipv4::ANY);
                client.state = State::Requesting;
                client.attempts = 0;
                client.next_send = crate::time::ticks() + 2000;
                self.send_dhcp(REQUEST);
            }
            (State::Requesting, ACK) => {
                let prefix = mask.map_or(24, |mask| mask.to_u32().leading_ones() as u8);
                let config = Config { address, prefix, gateway: router };
                info!(
                    "net: eth0: {address}/{prefix}{} (DHCP)",
                    router.map_or_else(alloc::string::String::new, |router| alloc::format!(" via {router}"))
                );
                if let Some(interface) = &mut self.interface {
                    interface.config = Some(config);
                }
                self.dhcp.state = State::Bound;
                self.dhcp.new_dns = Some(dns);
            }
            (State::Requesting, NAK) => {
                self.dhcp.state = State::Discovering;
                self.dhcp.attempts = 0;
                self.dhcp.next_send = 0;
            }
            _ => {}
        }
    }
}
