//! L2CAP: the channels a Bluetooth connection carries.
//!
//! An ACL connection to a device is one stream of packets; L2CAP divides
//! it into channels, each named at either end by a channel ID (CID) and
//! opened to a service by its PSM (protocol/service multiplexer, like a
//! port number: 0x11 and 0x13 are HID's control and interrupt channels).
//! Channel 1 carries signaling: requests to open, configure and close
//! channels, each with an identifier its response repeats.
//!
//! Opening a channel takes a connection request and response, then each
//! side configures the direction it receives (we ask for nothing beyond
//! the defaults) and the other agrees. This module does that bookkeeping
//! for one connection; what it wants sent collects in `outbox`, which
//! the stack sends as ACL data.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The signaling channel's CID.
pub(super) const SIGNALING: u16 = 0x0001;

const COMMAND_REJECT: u8 = 0x01;
const CONNECTION_REQUEST: u8 = 0x02;
const CONNECTION_RESPONSE: u8 = 0x03;
const CONFIGURE_REQUEST: u8 = 0x04;
const CONFIGURE_RESPONSE: u8 = 0x05;
const DISCONNECTION_REQUEST: u8 = 0x06;
const DISCONNECTION_RESPONSE: u8 = 0x07;
const ECHO_REQUEST: u8 = 0x08;
const ECHO_RESPONSE: u8 = 0x09;
const INFORMATION_REQUEST: u8 = 0x0A;
const INFORMATION_RESPONSE: u8 = 0x0B;

/// Connection results.
const SUCCESS: u16 = 0;
const PENDING: u16 = 1;
const PSM_NOT_SUPPORTED: u16 = 2;
/// Pending because the link isn't secured yet.
const AUTHENTICATION_PENDING: u16 = 1;

/// The largest packet we take on a channel: L2CAP's default (nothing
/// carried here needs more).
const MTU: u16 = 672;
/// Our CIDs start here (lower ones are fixed channels).
const FIRST_DYNAMIC_CID: u16 = 0x0040;

pub(super) struct Channel {
    pub psm: u16,
    pub local: u16,
    /// The device's CID for it, once known.
    pub remote: u16,
    /// Whether the device agreed to our configuration, and we to its.
    configured_out: bool,
    configured_in: bool,
    config_sent: bool,
    /// A connection request from the device we answered "pending" until
    /// the link is encrypted: its identifier.
    waiting_for_security: Option<u8>,
}

impl Channel {
    pub fn open(&self) -> bool {
        self.configured_in && self.configured_out
    }
}

pub(super) struct L2cap {
    pub channels: Vec<Channel>,
    /// Frames to send: (CID, payload).
    pub outbox: Vec<(u16, Vec<u8>)>,
    /// What went wrong, for the stack to report.
    pub errors: Vec<String>,
    /// Whether the link is encrypted (channels are only opened then).
    secure: bool,
    next_cid: u16,
    next_ident: u8,
    /// PSMs the device may open channels to.
    served: &'static [u16],
}

impl L2cap {
    pub fn new(served: &'static [u16]) -> Self {
        Self {
            channels: Vec::new(),
            outbox: Vec::new(),
            errors: Vec::new(),
            secure: false,
            next_cid: FIRST_DYNAMIC_CID,
            next_ident: 1,
            served,
        }
    }

    pub fn channel(&self, psm: u16) -> Option<&Channel> {
        self.channels.iter().find(|channel| channel.psm == psm)
    }

    pub fn is_open(&self, psm: u16) -> bool {
        self.channel(psm).is_some_and(Channel::open)
    }

    /// The PSM of our channel `cid`, if it's open.
    pub fn open_psm(&self, cid: u16) -> Option<u16> {
        self.channels.iter().find(|channel| channel.local == cid && channel.open()).map(|channel| channel.psm)
    }

    /// Asks the device to open a channel to `psm`.
    pub fn connect(&mut self, psm: u16) {
        let local = self.allocate_cid();
        self.channels.push(Channel {
            psm,
            local,
            remote: 0,
            configured_out: false,
            configured_in: false,
            config_sent: false,
            waiting_for_security: None,
        });
        let ident = self.ident();
        self.signal(CONNECTION_REQUEST, ident, &[psm.to_le_bytes(), local.to_le_bytes()].concat());
    }

    /// Queues data on the open channel to `psm`.
    pub fn send(&mut self, psm: u16, data: Vec<u8>) {
        match self.channel(psm).filter(|channel| channel.open()) {
            Some(channel) => self.outbox.push((channel.remote, data)),
            None => debug!("bluetooth: no open channel to PSM {psm:#06x}; dropped {} bytes", data.len()),
        }
    }

    /// The link is now encrypted: channels the device asked for while it
    /// wasn't can open.
    pub fn set_secure(&mut self) {
        self.secure = true;
        let waiting: Vec<(u16, u16, u8)> = self
            .channels
            .iter_mut()
            .filter_map(|channel| Some((channel.local, channel.remote, channel.waiting_for_security.take()?)))
            .collect();
        for (local, remote, ident) in waiting {
            self.connection_response(ident, local, remote, SUCCESS, 0);
            self.configure(local);
        }
    }

    /// Handles a signaling frame: one or more commands.
    pub fn receive_signal(&mut self, mut data: &[u8]) {
        while let [code, ident, low, high, rest @ ..] = data {
            let length = usize::from(u16::from_le_bytes([*low, *high]));
            let Some(body) = rest.get(..length) else {
                self.errors.push(String::from("cut-short L2CAP signal"));
                return;
            };
            self.command(*code, *ident, body);
            data = &rest[length..];
        }
    }

    fn command(&mut self, code: u8, ident: u8, body: &[u8]) {
        let word = |at: usize| body.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        match code {
            CONNECTION_REQUEST => {
                let (psm, remote) = (word(0), word(2));
                if !self.served.contains(&psm) {
                    debug!("bluetooth: refused a channel to PSM {psm:#06x}");
                    self.connection_response(ident, 0, remote, PSM_NOT_SUPPORTED, 0);
                    return;
                }
                let local = self.allocate_cid();
                self.channels.push(Channel {
                    psm,
                    local,
                    remote,
                    configured_out: false,
                    configured_in: false,
                    config_sent: false,
                    waiting_for_security: (!self.secure).then_some(ident),
                });
                if self.secure {
                    self.connection_response(ident, local, remote, SUCCESS, 0);
                    self.configure(local);
                } else {
                    self.connection_response(ident, local, remote, PENDING, AUTHENTICATION_PENDING);
                }
            }
            CONNECTION_RESPONSE => {
                let (remote, local, result) = (word(0), word(2), word(4));
                match result {
                    SUCCESS => {
                        if let Some(channel) = self.channels.iter_mut().find(|channel| channel.local == local) {
                            channel.remote = remote;
                            self.configure(local);
                        }
                    }
                    PENDING => {}
                    _ => {
                        if let Some(index) = self.channels.iter().position(|channel| channel.local == local) {
                            let channel = self.channels.remove(index);
                            self.errors
                                .push(format!("the device refused a channel to PSM {:#06x} ({result})", channel.psm));
                        }
                    }
                }
            }
            CONFIGURE_REQUEST => {
                let (local, flags) = (word(0), word(2));
                let Some(channel) = self.channels.iter_mut().find(|channel| channel.local == local) else {
                    // Invalid CID.
                    self.signal(COMMAND_REJECT, ident, &[&[2, 0][..], &local.to_le_bytes(), &[0, 0]].concat());
                    return;
                };
                // Bit 0: more configuration follows in another request.
                if flags & 1 == 0 {
                    channel.configured_in = true;
                }
                let remote = channel.remote;
                // Whatever it asks (its MTU, mostly) is fine.
                self.signal(CONFIGURE_RESPONSE, ident, &[remote.to_le_bytes(), [flags as u8 & 1, 0], [0, 0]].concat());
                self.configure(local);
            }
            CONFIGURE_RESPONSE => {
                let (local, result) = (word(0), word(4));
                if let Some(channel) = self.channels.iter_mut().find(|channel| channel.local == local) {
                    if result == SUCCESS {
                        channel.configured_out = true;
                    } else {
                        let psm = channel.psm;
                        self.errors.push(format!("the device rejected the configuration of PSM {psm:#06x} ({result})"));
                    }
                }
            }
            DISCONNECTION_REQUEST => {
                let (local, remote) = (word(0), word(2));
                self.channels.retain(|channel| channel.local != local);
                self.signal(DISCONNECTION_RESPONSE, ident, &[local.to_le_bytes(), remote.to_le_bytes()].concat());
            }
            DISCONNECTION_RESPONSE => {
                let local = word(2);
                self.channels.retain(|channel| channel.local != local);
            }
            ECHO_REQUEST => self.signal(ECHO_RESPONSE, ident, body),
            INFORMATION_REQUEST => {
                let kind = word(0);
                let answer: Vec<u8> = match kind {
                    // Extended features: none.
                    2 => [&kind.to_le_bytes()[..], &[0, 0, 0, 0, 0, 0]].concat(),
                    // Fixed channels: signaling only.
                    3 => [&kind.to_le_bytes()[..], &[0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0]].concat(),
                    // Not supported.
                    _ => [&kind.to_le_bytes()[..], &[1, 0]].concat(),
                };
                self.signal(INFORMATION_RESPONSE, ident, &answer);
            }
            COMMAND_REJECT => debug!("bluetooth: the device rejected an L2CAP command: {body:02x?}"),
            ECHO_RESPONSE | INFORMATION_RESPONSE => {}
            // Command not understood.
            _ => self.signal(COMMAND_REJECT, ident, &[0, 0]),
        }
    }

    fn connection_response(&mut self, ident: u8, local: u16, remote: u16, result: u16, status: u16) {
        let body = [local.to_le_bytes(), remote.to_le_bytes(), result.to_le_bytes(), status.to_le_bytes()].concat();
        self.signal(CONNECTION_RESPONSE, ident, &body);
    }

    /// Sends our configuration for channel `local` (once): the default MTU.
    fn configure(&mut self, local: u16) {
        let Some(channel) = self.channels.iter_mut().find(|channel| channel.local == local) else { return };
        if channel.config_sent || channel.remote == 0 {
            return;
        }
        channel.config_sent = true;
        let remote = channel.remote;
        let ident = self.ident();
        let body = [&remote.to_le_bytes()[..], &[0, 0], &[1, 2], &MTU.to_le_bytes()].concat();
        self.signal(CONFIGURE_REQUEST, ident, &body);
    }

    fn signal(&mut self, code: u8, ident: u8, body: &[u8]) {
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&[code, ident]);
        frame.extend_from_slice(&(body.len() as u16).to_le_bytes());
        frame.extend_from_slice(body);
        self.outbox.push((SIGNALING, frame));
    }

    fn ident(&mut self) -> u8 {
        let ident = self.next_ident;
        self.next_ident = self.next_ident.checked_add(1).unwrap_or(1);
        ident
    }

    fn allocate_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.checked_add(1).unwrap_or(FIRST_DYNAMIC_CID);
        cid
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    crate::ktest! {
        fn l2cap_channels_open_both_ways() {
            // We open the HID control channel: request, the device's
            // response, its configuration request, its response to ours.
            let mut l2cap = L2cap::new(&[0x11, 0x13]);
            l2cap.set_secure();
            l2cap.connect(0x11);
            let (cid, request) = l2cap.outbox.remove(0);
            assert_eq!(cid, SIGNALING);
            assert_eq!(request, vec![CONNECTION_REQUEST, 1, 4, 0, 0x11, 0, 0x40, 0]);
            l2cap.receive_signal(&[CONNECTION_RESPONSE, 1, 8, 0, 0x50, 0, 0x40, 0, 0, 0, 0, 0]);
            // Our configuration goes out once the device's CID is known.
            let (_, configure) = l2cap.outbox.remove(0);
            assert_eq!(configure[..8], [CONFIGURE_REQUEST, 2, 8, 0, 0x50, 0, 0, 0]);
            l2cap.receive_signal(&[CONFIGURE_REQUEST, 7, 4, 0, 0x40, 0, 0, 0]);
            let (_, response) = l2cap.outbox.remove(0);
            assert_eq!(response, vec![CONFIGURE_RESPONSE, 7, 6, 0, 0x50, 0, 0, 0, 0, 0]);
            assert!(!l2cap.is_open(0x11));
            l2cap.receive_signal(&[CONFIGURE_RESPONSE, 2, 6, 0, 0x40, 0, 0, 0, 0, 0]);
            assert!(l2cap.is_open(0x11));
            assert_eq!(l2cap.open_psm(0x40), Some(0x11));
            l2cap.send(0x11, vec![0x70]);
            assert_eq!(l2cap.outbox.remove(0), (0x50, vec![0x70]));

            // The device opens the interrupt channel before the link is
            // encrypted: pending until it is.
            let mut l2cap = L2cap::new(&[0x11, 0x13]);
            l2cap.receive_signal(&[CONNECTION_REQUEST, 3, 4, 0, 0x13, 0, 0x60, 0]);
            let (_, response) = l2cap.outbox.remove(0);
            assert_eq!(response, vec![CONNECTION_RESPONSE, 3, 8, 0, 0x40, 0, 0x60, 0, 1, 0, 1, 0]);
            l2cap.set_secure();
            let (_, response) = l2cap.outbox.remove(0);
            assert_eq!(response, vec![CONNECTION_RESPONSE, 3, 8, 0, 0x40, 0, 0x60, 0, 0, 0, 0, 0]);
            assert_eq!(l2cap.outbox.remove(0).1[0], CONFIGURE_REQUEST);
            // Services we don't offer are refused.
            l2cap.receive_signal(&[CONNECTION_REQUEST, 4, 4, 0, 0x01, 0, 0x61, 0]);
            let (_, response) = l2cap.outbox.remove(0);
            assert_eq!(response, vec![CONNECTION_RESPONSE, 4, 8, 0, 0, 0, 0x61, 0, 2, 0, 0, 0]);
        }
    }
}
