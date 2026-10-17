//! TLS 1.2 for eliOS, written from scratch: the client handshake and
//! records (`client`), and the cryptography under them: AES-128, AES-GCM,
//! HMAC-SHA-256 and TLS's PRF, X25519, X.509 certificate parsing, and
//! MS-CHAPv2 (the password check PEAP runs inside TLS). RSA
//! and the hashes come from the `apk` crate.
//!
//! The kernel's Wi-Fi driver uses it for PEAP (WPA2-Enterprise sign-in runs
//! inside a TLS session); it's `no_std` so userland can use it too.

#![no_std]

extern crate alloc;

pub mod aes;
pub mod client;
pub mod gcm;
pub mod hmac;
pub mod mschapv2;
pub mod peap;
#[cfg(test)]
mod tests;
pub mod x25519;
pub mod x509;

pub use self::aes::Aes128;
pub use self::client::{Client, Error, Output};
