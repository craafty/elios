//! ARP: finding the Ethernet address of a machine on the local network
//! from its IP address, by broadcasting "who has 10.0.2.2?" and caching the
//! answer.

use alloc::vec::Vec;

use super::{BROADCAST_MAC, ETHERTYPE_ARP, Ipv4, Stack, read_u16};

/// How long answers are trusted.
const LIFETIME_MS: u64 = 5 * 60 * 1000;
const RETRY_MS: u64 = 1000;
const ATTEMPTS: u32 = 3;
/// Packets kept per address while waiting for an answer.
const QUEUE_LIMIT: usize = 16;

struct Entry {
    address: Ipv4,
    mac: [u8; 6],
    expires: u64,
}

/// An address being looked up, and the packets waiting for the answer.
struct Lookup {
    address: Ipv4,
    packets: Vec<Vec<u8>>,
    attempts: u32,
    next_attempt: u64,
}

pub struct Cache {
    entries: Vec<Entry>,
    lookups: Vec<Lookup>,
}

impl Cache {
    pub const fn new() -> Self {
        Self { entries: Vec::new(), lookups: Vec::new() }
    }

    pub fn lookup(&self, address: Ipv4) -> Option<[u8; 6]> {
        let now = crate::time::ticks();
        self.entries.iter().find(|entry| entry.address == address && entry.expires > now).map(|entry| entry.mac)
    }

    /// Holds `packet` until `address` is found. Returns whether a request
    /// should go out now (it's the first packet waiting).
    pub fn queue(&mut self, address: Ipv4, packet: Vec<u8>, now: u64) -> bool {
        if let Some(lookup) = self.lookups.iter_mut().find(|lookup| lookup.address == address) {
            if lookup.packets.len() < QUEUE_LIMIT {
                lookup.packets.push(packet);
            }
            return false;
        }
        self.lookups.push(Lookup { address, packets: alloc::vec![packet], attempts: 1, next_attempt: now + RETRY_MS });
        true
    }

    /// Records an answer; returns the packets that were waiting for it.
    fn learn(&mut self, address: Ipv4, mac: [u8; 6], now: u64) -> Vec<Vec<u8>> {
        self.entries.retain(|entry| entry.address != address && entry.expires > now);
        self.entries.push(Entry { address, mac, expires: now + LIFETIME_MS });
        match self.lookups.iter().position(|lookup| lookup.address == address) {
            Some(index) => self.lookups.remove(index).packets,
            None => Vec::new(),
        }
    }

    /// Addresses to ask about again; lookups that ran out of attempts are
    /// dropped with their packets.
    pub fn retries(&mut self, now: u64) -> Vec<Ipv4> {
        self.lookups.retain(|lookup| lookup.attempts < ATTEMPTS || lookup.next_attempt > now);
        let mut due = Vec::new();
        for lookup in self.lookups.iter_mut().filter(|lookup| lookup.next_attempt <= now) {
            lookup.attempts += 1;
            lookup.next_attempt = now + RETRY_MS;
            due.push(lookup.address);
        }
        due
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.lookups.iter().map(|lookup| lookup.next_attempt).min()
    }

    pub fn entries(&self) -> Vec<(Ipv4, [u8; 6])> {
        self.entries.iter().map(|entry| (entry.address, entry.mac)).collect()
    }
}

const OPERATION_REQUEST: u16 = 1;
const OPERATION_REPLY: u16 = 2;

impl Stack {
    pub(super) fn receive_arp(&mut self, packet: &[u8]) {
        // Ethernet and IPv4 only: hardware type 1, protocol 0x0800, sizes 6
        // and 4.
        if packet.len() < 28
            || read_u16(packet, 0) != 1
            || read_u16(packet, 2) != 0x0800
            || packet[4] != 6
            || packet[5] != 4
        {
            return;
        }
        let operation = read_u16(packet, 6);
        let sender_mac: [u8; 6] = packet[8..14].try_into().unwrap();
        let sender = Ipv4(packet[14..18].try_into().unwrap());
        let target = Ipv4(packet[24..28].try_into().unwrap());
        let Some(address) = self.config().map(|config| config.address) else { return };

        let waiting = self.arp.learn(sender, sender_mac, crate::time::ticks());
        for ip_packet in waiting {
            self.send_frame(sender_mac, super::ETHERTYPE_IPV4, &ip_packet);
        }
        if operation == OPERATION_REQUEST && target == address {
            self.send_arp(OPERATION_REPLY, sender_mac, sender);
        }
    }

    pub(super) fn send_arp_request(&mut self, target: Ipv4) {
        self.send_arp(OPERATION_REQUEST, [0; 6], target);
    }

    fn send_arp(&mut self, operation: u16, target_mac: [u8; 6], target: Ipv4) {
        let Some(interface) = &self.interface else { return };
        let mac = interface.mac;
        let address = self.config().map_or(Ipv4::ANY, |config| config.address);
        let mut packet = Vec::with_capacity(28);
        packet.extend_from_slice(&[0, 1, 0x08, 0x00, 6, 4]);
        packet.extend_from_slice(&operation.to_be_bytes());
        packet.extend_from_slice(&mac);
        packet.extend_from_slice(&address.0);
        packet.extend_from_slice(&target_mac);
        packet.extend_from_slice(&target.0);
        let destination = if operation == OPERATION_REQUEST { BROADCAST_MAC } else { target_mac };
        self.send_frame(destination, ETHERTYPE_ARP, &packet);
    }
}
