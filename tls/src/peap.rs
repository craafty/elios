//! An EAP peer (RFC 3748) for PEAP version 0 with MS-CHAPv2 inside, the
//! WPA2-Enterprise method universities and companies use (eduroam, most
//! campus networks): the access point relays EAP between us and a RADIUS
//! server; we open a TLS session with the server inside EAP messages, then
//! prove the password through it. Both sides then derive the same key,
//! which becomes the Wi-Fi pairwise master key.
//!
//! Like [`Client`], it does no I/O: the driver feeds it EAP packets and
//! sends what it returns.
//!
//! PEAPv0 quirks (draft-kamath-pppext-peapv0, as Windows and FreeRADIUS do
//! it): inner EAP messages travel without their 4-byte header, except the
//! TLV (type 33) messages that end the inner conversation.

use alloc::string::String;
use alloc::vec::Vec;

use crate::client::Client;
use crate::mschapv2;

const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const SUCCESS: u8 = 3;
const FAILURE: u8 = 4;

const IDENTITY: u8 = 1;
const NOTIFICATION: u8 = 2;
const NAK: u8 = 3;
const PEAP: u8 = 25;
const MSCHAPV2: u8 = 26;
const TLV: u8 = 33;

/// PEAP flags: length included, more fragments, start.
const LENGTH_INCLUDED: u8 = 0x80;
const MORE_FRAGMENTS: u8 = 0x40;
const START: u8 = 0x20;

/// The most TLS data per EAP message; the access point's link MTU (1500)
/// less the EAPOL, EAP and PEAP headers, with room to spare.
const FRAGMENT: usize = 1000;

const RESULT_TLV: u16 = 3;
const CRYPTO_BINDING_TLV: u16 = 12;

/// What to do with an EAP packet's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Send this EAP packet.
    Send(Vec<u8>),
    /// Nothing to send (a duplicate or something not for us).
    Nothing,
    /// The server accepted us: the 32-byte pairwise master key.
    Success([u8; 32]),
    /// Authentication failed, and why.
    Failure(String),
}

pub struct Peer {
    identity: String,
    password: String,
    random: fn(&mut [u8]),
    tls: Option<Client>,
    /// TLS bytes from the server, waiting for the last fragment.
    incoming: Vec<u8>,
    /// TLS bytes for the server, waiting for its acknowledgements.
    outgoing: Vec<u8>,
    /// The last request's identifier and our answer, for retransmissions.
    last: Option<(u8, Vec<u8>)>,
    /// MS-CHAPv2's challenges and our response, to check the server's.
    authenticator_challenge: [u8; 16],
    peer_challenge: [u8; 16],
    nt_response: [u8; 24],
    /// Whether the server proved it knows the password.
    inner_success: bool,
    /// Whether the server said the inner authentication succeeded (TLV).
    result_success: bool,
    /// Why the inner authentication failed, if it did.
    inner_failure: Option<String>,
    /// Progress for the log, taken by [`Peer::take_notes`].
    notes: Vec<String>,
}

fn packet(code: u8, id: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&[code, id]);
    out.extend_from_slice(&(body.len() as u16 + 4).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// MS-CHAPv2's failure codes (RFC 2759 section 6).
fn failure_text(message: &str) -> String {
    let code = message.split_whitespace().find_map(|field| field.strip_prefix("E=")).unwrap_or("?");
    let meaning = match code {
        "691" => "wrong user name or password",
        "646" => "the account may not sign in at this time",
        "647" => "the account is disabled",
        "648" => "the password has expired",
        "649" => "the account may not use this network",
        _ => "refused",
    };
    alloc::format!("the server refused the sign-in: {meaning} (MS-CHAPv2 error {code})")
}

impl Peer {
    pub fn new(identity: &str, password: &str, random: fn(&mut [u8])) -> Self {
        Self {
            identity: String::from(identity),
            password: String::from(password),
            random,
            tls: None,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            last: None,
            authenticator_challenge: [0; 16],
            peer_challenge: [0; 16],
            nt_response: [0; 24],
            inner_success: false,
            result_success: false,
            inner_failure: None,
            notes: Vec::new(),
        }
    }

    /// What happened since the last call, for the log.
    pub fn take_notes(&mut self) -> Vec<String> {
        core::mem::take(&mut self.notes)
    }

    /// The TLS session (for its certificates and cipher suite).
    pub fn tls(&self) -> Option<&Client> {
        self.tls.as_ref()
    }

    /// Handles one EAP packet from the authenticator.
    pub fn receive(&mut self, eap: &[u8]) -> Reply {
        let (Some(&code), Some(&id)) = (eap.first(), eap.get(1)) else { return Reply::Nothing };
        let length = eap.get(2..4).map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
        let Some(eap) = eap.get(..length) else { return Reply::Nothing };
        match code {
            SUCCESS => {
                return match (&self.tls, self.result_success && self.inner_success) {
                    (Some(tls), true) => {
                        let msk = tls.export(b"client EAP encryption", 64);
                        Reply::Success(msk[..32].try_into().unwrap())
                    }
                    _ => Reply::Failure(String::from("the server said success before we finished signing in")),
                };
            }
            FAILURE => {
                return Reply::Failure(
                    self.inner_failure.clone().unwrap_or_else(|| String::from("the server refused the sign-in")),
                );
            }
            REQUEST if eap.len() > 4 => {}
            _ => return Reply::Nothing,
        }
        if let Some((last_id, answer)) = &self.last
            && *last_id == id
        {
            // A retransmission: our answer was lost.
            return Reply::Send(answer.clone());
        }
        let answer = match eap[4] {
            IDENTITY => {
                self.notes.push(alloc::format!("EAP identity request; answering {:?}", self.identity));
                packet(RESPONSE, id, &[&[IDENTITY][..], self.identity.as_bytes()].concat())
            }
            NOTIFICATION => {
                self.notes.push(alloc::format!("EAP notification: {}", String::from_utf8_lossy(&eap[5..])));
                packet(RESPONSE, id, &[NOTIFICATION])
            }
            PEAP => match self.peap(id, &eap[5..]) {
                Ok(answer) => answer,
                Err(error) => return Reply::Failure(error),
            },
            other => {
                self.notes.push(alloc::format!("the server proposed EAP method {other}; asking for PEAP"));
                packet(RESPONSE, id, &[NAK, PEAP])
            }
        };
        self.last = Some((id, answer.clone()));
        Reply::Send(answer)
    }

    /// A PEAP request: TLS data (maybe in fragments) or the start signal.
    fn peap(&mut self, id: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        let flags = *data.first().ok_or("empty PEAP message")?;
        let mut payload = &data[1..];
        if flags & LENGTH_INCLUDED != 0 {
            payload = payload.get(4..).ok_or("short PEAP message")?;
        }
        if flags & START != 0 {
            self.notes.push(alloc::format!("PEAP start (server version {}); starting TLS", flags & 7));
            let mut tls = Client::new(None, self.random);
            self.outgoing = tls.start();
            self.tls = Some(tls);
            self.incoming.clear();
            return Ok(self.next_fragment(id, true));
        }
        let tls = self.tls.as_mut().ok_or("PEAP data before PEAP start")?;
        if payload.is_empty() && !self.outgoing.is_empty() {
            // The server acknowledged a fragment; send the next.
            return Ok(self.next_fragment(id, false));
        }
        self.incoming.extend_from_slice(payload);
        if flags & MORE_FRAGMENTS != 0 {
            return Ok(packet(RESPONSE, id, &[PEAP, 0]));
        }
        let was_established = tls.established();
        let output = tls.receive(&core::mem::take(&mut self.incoming)).map_err(|error| alloc::format!("{error}"))?;
        let mut send = output.send;
        if !was_established && tls.established() {
            self.notes.push(alloc::format!("TLS established ({})", tls.suite_name()));
        }
        if !output.data.is_empty() {
            let inner = self.inner(id, &output.data)?;
            let tls = self.tls.as_mut().expect("checked above");
            send.extend(tls.send(&inner));
        }
        self.outgoing = send;
        Ok(self.next_fragment(id, true))
    }

    /// The next piece of our TLS data, or an empty acknowledgement.
    /// `first`: the start of a new message, whose first fragment carries
    /// the total length if there will be more.
    fn next_fragment(&mut self, id: u8, first: bool) -> Vec<u8> {
        let total = self.outgoing.len();
        let rest = self.outgoing.split_off(total.min(FRAGMENT));
        let chunk = core::mem::replace(&mut self.outgoing, rest);
        let mut body = alloc::vec![PEAP, 0];
        if !self.outgoing.is_empty() {
            body[1] |= MORE_FRAGMENTS;
            if first {
                body[1] |= LENGTH_INCLUDED;
                body.extend_from_slice(&(total as u32).to_be_bytes());
            }
        }
        body.extend_from_slice(&chunk);
        packet(RESPONSE, id, &body)
    }

    /// Phase 2: an inner EAP message from inside the TLS tunnel. Returns
    /// the plaintext to send back through it.
    fn inner(&mut self, outer_id: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        // A whole EAP packet (TLV messages), or one without its header.
        let whole = data.len() >= 5
            && data[0] == REQUEST
            && usize::from(u16::from_be_bytes([data[2], data[3]])) == data.len()
            && data[4] == TLV;
        let request = if whole { data.to_vec() } else { packet(REQUEST, outer_id, data) };
        let id = request[1];
        let kind = *request.get(4).ok_or("empty inner request")?;
        let body = &request[5..];
        let response = match kind {
            IDENTITY => {
                self.notes.push(String::from("inner identity request"));
                packet(RESPONSE, id, &[&[IDENTITY][..], self.identity.as_bytes()].concat())
            }
            MSCHAPV2 => self.mschapv2(id, body)?,
            TLV => return Ok(self.tlv(id, body)),
            other => {
                self.notes.push(alloc::format!("inner method {other} proposed; asking for MS-CHAPv2"));
                packet(RESPONSE, id, &[NAK, MSCHAPV2])
            }
        };
        // Without the header, as PEAPv0 sends everything but TLVs.
        Ok(response[4..].to_vec())
    }

    fn mschapv2(&mut self, id: u8, body: &[u8]) -> Result<Vec<u8>, String> {
        let opcode = *body.first().ok_or("empty MS-CHAPv2 message")?;
        match opcode {
            // Challenge: id, length, value size (16), challenge, server name.
            1 => {
                let ms_id = *body.get(1).ok_or("short MS-CHAPv2 challenge")?;
                if body.get(4) != Some(&16) {
                    return Err(String::from("bad MS-CHAPv2 challenge"));
                }
                self.authenticator_challenge.copy_from_slice(body.get(5..21).ok_or("short MS-CHAPv2 challenge")?);
                (self.random)(&mut self.peer_challenge);
                self.nt_response = mschapv2::nt_response(
                    &self.authenticator_challenge,
                    &self.peer_challenge,
                    &self.identity,
                    &self.password,
                );
                self.notes.push(alloc::format!(
                    "MS-CHAPv2 challenge from {:?}; answering",
                    String::from_utf8_lossy(&body[21..])
                ));
                let ms_length = (4 + 1 + 49 + self.identity.len()) as u16;
                let mut response = alloc::vec![MSCHAPV2, 2, ms_id];
                response.extend_from_slice(&ms_length.to_be_bytes());
                response.push(49);
                response.extend_from_slice(&self.peer_challenge);
                response.extend_from_slice(&[0; 8]);
                response.extend_from_slice(&self.nt_response);
                response.push(0);
                response.extend_from_slice(self.identity.as_bytes());
                Ok(packet(RESPONSE, id, &response))
            }
            // Success: "S=<authenticator response> M=<message>".
            3 => {
                let message = String::from_utf8_lossy(body.get(4..).unwrap_or_default()).into_owned();
                let expected = mschapv2::authenticator_response(
                    &self.password,
                    &self.nt_response,
                    &self.peer_challenge,
                    &self.authenticator_challenge,
                    &self.identity,
                );
                if !message.starts_with(&expected) {
                    return Err(String::from("the server couldn't prove it knows the password (wrong network?)"));
                }
                self.notes.push(String::from("MS-CHAPv2 succeeded; the server proved it knows the password"));
                self.inner_success = true;
                Ok(packet(RESPONSE, id, &[MSCHAPV2, 3]))
            }
            // Failure: "E=691 R=0 C=... V=3 M=...".
            4 => {
                let message = String::from_utf8_lossy(body.get(4..).unwrap_or_default()).into_owned();
                self.notes.push(alloc::format!("MS-CHAPv2 failure: {message}"));
                self.inner_failure = Some(failure_text(&message));
                Ok(packet(RESPONSE, id, &[MSCHAPV2, 4]))
            }
            _ => Err(alloc::format!("unknown MS-CHAPv2 message {opcode}")),
        }
    }

    /// The server's verdict on the inner authentication, in TLVs: we agree
    /// (success only if MS-CHAPv2 succeeded).
    fn tlv(&mut self, id: u8, mut tlvs: &[u8]) -> Vec<u8> {
        let mut server_success = false;
        while tlvs.len() >= 4 {
            let kind = u16::from_be_bytes([tlvs[0], tlvs[1]]) & 0x3FFF;
            let length = usize::from(u16::from_be_bytes([tlvs[2], tlvs[3]]));
            let value = tlvs.get(4..4 + length).unwrap_or_default();
            match kind {
                RESULT_TLV => server_success = value == [0, 1],
                CRYPTO_BINDING_TLV => {
                    self.notes.push(String::from("the server sent a crypto binding TLV; not supported, ignored"))
                }
                other => self.notes.push(alloc::format!("TLV {other} ignored")),
            }
            tlvs = tlvs.get(4 + length..).unwrap_or_default();
        }
        let success = server_success && self.inner_success;
        self.result_success = success;
        if !server_success && self.inner_failure.is_none() {
            // Some servers (FreeRADIUS) skip MS-CHAPv2's failure message.
            self.inner_failure = Some(String::from("the server refused the sign-in: wrong user name or password?"));
        }
        self.notes.push(alloc::format!(
            "result TLV: server says {}, we say {}",
            if server_success { "success" } else { "failure" },
            if success { "success" } else { "failure" }
        ));
        let status: u16 = if success { 1 } else { 2 };
        let mut body = alloc::vec![TLV];
        body.extend_from_slice(&(0x8000 | RESULT_TLV).to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&status.to_be_bytes());
        packet(RESPONSE, id, &body)
    }
}
