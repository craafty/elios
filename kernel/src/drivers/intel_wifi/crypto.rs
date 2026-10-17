//! The cryptography WPA2 needs, done in software: CCMP (AES, from the
//! `tls` crate, in CCM mode, which encrypts and authenticates each frame),
//! the SHA-1 based functions that turn a passphrase into keys (PBKDF2, the
//! 802.11 PRF, HMAC) plus AES key unwrap for the group key.
//!
//! The card could encrypt and decrypt frames itself, but doing it here
//! keeps the firmware's key handling (and its many API versions) out of
//! the driver; at Wi-Fi speeds the processor's cost is small.

use alloc::vec::Vec;

use apk::hash::sha1;
pub use tls::Aes128;

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
