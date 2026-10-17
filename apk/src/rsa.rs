//! RSA signature verification (PKCS #1 v1.5), and reading the public keys
//! it needs from PEM files.
//!
//! Verifying is raising the signature to the public exponent modulo the
//! key's modulus and checking the result is the expected padded hash. The
//! numbers are 4096 bits; multiplication modulo them uses Montgomery's
//! method, which replaces division by the modulus with shifts.

use alloc::vec;
use alloc::vec::Vec;

use crate::hash;

/// An RSA public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// The modulus, as 32-bit words, least significant first.
    modulus: Vec<u32>,
    exponent: Vec<u32>,
    /// The modulus's length in bytes.
    size: usize,
}

/// A big number from big-endian bytes (what keys and signatures use).
fn from_bytes(bytes: &[u8]) -> Vec<u32> {
    let mut words = vec![0u32; bytes.len().div_ceil(4).max(1)];
    for (index, &byte) in bytes.iter().rev().enumerate() {
        words[index / 4] |= u32::from(byte) << (8 * (index % 4));
    }
    words
}

fn to_bytes(words: &[u32], size: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; size];
    for (index, byte) in bytes.iter_mut().rev().enumerate() {
        if let Some(word) = words.get(index / 4) {
            *byte = (word >> (8 * (index % 4))) as u8;
        }
    }
    bytes
}

/// a >= b, for equal-length numbers.
fn at_least(a: &[u32], b: &[u32]) -> bool {
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        if x != y {
            return x > y;
        }
    }
    true
}

/// a -= b, for equal-length numbers; returns the borrow.
fn subtract(a: &mut [u32], b: &[u32]) -> bool {
    let mut borrow = 0i64;
    for (x, &y) in a.iter_mut().zip(b) {
        let value = i64::from(*x) - i64::from(y) - borrow;
        *x = value as u32;
        borrow = i64::from(value < 0);
    }
    borrow != 0
}

/// Arithmetic modulo an odd number, in Montgomery form: x is kept as
/// x·R mod n, where R = 2^(32·words).
struct Montgomery<'a> {
    n: &'a [u32],
    /// -n⁻¹ mod 2^32.
    inverse: u32,
    /// R² mod n, to convert into Montgomery form.
    r2: Vec<u32>,
}

impl<'a> Montgomery<'a> {
    fn new(n: &'a [u32]) -> Self {
        // Newton's iteration for n⁻¹ mod 2^32 (n is odd).
        let mut inverse: u32 = 1;
        for _ in 0..5 {
            inverse = inverse.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inverse)));
        }
        // R² mod n by doubling 1, 2·32·words times, reducing as it goes.
        let words = n.len();
        let mut r2 = vec![0u32; words];
        r2[0] = 1;
        for _ in 0..64 * words {
            let carry = r2[words - 1] >> 31;
            for index in (1..words).rev() {
                r2[index] = r2[index] << 1 | r2[index - 1] >> 31;
            }
            r2[0] <<= 1;
            if carry != 0 || at_least(&r2, n) {
                subtract(&mut r2, n);
            }
        }
        Self { n, inverse: inverse.wrapping_neg(), r2 }
    }

    /// a·b·R⁻¹ mod n (the coarsely integrated operand scanning method).
    fn multiply(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let words = self.n.len();
        let mut t = vec![0u32; words + 2];
        for &ai in a {
            let mut carry = 0u64;
            for j in 0..words {
                let sum = u64::from(t[j]) + u64::from(ai) * u64::from(b[j]) + carry;
                t[j] = sum as u32;
                carry = sum >> 32;
            }
            let sum = u64::from(t[words]) + carry;
            t[words] = sum as u32;
            t[words + 1] = (sum >> 32) as u32;

            let m = t[0].wrapping_mul(self.inverse);
            let mut carry = (u64::from(t[0]) + u64::from(m) * u64::from(self.n[0])) >> 32;
            for j in 1..words {
                let sum = u64::from(t[j]) + u64::from(m) * u64::from(self.n[j]) + carry;
                t[j - 1] = sum as u32;
                carry = sum >> 32;
            }
            let sum = u64::from(t[words]) + carry;
            t[words - 1] = sum as u32;
            t[words] = t[words + 1] + (sum >> 32) as u32;
        }
        let mut result = t[..words].to_vec();
        if t[words] != 0 || at_least(&result, self.n) {
            subtract(&mut result, self.n);
        }
        result
    }

    /// base^exponent mod n.
    fn power(&self, base: &[u32], exponent: &[u32]) -> Vec<u32> {
        let words = self.n.len();
        let mut one = vec![0u32; words];
        one[0] = 1;
        let base = self.multiply(base, &self.r2);
        let mut result = self.multiply(&one, &self.r2);
        let top = exponent.iter().rposition(|&word| word != 0).unwrap_or(0);
        for index in (0..=top).rev() {
            for bit in (0..32).rev() {
                result = self.multiply(&result, &result);
                if exponent[index] >> bit & 1 != 0 {
                    result = self.multiply(&result, &base);
                }
            }
        }
        self.multiply(&result, &one)
    }
}

/// Which hash a signature covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
}

impl PublicKey {
    /// Reads a key from a PEM file ("-----BEGIN PUBLIC KEY-----", which
    /// holds an X.509 SubjectPublicKeyInfo).
    pub fn from_pem(text: &str) -> Option<Self> {
        let body = text.split("-----BEGIN PUBLIC KEY-----").nth(1)?.split("-----END PUBLIC KEY-----").next()?;
        Self::from_der(&hash::base64_decode(body)?)
    }

    /// Reads a key from a DER SubjectPublicKeyInfo (as in certificates).
    pub fn from_der(der: &[u8]) -> Option<Self> {
        // SEQUENCE { SEQUENCE { algorithm }, BIT STRING { 0 unused bits,
        // SEQUENCE { INTEGER n, INTEGER e } } }
        let (_, info, _) = der_read(der, 0x30)?;
        let (_, _algorithm, rest) = der_read(info, 0x30)?;
        let (_, bits, _) = der_read(rest, 0x03)?;
        let (_, key, _) = der_read(bits.get(1..)?, 0x30)?;
        let (_, modulus, rest) = der_read(key, 0x02)?;
        let (_, exponent, _) = der_read(rest, 0x02)?;
        let modulus = &modulus[modulus.iter().position(|&b| b != 0)?..];
        if modulus.len() < 128 || modulus[modulus.len() - 1] & 1 == 0 {
            return None;
        }
        let mut words = from_bytes(modulus);
        words.resize(modulus.len().div_ceil(4), 0);
        Some(Self { modulus: words, exponent: from_bytes(exponent), size: modulus.len() })
    }

    /// The modulus's size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Encrypts `message` with PKCS #1 v1.5 padding (00 02, nonzero random
    /// bytes, 00, message), as TLS's RSA key exchange does. `random` fills
    /// a buffer with random bytes.
    pub fn encrypt(&self, message: &[u8], random: &mut dyn FnMut(&mut [u8])) -> Option<Vec<u8>> {
        if message.len() + 11 > self.size {
            return None;
        }
        let mut block = vec![0u8; self.size];
        block[1] = 2;
        let padding_end = self.size - message.len() - 1;
        for byte in &mut block[2..padding_end] {
            while *byte == 0 {
                let mut one = [0u8];
                random(&mut one);
                *byte = one[0];
            }
        }
        block[self.size - message.len()..].copy_from_slice(message);
        let mut value = from_bytes(&block);
        value.resize(self.modulus.len(), 0);
        Some(to_bytes(&Montgomery::new(&self.modulus).power(&value, &self.exponent), self.size))
    }

    /// Whether `signature` is this key's PKCS #1 v1.5 signature of a hash
    /// (`digest`) made with `hash`.
    pub fn verify(&self, signature: &[u8], hash: Hash, digest: &[u8]) -> bool {
        if signature.len() != self.size {
            return false;
        }
        let mut value = from_bytes(signature);
        value.resize(self.modulus.len(), 0);
        if at_least(&value, &self.modulus) {
            return false;
        }
        let decoded = to_bytes(&Montgomery::new(&self.modulus).power(&value, &self.exponent), self.size);
        decoded == expected_padding(self.size, hash, digest)
    }
}

/// The padded block a valid signature decodes to: 00 01 FF...FF 00, then
/// the DER encoding of the hash algorithm and the digest.
fn expected_padding(size: usize, hash: Hash, digest: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match hash {
        Hash::Sha1 => &[0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2B, 0x0E, 0x03, 0x02, 0x1A, 0x05, 0x00, 0x04, 0x14],
        Hash::Sha256 => &[
            0x30, 0x31, 0x30, 0x0D, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04,
            0x20,
        ],
    };
    let mut block = vec![0xFFu8; size];
    block[0] = 0;
    block[1] = 1;
    let tail = prefix.len() + digest.len();
    if tail + 11 > size {
        return Vec::new();
    }
    block[size - tail - 1] = 0;
    block[size - tail..size - digest.len()].copy_from_slice(prefix);
    block[size - digest.len()..].copy_from_slice(digest);
    block
}

/// Reads one DER element with tag `tag`: (header length, contents, what
/// follows).
fn der_read(data: &[u8], tag: u8) -> Option<(usize, &[u8], &[u8])> {
    if *data.first()? != tag {
        return None;
    }
    let first = *data.get(1)?;
    let (length, header) = if first & 0x80 == 0 {
        (usize::from(first), 2)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 {
            return None;
        }
        let length = data.get(2..2 + count)?.iter().fold(0usize, |value, &byte| value << 8 | usize::from(byte));
        (length, 2 + count)
    };
    let contents = data.get(header..header + length)?;
    Some((header, contents, &data[header + length..]))
}
