//! The cryptography WPA2-Personal needs, done in software: AES-128, CCMP
//! (AES in CCM mode, which encrypts and authenticates each frame), and the
//! SHA-1 based functions that turn a passphrase into keys (PBKDF2, the
//! 802.11 PRF, HMAC) plus AES key unwrap for the group key.
//!
//! The card could encrypt and decrypt frames itself, but doing it here
//! keeps the firmware's key handling (and its many API versions) out of
//! the driver; at Wi-Fi speeds the processor's cost is small.

use alloc::vec::Vec;

use apk::hash::sha1;

/// AES's substitution box (FIPS-197 figure 7).
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76, 0xca, 0x82, 0xc9,
    0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0, 0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f,
    0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15, 0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07,
    0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75, 0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3,
    0x29, 0xe3, 0x2f, 0x84, 0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58,
    0xcf, 0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8, 0x51, 0xa3,
    0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2, 0xcd, 0x0c, 0x13, 0xec, 0x5f,
    0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73, 0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88,
    0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb, 0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac,
    0x62, 0x91, 0x95, 0xe4, 0x79, 0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a,
    0xae, 0x08, 0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a, 0x70,
    0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e, 0xe1, 0xf8, 0x98, 0x11,
    0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf, 0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42,
    0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// The inverse substitution box, for decrypting.
const INVERSE_SBOX: [u8; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        table[SBOX[index] as usize] = index as u8;
        index += 1;
    }
    table
};

/// Multiplication by x in AES's field, GF(2^8).
fn times_two(value: u8) -> u8 {
    value << 1 ^ if value & 0x80 != 0 { 0x1B } else { 0 }
}

fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        a = times_two(a);
        b >>= 1;
    }
    product
}

/// AES with a 128-bit key (FIPS-197): 10 rounds over a 16-byte state,
/// stored column by column.
pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        let mut round_keys = [[0; 16]; 11];
        round_keys[0] = *key;
        let mut rcon = 1u8;
        for round in 1..11 {
            let previous = round_keys[round - 1];
            let mut word = [previous[13], previous[14], previous[15], previous[12]];
            for byte in &mut word {
                *byte = SBOX[*byte as usize];
            }
            word[0] ^= rcon;
            rcon = times_two(rcon);
            let mut next = [0; 16];
            for column in 0..4 {
                for row in 0..4 {
                    let before = if column == 0 { word[row] } else { next[(column - 1) * 4 + row] };
                    next[column * 4 + row] = previous[column * 4 + row] ^ before;
                }
            }
            round_keys[round] = next;
        }
        Self { round_keys }
    }

    pub fn encrypt(&self, block: &mut [u8; 16]) {
        xor_into(block, &self.round_keys[0]);
        for (round, round_key) in self.round_keys.iter().enumerate().skip(1) {
            for byte in block.iter_mut() {
                *byte = SBOX[*byte as usize];
            }
            shift_rows(block);
            if round != 10 {
                for column in block.chunks_mut(4) {
                    let [a, b, c, d] = [column[0], column[1], column[2], column[3]];
                    let all = a ^ b ^ c ^ d;
                    column[0] ^= all ^ times_two(a ^ b);
                    column[1] ^= all ^ times_two(b ^ c);
                    column[2] ^= all ^ times_two(c ^ d);
                    column[3] ^= all ^ times_two(d ^ a);
                }
            }
            xor_into(block, round_key);
        }
    }

    pub fn decrypt(&self, block: &mut [u8; 16]) {
        xor_into(block, &self.round_keys[10]);
        for round in (0..10).rev() {
            inverse_shift_rows(block);
            for byte in block.iter_mut() {
                *byte = INVERSE_SBOX[*byte as usize];
            }
            xor_into(block, &self.round_keys[round]);
            if round != 0 {
                for column in block.chunks_mut(4) {
                    let [a, b, c, d] = [column[0], column[1], column[2], column[3]];
                    column[0] = multiply(a, 14) ^ multiply(b, 11) ^ multiply(c, 13) ^ multiply(d, 9);
                    column[1] = multiply(a, 9) ^ multiply(b, 14) ^ multiply(c, 11) ^ multiply(d, 13);
                    column[2] = multiply(a, 13) ^ multiply(b, 9) ^ multiply(c, 14) ^ multiply(d, 11);
                    column[3] = multiply(a, 11) ^ multiply(b, 13) ^ multiply(c, 9) ^ multiply(d, 14);
                }
            }
        }
    }
}

/// Row `r` of the state moves `r` columns left.
fn shift_rows(block: &mut [u8; 16]) {
    let copy = *block;
    for column in 0..4 {
        for row in 0..4 {
            block[column * 4 + row] = copy[(column + row) % 4 * 4 + row];
        }
    }
}

fn inverse_shift_rows(block: &mut [u8; 16]) {
    let copy = *block;
    for column in 0..4 {
        for row in 0..4 {
            block[(column + row) % 4 * 4 + row] = copy[column * 4 + row];
        }
    }
}

fn xor_into(target: &mut [u8], source: &[u8]) {
    for (byte, other) in target.iter_mut().zip(source) {
        *byte ^= other;
    }
}

/// CCM (RFC 3610) as CCMP uses it: an 8-byte MIC and a 2-byte length, so a
/// 13-byte nonce. `aad` is authenticated but not encrypted. Encrypts `data`
/// in place and returns the MIC.
pub fn ccm_encrypt(aes: &Aes128, nonce: &[u8; 13], aad: &[u8], data: &mut [u8]) -> [u8; 8] {
    let mic = ccm_mac(aes, nonce, aad, data);
    ccm_ctr(aes, nonce, data, &mic)
}

/// Decrypts `data` in place and checks the MIC; false (with `data`
/// garbled) if the frame was forged or damaged.
pub fn ccm_decrypt(aes: &Aes128, nonce: &[u8; 13], aad: &[u8], data: &mut [u8], mic: &[u8; 8]) -> bool {
    let expected = ccm_ctr(aes, nonce, data, mic);
    let computed = ccm_mac(aes, nonce, aad, data);
    // Compared without an early exit, so timing doesn't reveal how much of
    // a forged MIC was right.
    computed.iter().zip(&expected).fold(0, |difference, (a, b)| difference | (a ^ b)) == 0
}

/// CBC-MAC over the first block (flags, nonce, length), the AAD (with its
/// length in front) and the data, each padded to whole blocks.
fn ccm_mac(aes: &Aes128, nonce: &[u8; 13], aad: &[u8], data: &[u8]) -> [u8; 8] {
    let mut x = [0u8; 16];
    // Flags: AAD present, (MIC length - 2) / 2 = 3, length field size - 1.
    x[0] = 0x40 | 3 << 3 | 1;
    x[1..14].copy_from_slice(nonce);
    x[14..16].copy_from_slice(&(data.len() as u16).to_be_bytes());
    aes.encrypt(&mut x);
    let mut header = Vec::with_capacity(aad.len() + 2);
    header.extend_from_slice(&(aad.len() as u16).to_be_bytes());
    header.extend_from_slice(aad);
    for part in [&header[..], data] {
        for chunk in part.chunks(16) {
            xor_into(&mut x, chunk);
            aes.encrypt(&mut x);
        }
    }
    x[..8].try_into().unwrap()
}

/// Counter mode: block 0 of the key stream encrypts the MIC, blocks 1 and
/// on the data. Returns the transformed MIC.
fn ccm_ctr(aes: &Aes128, nonce: &[u8; 13], data: &mut [u8], mic: &[u8; 8]) -> [u8; 8] {
    let counter_block = |counter: u16| {
        let mut block = [0u8; 16];
        block[0] = 1;
        block[1..14].copy_from_slice(nonce);
        block[14..16].copy_from_slice(&counter.to_be_bytes());
        aes.encrypt(&mut block);
        block
    };
    for (index, chunk) in data.chunks_mut(16).enumerate() {
        xor_into(chunk, &counter_block(index as u16 + 1));
    }
    let mut out = *mic;
    xor_into(&mut out, &counter_block(0));
    out
}

/// HMAC-SHA1 (RFC 2104) of the concatenation of `parts`.
pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..20].copy_from_slice(&sha1(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + parts.iter().map(|part| part.len()).sum::<usize>());
    inner.extend(block.iter().map(|byte| byte ^ 0x36));
    for part in parts {
        inner.extend_from_slice(part);
    }
    let mut outer = Vec::with_capacity(84);
    outer.extend(block.iter().map(|byte| byte ^ 0x5C));
    outer.extend_from_slice(&sha1(&inner));
    sha1(&outer)
}

/// The WPA2-Personal pairwise master key: PBKDF2-HMAC-SHA1 of the
/// passphrase, salted with the network's name, 4096 rounds, 256 bits.
pub fn passphrase_key(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    for (index, out) in key.chunks_mut(20).enumerate() {
        let block_number = (index as u32 + 1).to_be_bytes();
        let mut u = hmac_sha1(passphrase, &[ssid, &block_number]);
        let mut sum = u;
        for _ in 1..4096 {
            u = hmac_sha1(passphrase, &[&u]);
            xor_into(&mut sum, &u);
        }
        out.copy_from_slice(&sum[..out.len()]);
    }
    key
}

/// The 802.11 pseudo-random function (IEEE 802.11-2020 12.7.1.2):
/// HMAC-SHA1 of label, 0, data, counter, repeated for as many bytes as
/// needed.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 20);
    let mut counter = 0u8;
    while out.len() < len {
        out.extend_from_slice(&hmac_sha1(key, &[label, &[0], data, &[counter]]));
        counter += 1;
    }
    out.truncate(len);
    out
}

/// AES key unwrap (RFC 3394), which the access point uses to send the
/// group key. None if the integrity check fails.
pub fn key_unwrap(kek: &[u8; 16], wrapped: &[u8]) -> Option<Vec<u8>> {
    if wrapped.len() < 24 || !wrapped.len().is_multiple_of(8) {
        return None;
    }
    let aes = Aes128::new(kek);
    let n = wrapped.len() / 8 - 1;
    let mut a: [u8; 8] = wrapped[..8].try_into().unwrap();
    let mut r: Vec<[u8; 8]> = wrapped[8..].chunks(8).map(|chunk| chunk.try_into().unwrap()).collect();
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&(u64::from_be_bytes(a) ^ t).to_be_bytes());
            block[8..].copy_from_slice(&r[i]);
            aes.decrypt(&mut block);
            a.copy_from_slice(&block[..8]);
            r[i].copy_from_slice(&block[8..]);
        }
    }
    (a == [0xA6; 8]).then(|| r.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits.chunks(2).map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
    }

    crate::ktest! {
        fn aes_matches_fips_197() {
            let aes = Aes128::new(&hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap());
            let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
            aes.encrypt(&mut block);
            assert_eq!(block[..], hex("69c4e0d86a7b0430d8cdb78070b4c55a")[..]);
            aes.decrypt(&mut block);
            assert_eq!(block[..], hex("00112233445566778899aabbccddeeff")[..]);
        }

        fn ccmp_matches_the_standards_test_vector() {
            // IEEE 802.11-2012 M.6.4: the nonce and AAD as CCMP builds them
            // from the frame header and packet number.
            let aes = Aes128::new(&hex("c97c1f67ce371185514a8a19f2bdd52f").try_into().unwrap());
            let mut nonce = [0u8; 13];
            nonce[1..7].copy_from_slice(&hex("5030f1844408"));
            nonce[7..].copy_from_slice(&hex("b5039776e70c"));
            let aad = hex("0840 0fd2e128a57c 5030f1844408 abaea5b8fcba 0000");
            let mut data = hex("f8ba1a55d02f85ae967bb62fb6cda8eb7e78a050");
            let mic = ccm_encrypt(&aes, &nonce, &aad, &mut data);
            assert_eq!(data, hex("f3d0a2fe9a3dbf2342a643e43246e80c3c04d019"));
            assert_eq!(mic[..], hex("7845ce0b16f97623")[..]);
            assert!(ccm_decrypt(&aes, &nonce, &aad, &mut data, &mic));
            assert_eq!(data, hex("f8ba1a55d02f85ae967bb62fb6cda8eb7e78a050"));
            let mut forged = data.clone();
            ccm_encrypt(&aes, &nonce, &aad, &mut forged);
            forged[0] ^= 1;
            assert!(!ccm_decrypt(&aes, &nonce, &aad, &mut forged, &mic));
        }

        fn hmac_and_prf_match_their_test_vectors() {
            assert_eq!(
                hmac_sha1(b"Jefe", &[b"what do ya want ", b"for nothing?"])[..],
                hex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79")[..]
            );
            // IEEE 802.11i test vector for the PRF.
            assert_eq!(
                prf(&[0x0B; 20], b"prefix", b"Hi There", 64),
                hex("bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee
                     75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")
            );
        }

        fn passphrase_key_matches_ieee_802_11i() {
            assert_eq!(
                passphrase_key(b"password", b"IEEE")[..],
                hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e")[..]
            );
        }

        fn key_unwrap_matches_rfc_3394() {
            let kek = hex("000102030405060708090A0B0C0D0E0F").try_into().unwrap();
            let wrapped = hex("1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5");
            assert_eq!(key_unwrap(&kek, &wrapped), Some(hex("00112233445566778899AABBCCDDEEFF")));
            let mut damaged = wrapped.clone();
            damaged[3] ^= 1;
            assert_eq!(key_unwrap(&kek, &damaged), None);
        }
    }
}
