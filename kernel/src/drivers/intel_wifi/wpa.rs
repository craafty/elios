//! WPA2-Personal: the 4-way handshake that turns the network's password
//! into keys, and CCMP, the AES encryption every data frame then uses.
//!
//! The password and network name give the "PMK" (PBKDF2, 4096 rounds of
//! HMAC-SHA1). In the handshake the access point and station swap random
//! numbers ("nonces"); each derives the same pairwise keys ("PTK") from the
//! PMK, both addresses and both nonces, and proves it with a MIC on its
//! messages. The access point then sends the group key (for broadcasts),
//! encrypted with part of the PTK. Nothing secret crosses the air.
//!
//! The card's firmware can encrypt too, but doing it here keeps the
//! firmware interface small; at Wi-Fi 4 speeds AES in software is cheap.

use alloc::vec::Vec;

use super::crypto::{self, Aes128};
use super::ieee80211::{OUR_RSN, PROTECTED};

// EAPOL-Key "key information" bits.
const VERSION_AES: u16 = 2;
const PAIRWISE: u16 = 1 << 3;
const INSTALL: u16 = 1 << 6;
const ACK: u16 = 1 << 7;
const MIC: u16 = 1 << 8;
const SECURE: u16 = 1 << 9;
const ENCRYPTED_DATA: u16 = 1 << 12;

/// Where things are in an EAPOL-Key frame (802.1X header included).
const KEY_INFO: usize = 5;
const REPLAY_COUNTER: usize = 9;
const NONCE: usize = 17;
const RSC: usize = 65;
const MIC_FIELD: usize = 81;
const KEY_DATA_LEN: usize = 97;
const KEY_DATA: usize = 99;

/// The pairwise keys: KCK signs handshake messages, KEK encrypts the group
/// key, TK encrypts data.
pub struct Ptk {
    kck: [u8; 16],
    kek: [u8; 16],
    pub tk: [u8; 16],
}

/// A group key, with its index (frames say which they use) and the packet
/// number the access point's broadcasts continue from.
#[derive(Clone)]
pub struct GroupKey {
    pub index: u8,
    pub key: [u8; 16],
    pub rsc: u64,
}

/// What a handshake message calls for.
pub enum Step {
    /// Send this EAPOL frame.
    Reply(Vec<u8>),
    /// Send this, then use these keys (the handshake is done).
    Done(Vec<u8>, [u8; 16], GroupKey),
    /// A new group key (the access point changes it now and then).
    NewGroupKey(Vec<u8>, GroupKey),
    /// Ignored, and why.
    Ignore(&'static str),
}

pub struct Handshake {
    pmk: [u8; 32],
    station: [u8; 6],
    access_point: [u8; 6],
    snonce: [u8; 32],
    anonce: Option<[u8; 32]>,
    ptk: Option<Ptk>,
    /// The last replay counter accepted (messages must increase it).
    replay: Option<u64>,
    /// The access point's RSN element, which message 3 must repeat.
    ap_rsn: Vec<u8>,
}

impl Handshake {
    pub fn new(pmk: [u8; 32], station: [u8; 6], access_point: [u8; 6], ap_rsn: Vec<u8>) -> Self {
        let mut snonce = [0; 32];
        crate::random::fill(&mut snonce);
        Self { pmk, station, access_point, snonce, anonce: None, ptk: None, replay: None, ap_rsn }
    }

    /// Handles an EAPOL frame from the access point (from the 802.1X
    /// header on).
    pub fn handle(&mut self, frame: &[u8]) -> Step {
        if frame.len() < KEY_DATA || frame[1] != 3 || (frame[4] != 2 && frame[4] != 254) {
            return Step::Ignore("not an EAPOL-Key frame");
        }
        let body_len = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
        let frame = &frame[..(4 + body_len).min(frame.len())];
        let info = u16::from_be_bytes([frame[KEY_INFO], frame[KEY_INFO + 1]]);
        let replay = u64::from_be_bytes(frame[REPLAY_COUNTER..REPLAY_COUNTER + 8].try_into().unwrap());
        let data_len = usize::from(u16::from_be_bytes([frame[KEY_DATA_LEN], frame[KEY_DATA_LEN + 1]]));
        let Some(key_data) = frame.get(KEY_DATA..KEY_DATA + data_len) else {
            return Step::Ignore("key data longer than the frame");
        };
        if info & 0x7 != VERSION_AES {
            return Step::Ignore("not the AES key descriptor version");
        }
        if info & ACK == 0 {
            return Step::Ignore("not from the authenticator");
        }
        if self.replay.is_some_and(|last| replay <= last) {
            return Step::Ignore("replayed message");
        }

        if info & PAIRWISE != 0 && info & MIC == 0 {
            // Message 1: the access point's nonce. Derive the keys and
            // answer with ours, signed.
            let anonce: [u8; 32] = frame[NONCE..NONCE + 32].try_into().unwrap();
            self.anonce = Some(anonce);
            self.ptk = Some(derive_ptk(&self.pmk, &self.station, &self.access_point, &self.snonce, &anonce));
            self.replay = Some(replay);
            let reply = self.message(PAIRWISE | MIC, replay, &self.snonce, &OUR_RSN);
            return Step::Reply(reply);
        }

        let Some(ptk) = &self.ptk else { return Step::Ignore("no message 1 yet") };
        if !verify_mic(&ptk.kck, frame) {
            return Step::Ignore("bad MIC (wrong password?)");
        }
        let data = if info & ENCRYPTED_DATA != 0 {
            match crypto::key_unwrap(&ptk.kek, key_data) {
                Some(data) => data,
                None => return Step::Ignore("the key data didn't decrypt"),
            }
        } else {
            key_data.to_vec()
        };
        let Some(mut group) = find_group_key(&data) else { return Step::Ignore("no group key") };
        group.rsc = u64::from_le_bytes(frame[RSC..RSC + 8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
        self.replay = Some(replay);

        if info & PAIRWISE != 0 {
            // Message 3: the same nonce as message 1 and the RSN element
            // from the beacon (else someone is downgrading the security).
            if self.anonce.as_ref().map(|nonce| &nonce[..]) != frame.get(NONCE..NONCE + 32) {
                return Step::Ignore("message 3's nonce differs from message 1's");
            }
            if !data.windows(self.ap_rsn.len()).any(|window| window == self.ap_rsn) {
                warn!("wifi: the access point's RSN element in message 3 differs from its beacon's");
            }
            if info & INSTALL == 0 {
                return Step::Ignore("message 3 without the install bit");
            }
            let reply = self.message(PAIRWISE | MIC | SECURE, replay, &[0; 32], &[]);
            Step::Done(reply, ptk.tk, group)
        } else {
            // Group key message 1: answer with message 2.
            let reply = self.message(MIC | SECURE, replay, &[0; 32], &[]);
            Step::NewGroupKey(reply, group)
        }
    }

    /// Builds a signed EAPOL-Key message to the access point.
    fn message(&self, flags: u16, replay: u64, nonce: &[u8; 32], key_data: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(KEY_DATA + key_data.len());
        let body_len = (KEY_DATA - 4 + key_data.len()) as u16;
        // 802.1X version 1, type "key", length; RSN key descriptor.
        frame.extend_from_slice(&[1, 3]);
        frame.extend_from_slice(&body_len.to_be_bytes());
        frame.push(2);
        frame.extend_from_slice(&(flags | VERSION_AES).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&replay.to_be_bytes());
        frame.extend_from_slice(nonce);
        // IV, RSC, reserved, MIC (filled in below).
        frame.resize(KEY_DATA_LEN, 0);
        frame.extend_from_slice(&(key_data.len() as u16).to_be_bytes());
        frame.extend_from_slice(key_data);
        if let Some(ptk) = &self.ptk {
            let mic = crypto::hmac_sha1(&ptk.kck, &[&frame]);
            frame[MIC_FIELD..MIC_FIELD + 16].copy_from_slice(&mic[..16]);
        }
        frame
    }
}

/// PTK = PRF-384(PMK, "Pairwise key expansion", the smaller address, the
/// larger, the smaller nonce, the larger).
fn derive_ptk(pmk: &[u8; 32], station: &[u8; 6], access_point: &[u8; 6], snonce: &[u8; 32], anonce: &[u8; 32]) -> Ptk {
    let mut data = Vec::with_capacity(76);
    data.extend_from_slice(station.min(access_point));
    data.extend_from_slice(station.max(access_point));
    data.extend_from_slice(snonce.min(anonce));
    data.extend_from_slice(snonce.max(anonce));
    let ptk = crypto::prf(pmk, b"Pairwise key expansion", &data, 48);
    Ptk {
        kck: ptk[..16].try_into().unwrap(),
        kek: ptk[16..32].try_into().unwrap(),
        tk: ptk[32..48].try_into().unwrap(),
    }
}

/// Checks a message's MIC: HMAC-SHA1 over the frame with the MIC zeroed.
fn verify_mic(kck: &[u8; 16], frame: &[u8]) -> bool {
    let mut copy = frame.to_vec();
    copy[MIC_FIELD..MIC_FIELD + 16].fill(0);
    let mic = crypto::hmac_sha1(kck, &[&copy]);
    mic[..16].iter().zip(&frame[MIC_FIELD..MIC_FIELD + 16]).fold(0, |difference, (a, b)| difference | (a ^ b)) == 0
}

/// The GTK "key data encapsulation" (vendor element 00-0F-AC:1): key
/// index, reserved, the key.
fn find_group_key(data: &[u8]) -> Option<GroupKey> {
    let mut rest = data;
    while rest.len() >= 2 {
        let (id, len) = (rest[0], usize::from(rest[1]));
        // Padding: 0xDD followed by zeros.
        if id == 0xDD && len == 0 {
            break;
        }
        let value = rest.get(2..2 + len)?;
        if id == 0xDD && value.len() >= 6 + 16 && value[..4] == [0x00, 0x0F, 0xAC, 1] {
            return Some(GroupKey { index: value[4] & 0x3, key: value[6..22].try_into().unwrap(), rsc: 0 });
        }
        rest = &rest[2 + len..];
    }
    None
}

/// The CCMP "additional authenticated data" and nonce for a frame: the
/// header with the bits that may change in flight (retry, power
/// management, more data, the sequence number) masked, and the sender,
/// priority and packet number.
fn ccmp_inputs(header: &[u8], pn: u64) -> (Vec<u8>, [u8; 13]) {
    let mut aad = Vec::with_capacity(30);
    let is_data = header[0] & 0x0C == 0x08;
    let kind = if is_data { header[0] & !0x70 } else { header[0] };
    aad.push(kind);
    aad.push(header[1] & !(0x08 | 0x10 | 0x20) | PROTECTED);
    aad.extend_from_slice(&header[4..22]);
    aad.extend_from_slice(&[header[22] & 0x0F, 0]);
    let mut priority = if is_data { 0 } else { 0x10 };
    let four_addresses = header[1] & 0x3 == 0x3;
    let mut offset = 24;
    if four_addresses {
        aad.extend_from_slice(&header[24..30]);
        offset = 30;
    }
    if is_data && header[0] & 0x80 != 0 {
        priority = header[offset] & 0x0F;
        aad.extend_from_slice(&[priority, 0]);
    }
    let mut nonce = [0u8; 13];
    nonce[0] = priority;
    nonce[1..7].copy_from_slice(&header[10..16]);
    nonce[7..].copy_from_slice(&pn.to_be_bytes()[2..]);
    (aad, nonce)
}

/// Encrypts a frame body: returns the CCMP header, the encrypted body and
/// the MIC, to follow `header` (which must have the protected bit set).
pub fn ccmp_encrypt(aes: &Aes128, header: &[u8], pn: u64, key_index: u8, body: &[u8]) -> Vec<u8> {
    let (aad, nonce) = ccmp_inputs(header, pn);
    let pn_bytes = pn.to_le_bytes();
    let mut out = Vec::with_capacity(body.len() + 16);
    out.extend_from_slice(&[pn_bytes[0], pn_bytes[1], 0, 0x20 | key_index << 6]);
    out.extend_from_slice(&pn_bytes[2..6]);
    out.extend_from_slice(body);
    let mic = crypto::ccm_encrypt(aes, &nonce, &aad, &mut out[8..]);
    out.extend_from_slice(&mic);
    out
}

/// The packet number and key index of a protected frame (whose header is
/// `header_len` bytes).
pub fn ccmp_header(frame: &[u8], header_len: usize) -> Option<(u64, u8)> {
    let h = frame.get(header_len..header_len + 8)?;
    if h[3] & 0x20 == 0 {
        return None;
    }
    let pn = u64::from_le_bytes([h[0], h[1], h[4], h[5], h[6], h[7], 0, 0]);
    Some((pn, h[3] >> 6))
}

/// Decrypts a protected frame's body in place; returns it (without the
/// CCMP header and MIC), or None if it was forged or damaged.
pub fn ccmp_decrypt<'a>(aes: &Aes128, frame: &'a mut [u8], header_len: usize, pn: u64) -> Option<&'a [u8]> {
    if frame.len() < header_len + 16 {
        return None;
    }
    let (aad, nonce) = ccmp_inputs(&frame[..header_len], pn);
    let len = frame.len();
    let mic: [u8; 8] = frame[len - 8..].try_into().unwrap();
    let body = &mut frame[header_len + 8..len - 8];
    crypto::ccm_decrypt(aes, &nonce, &aad, body, &mic).then_some(&*body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits.chunks(2).map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
    }

    /// RFC 3394 key wrap, as an access point would send the group key.
    fn key_wrap(kek: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let aes = Aes128::new(kek);
        let n = data.len() / 8;
        let mut a = [0xA6u8; 8];
        let mut r: Vec<[u8; 8]> = data.chunks(8).map(|chunk| chunk.try_into().unwrap()).collect();
        for j in 0..6 {
            for (i, word) in r.iter_mut().enumerate() {
                let mut block = [0u8; 16];
                block[..8].copy_from_slice(&a);
                block[8..].copy_from_slice(word);
                aes.encrypt(&mut block);
                let t = (n * j + i + 1) as u64;
                a = (u64::from_be_bytes(block[..8].try_into().unwrap()) ^ t).to_be_bytes();
                word.copy_from_slice(&block[8..]);
            }
        }
        let mut out = a.to_vec();
        out.extend(r.concat());
        out
    }

    /// An access point's EAPOL-Key message.
    fn ap_message(flags: u16, replay: u64, anonce: &[u8; 32], key_data: &[u8], kck: Option<&[u8; 16]>) -> Vec<u8> {
        let mut frame = alloc::vec![2, 3];
        frame.extend_from_slice(&((KEY_DATA - 4 + key_data.len()) as u16).to_be_bytes());
        frame.push(2);
        frame.extend_from_slice(&(flags | VERSION_AES | ACK).to_be_bytes());
        frame.extend_from_slice(&[0, 16]);
        frame.extend_from_slice(&replay.to_be_bytes());
        frame.extend_from_slice(anonce);
        frame.resize(RSC, 0);
        frame.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0]);
        frame.resize(KEY_DATA_LEN, 0);
        frame.extend_from_slice(&(key_data.len() as u16).to_be_bytes());
        frame.extend_from_slice(key_data);
        if let Some(kck) = kck {
            let mic = crypto::hmac_sha1(kck, &[&frame]);
            frame[MIC_FIELD..MIC_FIELD + 16].copy_from_slice(&mic[..16]);
        }
        frame
    }

    crate::ktest! {
        fn the_four_way_handshake_completes() {
            let pmk = crypto::passphrase_key(b"correct horse", b"home");
            let (station, ap) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 2]);
            let mut handshake = Handshake::new(pmk, station, ap, OUR_RSN.to_vec());
            let anonce = [7u8; 32];
            let Step::Reply(m2) = handshake.handle(&ap_message(PAIRWISE, 1, &anonce, &[], None)) else {
                panic!("no message 2")
            };
            // The access point derives the same keys from message 2's nonce
            // and checks its MIC.
            let snonce: [u8; 32] = m2[NONCE..NONCE + 32].try_into().unwrap();
            let ptk = derive_ptk(&pmk, &station, &ap, &snonce, &anonce);
            assert!(verify_mic(&ptk.kck, &m2));
            assert_eq!(&m2[KEY_DATA..], &OUR_RSN);

            let gtk = [9u8; 16];
            let mut key_data = OUR_RSN.to_vec();
            key_data.extend_from_slice(&[0xDD, 22, 0x00, 0x0F, 0xAC, 1, 1, 0]);
            key_data.extend_from_slice(&gtk);
            key_data.extend_from_slice(&[0xDD, 0]);
            key_data.resize(key_data.len().next_multiple_of(8), 0);
            let wrapped = key_wrap(&ptk.kek, &key_data);
            let flags = PAIRWISE | MIC | INSTALL | SECURE | ENCRYPTED_DATA;
            // A forged message 3 (wrong MIC) is ignored.
            let mut forged = ap_message(flags, 2, &anonce, &wrapped, Some(&ptk.kck));
            forged[MIC_FIELD] ^= 1;
            assert!(matches!(handshake.handle(&forged), Step::Ignore(_)));
            let m3 = ap_message(flags, 2, &anonce, &wrapped, Some(&ptk.kck));
            let Step::Done(m4, tk, group) = handshake.handle(&m3) else { panic!("no message 4") };
            assert!(verify_mic(&ptk.kck, &m4));
            assert_eq!(tk, ptk.tk);
            assert_eq!((group.index, group.key, group.rsc), (1, gtk, 5));
            // Replays are ignored.
            assert!(matches!(handshake.handle(&m3), Step::Ignore(_)));
        }

        fn ccmp_frames_match_the_standards_test_vector() {
            // IEEE 802.11-2012 M.6.4, as a whole frame.
            let aes = Aes128::new(&hex("c97c1f67ce371185514a8a19f2bdd52f").try_into().unwrap());
            let header = hex("0848 2c00 0fd2e128a57c 5030f1844408 abaea5b8fcba 8033");
            let body = hex("f8ba1a55d02f85ae967bb62fb6cda8eb7e78a050");
            let encrypted = ccmp_encrypt(&aes, &header, 0xB503_9776_E70C, 0, &body);
            assert_eq!(
                encrypted,
                hex("0ce7 0020 769703b5 f3d0a2fe9a3dbf2342a643e43246e80c3c04d019 7845ce0b16f97623")
            );
            let mut frame = header.clone();
            frame.extend_from_slice(&encrypted);
            assert_eq!(ccmp_header(&frame, 24), Some((0xB503_9776_E70C, 0)));
            assert_eq!(ccmp_decrypt(&aes, &mut frame, 24, 0xB503_9776_E70C), Some(&body[..]));
        }
    }
}
