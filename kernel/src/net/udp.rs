//! UDP: single datagrams between ports, with no delivery guarantees.

use super::socket::Endpoint;
use super::{Ipv4, PROTOCOL_UDP, Stack, checksum, pseudo_header_sum, read_u16};
use crate::errno::Result;

impl Stack {
    pub(super) fn receive_udp(&mut self, source: Ipv4, destination: Ipv4, bytes: &[u8]) {
        if bytes.len() < 8 {
            return;
        }
        let length = usize::from(read_u16(bytes, 4));
        if length < 8 || length > bytes.len() {
            return;
        }
        let bytes = &bytes[..length];
        // A zero checksum means the sender didn't compute one.
        if read_u16(bytes, 6) != 0 && checksum(bytes, pseudo_header_sum(source, destination, PROTOCOL_UDP, length)) != 0
        {
            return;
        }
        let from = Endpoint { address: source, port: read_u16(bytes, 0) };
        let to = Endpoint { address: destination, port: read_u16(bytes, 2) };
        let data = &bytes[8..];
        if to.port == super::dhcp::CLIENT_PORT && from.port == super::dhcp::SERVER_PORT {
            self.receive_dhcp(data);
            return;
        }
        if !self.deliver_datagram(PROTOCOL_UDP, to, from, data, false) && self.is_local(destination) {
            // Nobody's listening: say so, as Linux does.
            self.send_unreachable(source, destination, 3, &original_packet(source, destination, bytes));
        }
    }

    pub fn send_udp(&mut self, from: Endpoint, to: Endpoint, data: &[u8]) -> Result<()> {
        let length = 8 + data.len();
        let mut datagram = alloc::vec::Vec::with_capacity(length);
        datagram.extend_from_slice(&from.port.to_be_bytes());
        datagram.extend_from_slice(&to.port.to_be_bytes());
        datagram.extend_from_slice(&(length as u16).to_be_bytes());
        datagram.extend_from_slice(&[0, 0]);
        datagram.extend_from_slice(data);
        let sum = match checksum(&datagram, pseudo_header_sum(from.address, to.address, PROTOCOL_UDP, length)) {
            // Zero means "no checksum", so a computed zero is sent as all ones.
            0 => 0xFFFF,
            sum => sum,
        };
        datagram[6..8].copy_from_slice(&sum.to_be_bytes());
        self.send_ip(from.address, to.address, PROTOCOL_UDP, &datagram)
    }
}

/// The start of the IP packet a datagram came in, rebuilt for an ICMP
/// error: the IP header (20 bytes) and the first 8 bytes of the datagram.
fn original_packet(source: Ipv4, destination: Ipv4, datagram: &[u8]) -> [u8; 28] {
    let mut packet = [0; 28];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&((20 + datagram.len()) as u16).to_be_bytes());
    packet[8] = 64;
    packet[9] = PROTOCOL_UDP;
    packet[12..16].copy_from_slice(&source.0);
    packet[16..20].copy_from_slice(&destination.0);
    packet[20..28].copy_from_slice(&datagram[..8]);
    packet
}
