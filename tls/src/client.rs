//! A TLS 1.2 client (RFC 5246) that doesn't do its own I/O: the caller
//! hands it whatever bytes arrive and sends whatever it returns. So the same
//! code runs over TCP or inside EAP messages (PEAP).
//!
//! It offers AES-128-GCM with SHA-256 (RFC 5288), keyed either by an
//! X25519 exchange signed with the server's RSA key (ECDHE_RSA, preferred)
//! or by RSA encryption of the secret. Every TLS 1.2 server worth talking
//! to supports one of those.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use apk::hash::{sha1, sha256};
use apk::rsa::{Hash, PublicKey};

use crate::gcm::Gcm;
use crate::hmac::prf;
use crate::x509::{Certificate, KeyType};
use crate::x25519;

const CHANGE_CIPHER_SPEC: u8 = 20;
const ALERT: u8 = 21;
const HANDSHAKE: u8 = 22;
const APPLICATION_DATA: u8 = 23;

const CLIENT_HELLO: u8 = 1;
const SERVER_HELLO: u8 = 2;
const CERTIFICATE: u8 = 11;
const SERVER_KEY_EXCHANGE: u8 = 12;
const CERTIFICATE_REQUEST: u8 = 13;
const SERVER_HELLO_DONE: u8 = 14;
const CLIENT_KEY_EXCHANGE: u8 = 16;
const FINISHED: u8 = 20;

const ECDHE_RSA_AES128_GCM_SHA256: u16 = 0xC02F;
const RSA_AES128_GCM_SHA256: u16 = 0x009C;
const X25519: u16 = 0x001D;
const RSA_PKCS1_SHA256: u16 = 0x0401;
const RSA_PKCS1_SHA1: u16 = 0x0201;

/// The most plaintext a record may carry.
const MAX_FRAGMENT: usize = 16384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server sent a fatal alert (its description code).
    Alert(u8),
    /// The server broke the protocol, or chose something we didn't offer.
    Protocol(&'static str),
    /// A record failed authentication.
    BadRecord,
    /// The server's key exchange signature or Finished didn't check out.
    BadSignature,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Alert(code) => write!(f, "the server sent alert {code} ({})", alert_name(*code)),
            Error::Protocol(what) => write!(f, "TLS protocol error: {what}"),
            Error::BadRecord => write!(f, "a TLS record failed its integrity check"),
            Error::BadSignature => write!(f, "the server's TLS signature didn't verify"),
        }
    }
}

fn alert_name(code: u8) -> &'static str {
    match code {
        0 => "close notify",
        10 => "unexpected message",
        20 => "bad record MAC",
        40 => "handshake failure",
        42 => "bad certificate",
        46 => "certificate unknown",
        47 => "illegal parameter",
        48 => "unknown CA",
        50 => "decode error",
        51 => "decrypt error",
        70 => "protocol version",
        71 => "insufficient security",
        80 => "internal error",
        _ => "?",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    ServerHello,
    Certificate,
    /// ServerKeyExchange, CertificateRequest or ServerHelloDone.
    ServerHelloDone,
    ChangeCipherSpec,
    Finished,
    Established,
    Closed,
}

/// One direction's record protection.
struct Cipher {
    gcm: Gcm,
    /// The implicit part of the nonce.
    salt: [u8; 4],
    sequence: u64,
}

impl Cipher {
    fn new(key: &[u8], salt: &[u8]) -> Self {
        Self { gcm: Gcm::new(key.try_into().unwrap()), salt: salt.try_into().unwrap(), sequence: 0 }
    }

    fn nonce(&self, explicit: &[u8]) -> [u8; 12] {
        let mut nonce = [0; 12];
        nonce[..4].copy_from_slice(&self.salt);
        nonce[4..].copy_from_slice(explicit);
        nonce
    }

    fn additional_data(&self, kind: u8, length: usize) -> [u8; 13] {
        let mut aad = [0; 13];
        aad[..8].copy_from_slice(&self.sequence.to_be_bytes());
        aad[8] = kind;
        aad[9..11].copy_from_slice(&[3, 3]);
        aad[11..].copy_from_slice(&(length as u16).to_be_bytes());
        aad
    }

    fn seal(&mut self, kind: u8, plaintext: &[u8]) -> Vec<u8> {
        let explicit = self.sequence.to_be_bytes();
        let sealed = self.gcm.seal(&self.nonce(&explicit), &self.additional_data(kind, plaintext.len()), plaintext);
        self.sequence += 1;
        [&explicit[..], &sealed].concat()
    }

    fn open(&mut self, kind: u8, payload: &[u8]) -> Option<Vec<u8>> {
        let explicit = payload.get(..8)?;
        let length = payload.len().checked_sub(8 + 16)?;
        let plaintext = self.gcm.open(&self.nonce(explicit), &self.additional_data(kind, length), &payload[8..])?;
        self.sequence += 1;
        Some(plaintext)
    }
}

/// What a call to [`Client::receive`] produced.
#[derive(Default)]
pub struct Output {
    /// Bytes to send to the server.
    pub send: Vec<u8>,
    /// Application data from the server.
    pub data: Vec<u8>,
}

pub struct Client {
    random: fn(&mut [u8]),
    server_name: Option<String>,
    state: State,
    suite: u16,
    client_random: [u8; 32],
    server_random: [u8; 32],
    /// Every handshake message so far, for the Finished messages.
    transcript: Vec<u8>,
    /// Received bytes not yet a whole record.
    incoming: Vec<u8>,
    /// Handshake bytes not yet a whole message.
    handshake: Vec<u8>,
    certificates: Vec<Vec<u8>>,
    /// The server's X25519 public key, from ServerKeyExchange.
    server_share: Option<[u8; 32]>,
    certificate_requested: bool,
    master: [u8; 48],
    write: Option<Cipher>,
    read: Option<Cipher>,
    /// The server's cipher, waiting for its ChangeCipherSpec.
    next_read: Option<Cipher>,
}

fn handshake_message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(body.len() + 4);
    message.push(kind);
    message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    message.extend_from_slice(body);
    message
}

fn extension(kind: u16, body: &[u8]) -> Vec<u8> {
    [&kind.to_be_bytes()[..], &(body.len() as u16).to_be_bytes(), body].concat()
}

/// Reads big-endian numbers and length-prefixed fields off the front.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, count: usize) -> Result<&'a [u8], Error> {
        let (front, rest) = self.0.split_at_checked(count).ok_or(Error::Protocol("message too short"))?;
        self.0 = rest;
        Ok(front)
    }

    fn number(&mut self, size: usize) -> Result<usize, Error> {
        Ok(self.bytes(size)?.iter().fold(0, |value, &byte| value << 8 | usize::from(byte)))
    }

    fn vector(&mut self, length_size: usize) -> Result<&'a [u8], Error> {
        let length = self.number(length_size)?;
        self.bytes(length)
    }
}

impl Client {
    /// `server_name` goes in the server name extension (for HTTPS);
    /// `random` fills buffers with secure random bytes.
    pub fn new(server_name: Option<&str>, random: fn(&mut [u8])) -> Self {
        Self {
            random,
            server_name: server_name.map(String::from),
            state: State::ServerHello,
            suite: 0,
            client_random: [0; 32],
            server_random: [0; 32],
            transcript: Vec::new(),
            incoming: Vec::new(),
            handshake: Vec::new(),
            certificates: Vec::new(),
            server_share: None,
            certificate_requested: false,
            master: [0; 48],
            write: None,
            read: None,
            next_read: None,
        }
    }

    /// The ClientHello, to send first.
    pub fn start(&mut self) -> Vec<u8> {
        (self.random)(&mut self.client_random);
        let mut body = Vec::new();
        body.extend_from_slice(&[3, 3]);
        body.extend_from_slice(&self.client_random);
        body.push(0); // no session id
        let suites = [ECDHE_RSA_AES128_GCM_SHA256, RSA_AES128_GCM_SHA256];
        body.extend_from_slice(&(suites.len() as u16 * 2).to_be_bytes());
        for suite in suites {
            body.extend_from_slice(&suite.to_be_bytes());
        }
        body.extend_from_slice(&[1, 0]); // no compression
        let mut extensions = Vec::new();
        if let Some(name) = &self.server_name {
            let entry = [&[0][..], &(name.len() as u16).to_be_bytes(), name.as_bytes()].concat();
            let list = [&(entry.len() as u16).to_be_bytes()[..], &entry].concat();
            extensions.extend(extension(0, &list));
        }
        extensions.extend(extension(10, &[0, 2, (X25519 >> 8) as u8, X25519 as u8]));
        extensions.extend(extension(11, &[1, 0]));
        let algorithms = [RSA_PKCS1_SHA256, RSA_PKCS1_SHA1];
        let mut list = (algorithms.len() as u16 * 2).to_be_bytes().to_vec();
        for algorithm in algorithms {
            list.extend_from_slice(&algorithm.to_be_bytes());
        }
        extensions.extend(extension(13, &list));
        extensions.extend(extension(0xFF01, &[0])); // secure renegotiation
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let message = handshake_message(CLIENT_HELLO, &body);
        self.transcript.extend_from_slice(&message);
        // Version 1.0 in the record header, as most clients do, for old
        // servers that choke on anything else there.
        record(HANDSHAKE, [3, 1], &message)
    }

    pub fn established(&self) -> bool {
        self.state == State::Established
    }

    pub fn closed(&self) -> bool {
        self.state == State::Closed
    }

    /// The server's certificates, its own first.
    pub fn certificates(&self) -> &[Vec<u8>] {
        &self.certificates
    }

    /// The negotiated cipher suite's name.
    pub fn suite_name(&self) -> &'static str {
        match self.suite {
            ECDHE_RSA_AES128_GCM_SHA256 => "ECDHE-RSA-AES128-GCM-SHA256",
            RSA_AES128_GCM_SHA256 => "RSA-AES128-GCM-SHA256",
            _ => "none",
        }
    }

    /// Keying material derived from the session (RFC 5705 without a
    /// context, which is how EAP methods like PEAP derive their keys).
    pub fn export(&self, label: &[u8], len: usize) -> Vec<u8> {
        prf(&self.master, label, &[&self.client_random[..], &self.server_random].concat(), len)
    }

    /// Encrypts application data into records.
    pub fn send(&mut self, data: &[u8]) -> Vec<u8> {
        let cipher = self.write.as_mut().expect("send before the handshake finished");
        let mut out = Vec::new();
        for chunk in data.chunks(MAX_FRAGMENT) {
            out.extend(record(APPLICATION_DATA, [3, 3], &cipher.seal(APPLICATION_DATA, chunk)));
        }
        out
    }

    /// Takes bytes from the server.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Output, Error> {
        self.incoming.extend_from_slice(bytes);
        let mut output = Output::default();
        while self.incoming.len() >= 5 {
            let length = usize::from(u16::from_be_bytes([self.incoming[3], self.incoming[4]]));
            if self.incoming.len() < 5 + length {
                break;
            }
            let record: Vec<u8> = self.incoming.drain(..5 + length).collect();
            let (kind, payload) = (record[0], &record[5..]);
            if record[1] != 3 {
                return Err(Error::Protocol("not a TLS record"));
            }
            let plaintext = match &mut self.read {
                Some(cipher) => cipher.open(kind, payload).ok_or(Error::BadRecord)?,
                None => payload.to_vec(),
            };
            match kind {
                HANDSHAKE => {
                    self.handshake.extend_from_slice(&plaintext);
                    self.handshake_messages(&mut output)?;
                }
                CHANGE_CIPHER_SPEC => {
                    if self.state != State::ChangeCipherSpec || plaintext != [1] {
                        return Err(Error::Protocol("unexpected ChangeCipherSpec"));
                    }
                    self.read = self.next_read.take();
                    self.state = State::Finished;
                }
                ALERT => {
                    let (level, code) =
                        (plaintext.first().copied().unwrap_or(2), plaintext.get(1).copied().unwrap_or(0));
                    if code == 0 {
                        self.state = State::Closed;
                    } else if level == 2 {
                        return Err(Error::Alert(code));
                    }
                }
                APPLICATION_DATA if self.state == State::Established => output.data.extend_from_slice(&plaintext),
                _ => return Err(Error::Protocol("unexpected record type")),
            }
        }
        Ok(output)
    }

    fn handshake_messages(&mut self, output: &mut Output) -> Result<(), Error> {
        while self.handshake.len() >= 4 {
            let length = usize::from(self.handshake[1]) << 16
                | usize::from(self.handshake[2]) << 8
                | usize::from(self.handshake[3]);
            if self.handshake.len() < 4 + length {
                break;
            }
            let message: Vec<u8> = self.handshake.drain(..4 + length).collect();
            let (kind, body) = (message[0], &message[4..]);
            match (self.state, kind) {
                (State::ServerHello, SERVER_HELLO) => self.server_hello(body)?,
                (State::Certificate, CERTIFICATE) => self.certificate(body)?,
                (State::ServerHelloDone, SERVER_KEY_EXCHANGE) if self.suite == ECDHE_RSA_AES128_GCM_SHA256 => {
                    self.server_key_exchange(body)?
                }
                (State::ServerHelloDone, CERTIFICATE_REQUEST) => self.certificate_requested = true,
                (State::ServerHelloDone, SERVER_HELLO_DONE) => {
                    self.transcript.extend_from_slice(&message);
                    output.send.extend(self.client_flight()?);
                    continue;
                }
                (State::Finished, FINISHED) => {
                    let expected = prf(&self.master, b"server finished", &sha256(&self.transcript), 12);
                    if body != expected {
                        return Err(Error::BadSignature);
                    }
                    self.state = State::Established;
                }
                (_, 0) => continue, // HelloRequest: ignored, and not in the transcript
                _ => return Err(Error::Protocol("unexpected handshake message")),
            }
            self.transcript.extend_from_slice(&message);
        }
        Ok(())
    }

    fn server_hello(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut reader = Reader(body);
        if reader.bytes(2)? != [3, 3] {
            return Err(Error::Protocol("the server doesn't speak TLS 1.2"));
        }
        self.server_random.copy_from_slice(reader.bytes(32)?);
        reader.vector(1)?; // session id
        self.suite = reader.number(2)? as u16;
        if ![ECDHE_RSA_AES128_GCM_SHA256, RSA_AES128_GCM_SHA256].contains(&self.suite) {
            return Err(Error::Protocol("the server chose a cipher suite we didn't offer"));
        }
        if reader.number(1)? != 0 {
            return Err(Error::Protocol("the server chose compression"));
        }
        self.state = State::Certificate;
        Ok(())
    }

    fn certificate(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut list = Reader(Reader(body).vector(3)?);
        while !list.0.is_empty() {
            self.certificates.push(list.vector(3)?.to_vec());
        }
        let leaf = self.certificates.first().ok_or(Error::Protocol("the server sent no certificate"))?;
        let parsed = Certificate::parse(leaf).ok_or(Error::Protocol("unreadable server certificate"))?;
        if parsed.key_type != KeyType::Rsa {
            return Err(Error::Protocol("the server's certificate doesn't have an RSA key"));
        }
        self.state = State::ServerHelloDone;
        Ok(())
    }

    fn server_public_key(&self) -> Result<PublicKey, Error> {
        let leaf = Certificate::parse(&self.certificates[0]).ok_or(Error::Protocol("unreadable server certificate"))?;
        PublicKey::from_der(leaf.public_key).ok_or(Error::Protocol("unusable RSA key"))
    }

    fn server_key_exchange(&mut self, body: &[u8]) -> Result<(), Error> {
        let mut reader = Reader(body);
        if reader.number(1)? != 3 || reader.number(2)? != usize::from(X25519) {
            return Err(Error::Protocol("the server chose a curve we didn't offer"));
        }
        let share: [u8; 32] = reader.vector(1)?.try_into().map_err(|_| Error::Protocol("bad X25519 key"))?;
        let params = &body[..body.len() - reader.0.len()];
        let algorithm = reader.number(2)? as u16;
        let signature = reader.vector(2)?;
        let signed = [&self.client_random[..], &self.server_random, params].concat();
        let key = self.server_public_key()?;
        let good = match algorithm {
            RSA_PKCS1_SHA256 => key.verify(signature, Hash::Sha256, &sha256(&signed)),
            RSA_PKCS1_SHA1 => key.verify(signature, Hash::Sha1, &sha1(&signed)),
            _ => return Err(Error::Protocol("the server signed with an algorithm we didn't offer")),
        };
        if !good {
            return Err(Error::BadSignature);
        }
        self.server_share = Some(share);
        Ok(())
    }

    /// Our answer to ServerHelloDone: (an empty Certificate,)
    /// ClientKeyExchange, ChangeCipherSpec, Finished.
    fn client_flight(&mut self) -> Result<Vec<u8>, Error> {
        let mut messages = Vec::new();
        if self.certificate_requested {
            messages.extend(handshake_message(CERTIFICATE, &[0, 0, 0]));
        }
        let (exchange, premaster) = if self.suite == ECDHE_RSA_AES128_GCM_SHA256 {
            let server = self.server_share.ok_or(Error::Protocol("no ServerKeyExchange"))?;
            let mut private = [0; 32];
            (self.random)(&mut private);
            let shared = x25519::x25519(&private, &server);
            if shared == [0; 32] {
                return Err(Error::Protocol("bad X25519 key"));
            }
            let public = x25519::public_key(&private);
            ([&[32][..], &public].concat(), shared.to_vec())
        } else {
            let mut premaster = [0u8; 48];
            (self.random)(&mut premaster);
            premaster[..2].copy_from_slice(&[3, 3]);
            let fill = self.random;
            let mut random = |buffer: &mut [u8]| fill(buffer);
            let encrypted = self
                .server_public_key()?
                .encrypt(&premaster, &mut random)
                .ok_or(Error::Protocol("RSA key too small"))?;
            ([&(encrypted.len() as u16).to_be_bytes()[..], &encrypted].concat(), premaster.to_vec())
        };
        messages.extend(handshake_message(CLIENT_KEY_EXCHANGE, &exchange));
        self.transcript.extend_from_slice(&messages);

        let randoms = [&self.client_random[..], &self.server_random].concat();
        self.master.copy_from_slice(&prf(&premaster, b"master secret", &randoms, 48));
        let server_first = [&self.server_random[..], &self.client_random].concat();
        let keys = prf(&self.master, b"key expansion", &server_first, 40);
        let mut write = Cipher::new(&keys[..16], &keys[32..36]);
        self.next_read = Some(Cipher::new(&keys[16..32], &keys[36..40]));

        let verify = prf(&self.master, b"client finished", &sha256(&self.transcript), 12);
        let finished = handshake_message(FINISHED, &verify);
        self.transcript.extend_from_slice(&finished);

        let mut out = record(HANDSHAKE, [3, 3], &messages);
        out.extend(record(CHANGE_CIPHER_SPEC, [3, 3], &[1]));
        out.extend(record(HANDSHAKE, [3, 3], &write.seal(HANDSHAKE, &finished)));
        self.write = Some(write);
        self.state = State::ChangeCipherSpec;
        Ok(out)
    }
}

fn record(kind: u8, version: [u8; 2], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(kind);
    out.extend_from_slice(&version);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}
