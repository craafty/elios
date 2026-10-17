//! The PEAP peer against a real RADIUS server (FreeRADIUS), the way an
//! access point relays it: each EAP packet goes in an Access-Request, and
//! the Access-Accept carries the key the access point would get (its
//! MS-MPPE-Recv-Key), which must equal ours.
//!
//! Needs a server: set `RADIUS_SERVER=host:port` (secret `testing123`, a
//! user `testuser` with password `testpass`); skipped otherwise.

use std::io::Read;
use std::net::UdpSocket;
use std::time::Duration;

use tls::peap::{Peer, Reply};

const SECRET: &[u8] = b"testing123";

fn random(buffer: &mut [u8]) {
    std::fs::File::open("/dev/urandom").unwrap().read_exact(buffer).unwrap();
}

/// MD5 (RFC 1321), which RADIUS uses everywhere.
fn md5(parts: &[&[u8]]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let mut message: Vec<u8> = parts.concat();
    let bits = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());
    let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in message.chunks(64) {
        let m: Vec<u32> = block.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g]).rotate_left(S[i]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(rotated);
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0; 16];
    for (out, word) in digest.chunks_mut(4).zip(state) {
        out.copy_from_slice(&word.to_le_bytes());
    }
    digest
}

fn hmac_md5(key: &[u8], message: &[u8]) -> [u8; 16] {
    let mut padded = [0u8; 64];
    padded[..key.len()].copy_from_slice(key);
    let inner: Vec<u8> = padded.iter().map(|b| b ^ 0x36).collect();
    let outer: Vec<u8> = padded.iter().map(|b| b ^ 0x5c).collect();
    md5(&[&outer, &md5(&[&inner, message])])
}

fn attribute(packet: &mut Vec<u8>, kind: u8, value: &[u8]) {
    packet.extend_from_slice(&[kind, value.len() as u8 + 2]);
    packet.extend_from_slice(value);
}

fn attributes(packet: &[u8]) -> Vec<(u8, &[u8])> {
    let mut found = Vec::new();
    let mut rest = &packet[20..];
    while rest.len() >= 2 && rest[1] >= 2 {
        let length = usize::from(rest[1]);
        found.push((rest[0], &rest[2..length]));
        rest = &rest[length..];
    }
    found
}

/// An Access-Request carrying `eap` (and the server's State).
fn request(id: u8, authenticator: &[u8; 16], eap: &[u8], state: Option<&[u8]>) -> Vec<u8> {
    let mut packet = vec![1, id, 0, 0];
    packet.extend_from_slice(authenticator);
    attribute(&mut packet, 1, b"testuser");
    for chunk in eap.chunks(253) {
        attribute(&mut packet, 79, chunk);
    }
    if let Some(state) = state {
        attribute(&mut packet, 24, state);
    }
    attribute(&mut packet, 80, &[0; 16]);
    let length = packet.len() as u16;
    packet[2..4].copy_from_slice(&length.to_be_bytes());
    let signature = hmac_md5(SECRET, &packet);
    let at = packet.len() - 16;
    packet[at..].copy_from_slice(&signature);
    packet
}

/// MS-MPPE-Recv-Key (RFC 2548): the access point's copy of our PMK.
fn recv_key(value: &[u8], authenticator: &[u8; 16]) -> Vec<u8> {
    let (salt, encrypted) = value.split_at(2);
    let mut plain = Vec::new();
    let mut previous: Vec<u8> = [authenticator.as_slice(), salt].concat();
    for block in encrypted.chunks(16) {
        let mask = md5(&[SECRET, &previous]);
        plain.extend(block.iter().zip(mask).map(|(c, m)| c ^ m));
        previous = block.to_vec();
    }
    let length = usize::from(plain[0]);
    plain[1..1 + length].to_vec()
}

/// Runs a whole sign-in; returns our reply to the last packet and the
/// key from the Access-Accept, if any.
fn sign_in(server: &str, password: &str) -> (Reply, Option<Vec<u8>>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(server).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut peer = Peer::new("testuser", password, random);
    // The access point's identity request.
    let mut reply = peer.receive(&[1, 0, 0, 5, 1]);
    let mut state: Option<Vec<u8>> = None;
    for id in 0..=255u8 {
        let Reply::Send(eap) = &reply else { break };
        let mut authenticator = [0; 16];
        random(&mut authenticator);
        socket.send(&request(id, &authenticator, eap, state.as_deref())).unwrap();
        let mut buffer = [0; 4096];
        let count = socket.recv(&mut buffer).unwrap();
        let answer = &buffer[..count];
        let found = attributes(answer);
        let eap: Vec<u8> = found.iter().filter(|(kind, _)| *kind == 79).flat_map(|(_, v)| v.to_vec()).collect();
        state = found.iter().find(|(kind, _)| *kind == 24).map(|(_, v)| v.to_vec());
        reply = peer.receive(&eap);
        for note in peer.take_notes() {
            eprintln!("  {note}");
        }
        if answer[0] != 11 {
            // Accept or reject: the conversation is over.
            let key = found
                .iter()
                .find(|(kind, v)| *kind == 26 && v[..4] == 311u32.to_be_bytes() && v[4] == 17)
                .map(|(_, v)| recv_key(&v[6..], &authenticator));
            return (reply, key);
        }
    }
    (reply, None)
}

#[test]
fn signs_in_with_freeradius() {
    let Ok(server) = std::env::var("RADIUS_SERVER") else {
        eprintln!("RADIUS_SERVER isn't set; skipping");
        return;
    };
    let (reply, key) = sign_in(&server, "testpass");
    let Reply::Success(pmk) = reply else { panic!("sign-in failed: {reply:?}") };
    assert_eq!(key.as_deref(), Some(&pmk[..]), "our PMK differs from the access point's");

    let (reply, key) = sign_in(&server, "wrongpass");
    assert!(matches!(&reply, Reply::Failure(why) if why.contains("password")), "{reply:?}");
    assert_eq!(key, None);
}

#[test]
fn md5_known_answers() {
    let hex = |d: [u8; 16]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex(md5(&[b""])), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(hex(md5(&[b"The quick brown fox ", b"jumps over the lazy dog"])), "9e107d9d372bb6826bd81d3542a419d6");
}
