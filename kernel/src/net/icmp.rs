//! ICMP: echo requests and replies (ping), and error reports.

use alloc::vec::Vec;

use super::socket::Endpoint;
use super::{Ipv4, PROTOCOL_ICMP, Stack, checksum, read_u16};
use crate::errno::{Errno, Result};

const ECHO_REPLY: u8 = 0;
const DESTINATION_UNREACHABLE: u8 = 3;
const ECHO_REQUEST: u8 = 8;

impl Stack {
    pub(super) fn receive_icmp(&mut self, source: Ipv4, destination: Ipv4, message: &[u8], packet: &[u8]) {
        if message.len() < 8 || checksum(message, 0) != 0 {
            return;
        }
        // Raw sockets see every ICMP message, IP header and all.
        let from = Endpoint { address: source, port: 0 };
        self.deliver_datagram(PROTOCOL_ICMP, Endpoint::default(), from, packet, true);
        match message[0] {
            ECHO_REQUEST => {
                let mut reply = message.to_vec();
                reply[0] = ECHO_REPLY;
                reply[2..4].fill(0);
                let sum = checksum(&reply, 0);
                reply[2..4].copy_from_slice(&sum.to_be_bytes());
                let Some(us) = (if self.is_local(destination) { Some(destination) } else { self.source_for(source) })
                else {
                    return;
                };
                let _ = self.send_ip(us, source, PROTOCOL_ICMP, &reply);
            }
            ECHO_REPLY => {
                // Ping sockets are bound to the echo identifier.
                let local = Endpoint { address: destination, port: read_u16(message, 4) };
                self.deliver_datagram(PROTOCOL_ICMP, local, from, message, false);
            }
            _ => {}
        }
    }

    /// Sends what a program wrote to an ICMP socket. Ping sockets may only
    /// send echo requests, and get their socket's identifier and a fresh
    /// checksum; raw sockets send the message as is.
    pub(super) fn send_icmp_from_socket(
        &mut self,
        source: Ipv4,
        destination: Ipv4,
        identifier: u16,
        raw: bool,
        message: &[u8],
    ) -> Result<()> {
        if message.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let mut message = message.to_vec();
        if !raw {
            if message[0] != ECHO_REQUEST {
                return Err(Errno::EINVAL);
            }
            message[4..6].copy_from_slice(&identifier.to_be_bytes());
            message[2..4].fill(0);
            let sum = checksum(&message, 0);
            message[2..4].copy_from_slice(&sum.to_be_bytes());
        }
        self.send_ip(source, destination, PROTOCOL_ICMP, &message)
    }

    /// Reports that `packet` (its IP header and first 8 bytes of data)
    /// couldn't be delivered, with an unreachable `code` (3: port).
    pub(super) fn send_unreachable(&mut self, to: Ipv4, from: Ipv4, code: u8, packet: &[u8]) {
        let mut message = Vec::with_capacity(8 + packet.len());
        message.extend_from_slice(&[DESTINATION_UNREACHABLE, code, 0, 0, 0, 0, 0, 0]);
        message.extend_from_slice(packet);
        let sum = checksum(&message, 0);
        message[2..4].copy_from_slice(&sum.to_be_bytes());
        let _ = self.send_ip(from, to, PROTOCOL_ICMP, &message);
    }
}
