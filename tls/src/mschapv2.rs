//! MS-CHAPv2 (RFC 2759), the password check inside PEAP: the client
//! proves it knows the password by encrypting a challenge with DES keys made
//! from the password's MD4 hash, and checks that the server knows it too.
//!
//! MD4 and DES are long broken, which is why PEAP wraps all of this in TLS.

use alloc::string::String;
use alloc::vec::Vec;

use apk::hash::sha1;

/// One MD4 round: its function, constant, word order and shifts.
type Round = (fn(u32, u32, u32) -> u32, u32, [usize; 16], [u32; 4]);

/// MD4 (RFC 1320): MS-CHAPv2 hashes the password with it.
pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_le_bytes());
    let mut state: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    for block in message.chunks(64) {
        let x: Vec<u32> = block.chunks(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
        let [mut a, mut b, mut c, mut d] = state;
        let rounds: [Round; 3] = [
            (|x, y, z| (x & y) | (!x & z), 0, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15], [3, 7, 11, 19]),
            (
                |x, y, z| (x & y) | (x & z) | (y & z),
                0x5A82_7999,
                [0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15],
                [3, 5, 9, 13],
            ),
            (|x, y, z| x ^ y ^ z, 0x6ED9_EBA1, [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15], [3, 9, 11, 15]),
        ];
        for (function, constant, order, shifts) in rounds {
            for (step, &index) in order.iter().enumerate() {
                let new = a
                    .wrapping_add(function(b, c, d))
                    .wrapping_add(x[index])
                    .wrapping_add(constant)
                    .rotate_left(shifts[step % 4]);
                // The registers rotate: the next step updates d, then c, then b.
                (a, b, c, d) = (d, new, b, c);
            }
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0; 16];
    for (chunk, word) in digest.chunks_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    digest
}

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32,
    24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47,
    39, 31, 23, 15, 7,
];
const EXPANSION: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18, 19, 20, 21, 20, 21,
    22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const PERMUTATION: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4,
    25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36, 63, 55,
    47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52, 31, 37, 47, 55, 30,
    40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const SBOXES: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8, 4,
        1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5, 0,
        14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13,
        6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10,
        6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6, 4,
        2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9,
        14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1,
        4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2, 7,
        11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Picks bits of an `width`-bit value by the standard's tables, which
/// number bits from 1 at the most significant end.
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &bit| out << 1 | (input >> (width - u32::from(bit)) & 1))
}

/// DES (FIPS 46-3) encryption of one block.
pub fn des_encrypt(key: [u8; 8], block: [u8; 8]) -> [u8; 8] {
    let key_bits = permute(u64::from_be_bytes(key), 64, &PC1);
    let (mut c, mut d) = (key_bits >> 28, key_bits & 0x0FFF_FFFF);
    let mut state = permute(u64::from_be_bytes(block), 64, &IP);
    let (mut left, mut right) = (state >> 32, state & 0xFFFF_FFFF);
    for shift in SHIFTS {
        c = (c << shift | c >> (28 - shift)) & 0x0FFF_FFFF;
        d = (d << shift | d >> (28 - shift)) & 0x0FFF_FFFF;
        let subkey = permute(c << 28 | d, 56, &PC2);
        let expanded = permute(right, 32, &EXPANSION) ^ subkey;
        let mut substituted = 0;
        for (index, sbox) in SBOXES.iter().enumerate() {
            let six = (expanded >> (42 - 6 * index)) & 0x3F;
            let row = (six >> 4 & 2) | (six & 1);
            let column = six >> 1 & 0xF;
            substituted = substituted << 4 | u64::from(sbox[(row * 16 + column) as usize]);
        }
        (left, right) = (right, left ^ permute(substituted, 32, &PERMUTATION));
    }
    state = right << 32 | left;
    // The final permutation is the initial one's inverse.
    let mut out = 0u64;
    for (position, &bit) in IP.iter().enumerate() {
        out |= (state >> (63 - position) & 1) << (64 - u32::from(bit));
    }
    out.to_be_bytes()
}

/// A DES key from 7 bytes: 7 bits per byte, the low (parity) bit unused.
fn des_key(bytes: &[u8]) -> [u8; 8] {
    let bits = bytes.iter().fold(0u64, |value, &byte| value << 8 | u64::from(byte));
    let mut key = [0; 8];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = ((bits >> (49 - 7 * index)) as u8) << 1;
    }
    key
}

/// The NT hash of a password: MD4 of its UTF-16LE encoding.
pub fn password_hash(password: &str) -> [u8; 16] {
    let unicode: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    md4(&unicode)
}

fn challenge_hash(peer_challenge: &[u8; 16], authenticator_challenge: &[u8; 16], user: &str) -> [u8; 8] {
    // The user name without any "DOMAIN\" in front.
    let user = user.rsplit('\\').next().unwrap_or(user);
    let digest = sha1(&[&peer_challenge[..], authenticator_challenge, user.as_bytes()].concat());
    digest[..8].try_into().unwrap()
}

/// The 24-byte NT-Response to the server's challenge.
pub fn nt_response(
    authenticator_challenge: &[u8; 16],
    peer_challenge: &[u8; 16],
    user: &str,
    password: &str,
) -> [u8; 24] {
    let challenge = challenge_hash(peer_challenge, authenticator_challenge, user);
    let mut padded = [0u8; 21];
    padded[..16].copy_from_slice(&password_hash(password));
    let mut response = [0; 24];
    for (index, chunk) in response.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&des_encrypt(des_key(&padded[index * 7..index * 7 + 7]), challenge));
    }
    response
}

/// What the server's success message must say ("S=" and 40 hex digits),
/// proving it knows the password too.
pub fn authenticator_response(
    password: &str,
    nt_response: &[u8; 24],
    peer_challenge: &[u8; 16],
    authenticator_challenge: &[u8; 16],
    user: &str,
) -> String {
    const MAGIC1: &[u8] = b"Magic server to client signing constant";
    const MAGIC2: &[u8] = b"Pad to make it do more than one iteration";
    let hash_hash = md4(&password_hash(password));
    let digest = sha1(&[&hash_hash[..], nt_response, MAGIC1].concat());
    let challenge = challenge_hash(peer_challenge, authenticator_challenge, user);
    let digest = sha1(&[&digest[..], &challenge, MAGIC2].concat());
    let mut text = String::from("S=");
    for byte in digest {
        text.push_str(&alloc::format!("{byte:02X}"));
    }
    text
}
