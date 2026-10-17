//! HMAC-SHA-256 (RFC 2104) and TLS 1.2's pseudorandom function (RFC 5246
//! section 5), which stretches the key exchange's secret into keys.

use alloc::vec::Vec;

use apk::hash::sha256;

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = block.iter().map(|byte| byte ^ 0x36).collect();
    for part in parts {
        inner.extend_from_slice(part);
    }
    let mut outer: Vec<u8> = block.iter().map(|byte| byte ^ 0x5C).collect();
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

/// P_SHA256(secret, label + seed), `len` bytes.
pub fn prf(secret: &[u8], label: &[u8], seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut a = hmac_sha256(secret, &[label, seed]);
    while out.len() < len {
        out.extend_from_slice(&hmac_sha256(secret, &[&a, label, seed]));
        a = hmac_sha256(secret, &[&a]);
    }
    out.truncate(len);
    out
}
