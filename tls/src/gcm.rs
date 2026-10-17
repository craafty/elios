//! AES-128 in Galois/Counter Mode (NIST SP 800-38D) with a 12-byte nonce
//! and a 16-byte tag: counter-mode encryption plus a polynomial MAC over
//! the additional data and the ciphertext. TLS 1.2's AES-GCM suites use it.

use alloc::vec::Vec;

use crate::aes::Aes128;

pub struct Gcm {
    aes: Aes128,
    /// The hash key: the encryption of a zero block.
    h: u128,
}

/// Multiplication in GCM's field, GF(2^128), whose bits are numbered from
/// the most significant end (the spec's "rightshift" algorithm).
fn multiply(x: u128, y: u128) -> u128 {
    const R: u128 = 0xE1 << 120;
    let mut z = 0;
    let mut v = y;
    for bit in (0..128).rev() {
        if x >> bit & 1 != 0 {
            z ^= v;
        }
        v = if v & 1 != 0 { v >> 1 ^ R } else { v >> 1 };
    }
    z
}

impl Gcm {
    pub fn new(key: &[u8; 16]) -> Self {
        let aes = Aes128::new(key);
        let mut zero = [0; 16];
        aes.encrypt(&mut zero);
        Self { aes, h: u128::from_be_bytes(zero) }
    }

    fn ghash(&self, aad: &[u8], ciphertext: &[u8]) -> u128 {
        let mut y = 0;
        for data in [aad, ciphertext] {
            for chunk in data.chunks(16) {
                let mut block = [0; 16];
                block[..chunk.len()].copy_from_slice(chunk);
                y = multiply(y ^ u128::from_be_bytes(block), self.h);
            }
        }
        let lengths = ((aad.len() as u128 * 8) << 64) | (ciphertext.len() as u128 * 8);
        multiply(y ^ lengths, self.h)
    }

    /// Counter mode from block `counter` on: XORs the key stream into `data`.
    fn ctr(&self, nonce: &[u8; 12], data: &mut [u8]) {
        for (index, chunk) in data.chunks_mut(16).enumerate() {
            let mut block = [0; 16];
            block[..12].copy_from_slice(nonce);
            block[12..].copy_from_slice(&(index as u32 + 2).to_be_bytes());
            self.aes.encrypt(&mut block);
            crate::aes::xor_into(chunk, &block);
        }
    }

    fn tag(&self, nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let mut j0 = [0; 16];
        j0[..12].copy_from_slice(nonce);
        j0[15] = 1;
        self.aes.encrypt(&mut j0);
        (u128::from_be_bytes(j0) ^ self.ghash(aad, ciphertext)).to_be_bytes()
    }

    /// Encrypts `plaintext`; returns the ciphertext followed by the tag.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut out = plaintext.to_vec();
        self.ctr(nonce, &mut out);
        let tag = self.tag(nonce, aad, &out);
        out.extend_from_slice(&tag);
        out
    }

    /// Checks and decrypts ciphertext-plus-tag; `None` if it was altered.
    pub fn open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let (ciphertext, tag) = sealed.split_at_checked(sealed.len().checked_sub(16)?)?;
        let expected = self.tag(nonce, aad, ciphertext);
        // Constant time, so timing doesn't reveal how much of a forged tag
        // was right.
        if expected.iter().zip(tag).fold(0, |difference, (a, b)| difference | (a ^ b)) != 0 {
            return None;
        }
        let mut out = ciphertext.to_vec();
        self.ctr(nonce, &mut out);
        Some(out)
    }
}
