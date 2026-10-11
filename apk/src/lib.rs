//! Alpine Linux's package format, so eliOS can install Alpine's packages:
//! decompression, archives, hashes, signatures, indexes and dependencies,
//! all written for eliOS.
//!
//! # How a package is trusted
//!
//! A repository's index (`APKINDEX.tar.gz`) is two gzip streams one after
//! the other: a tar holding an RSA signature, made with one of Alpine's
//! keys, of the second stream's compressed bytes; and a tar holding the
//! index itself. The index lists, for each package, the SHA-1 of its
//! *control section*.
//!
//! A package (`name-version.apk`) is three gzip streams: a signature (not
//! needed when the index vouches for the package), the control section (a
//! tar with `.PKGINFO`, which includes the SHA-256 of the next stream as
//! `datahash`), and the data: a tar of the package's files.
//!
//! So one signature check on the index, then two hashes per package,
//! verify every byte, which is why plain HTTP is safe to fetch them over.

#![no_std]

extern crate alloc;

pub mod hash;
pub mod index;
pub mod inflate;
pub mod rsa;
pub mod tar;
#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::vec::Vec;

pub use self::index::{Dependency, Index, Package, compare_versions};
pub use self::rsa::PublicKey;

/// The most an index or package section may decompress to (Python's data,
/// the biggest common package, is about 50 MiB).
const LIMIT: usize = 512 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not a valid gzip, tar or index.
    Corrupt(&'static str),
    /// Signed with a key we don't have.
    UnknownKey(String),
    /// The signature or a checksum doesn't match: tampered with, or
    /// damaged on the way.
    BadSignature,
    BadChecksum,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Error::Corrupt(what) => write!(f, "damaged: {what}"),
            Error::UnknownKey(key) => write!(f, "signed with an unknown key ({key})"),
            Error::BadSignature => f.write_str("the signature doesn't match (tampered with or damaged)"),
            Error::BadChecksum => f.write_str("the checksum doesn't match (tampered with or damaged)"),
        }
    }
}

/// One gzip stream of a file: its compressed bytes, and what they
/// decompress to.
pub type Member<'a> = (&'a [u8], Vec<u8>);

/// The gzip streams a file is made of.
pub fn gzip_members(data: &[u8]) -> Result<Vec<Member<'_>>, Error> {
    let mut members = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let (output, used) = inflate::gunzip(&data[offset..], LIMIT).map_err(|_| Error::Corrupt("gzip"))?;
        members.push((&data[offset..offset + used], output));
        offset += used;
    }
    Ok(members)
}

/// A trusted key: its file name (as signatures name it) and the key.
pub struct Key {
    pub name: String,
    pub key: PublicKey,
}

/// Checks a signature section (the first gzip stream of an index or
/// package) against the bytes it signs.
fn verify_signature(signature_tar: &[u8], signed: &[u8], keys: &[Key]) -> Result<(), Error> {
    let entries = tar::entries(signature_tar).map_err(Error::Corrupt)?;
    let entry = entries.iter().find(|entry| entry.path.starts_with(".SIGN.")).ok_or(Error::Corrupt("no signature"))?;
    let (hash, key_name) = if let Some(name) = entry.path.strip_prefix(".SIGN.RSA256.") {
        (rsa::Hash::Sha256, name)
    } else if let Some(name) = entry.path.strip_prefix(".SIGN.RSA.") {
        (rsa::Hash::Sha1, name)
    } else {
        return Err(Error::Corrupt("unknown signature type"));
    };
    let key = keys.iter().find(|key| key.name == key_name).ok_or_else(|| Error::UnknownKey(key_name.into()))?;
    let digest: Vec<u8> = match hash {
        rsa::Hash::Sha1 => hash::sha1(signed).to_vec(),
        rsa::Hash::Sha256 => hash::sha256(signed).to_vec(),
    };
    if key.key.verify(entry.data, hash, &digest) { Ok(()) } else { Err(Error::BadSignature) }
}

/// Checks a repository index's signature and returns its text.
pub fn read_index(file: &[u8], keys: &[Key]) -> Result<String, Error> {
    let members = gzip_members(file)?;
    let [(_, signature), (signed, contents)] = members.as_slice() else {
        return Err(Error::Corrupt("an index has two parts"));
    };
    verify_signature(signature, signed, keys)?;
    let entries = tar::entries(contents).map_err(Error::Corrupt)?;
    let index = entries.iter().find(|entry| entry.path == "APKINDEX").ok_or(Error::Corrupt("no APKINDEX"))?;
    Ok(String::from_utf8_lossy(index.data).into_owned())
}

/// A verified package, unpacked into memory.
pub struct Contents {
    /// The control section's files (`.PKGINFO`, install scripts).
    pub control: Vec<u8>,
    /// The files, as a tar archive.
    pub data: Vec<u8>,
}

impl Contents {
    /// The package's install scripts (`.pre-install`, `.post-install`...),
    /// by name.
    pub fn scripts(&self) -> Vec<String> {
        tar::entries(&self.control)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.path.starts_with('.') && entry.path != ".PKGINFO")
            .map(|entry| entry.path)
            .collect()
    }
}

/// Checks a downloaded package against the index's checksum for it (and
/// the data against the control section's hash), and decompresses it.
pub fn read_package(file: &[u8], expected: &[u8; 20]) -> Result<Contents, Error> {
    let members = gzip_members(file)?;
    // The signature stream is optional (the index vouches for the
    // package), so the control and data sections are the last two.
    let [.., (control_raw, control), (data_raw, data)] = members.as_slice() else {
        return Err(Error::Corrupt("a package has at least two parts"));
    };
    if hash::sha1(control_raw) != *expected {
        return Err(Error::BadChecksum);
    }
    let entries = tar::entries(control).map_err(Error::Corrupt)?;
    let info = entries.iter().find(|entry| entry.path == ".PKGINFO").ok_or(Error::Corrupt("no .PKGINFO"))?;
    let info = String::from_utf8_lossy(info.data);
    let datahash =
        info.lines().find_map(|line| line.strip_prefix("datahash = ")).ok_or(Error::Corrupt("no datahash"))?;
    if hash::hex(&hash::sha256(data_raw)) != datahash.trim() {
        return Err(Error::BadChecksum);
    }
    Ok(Contents { control: control.clone(), data: data.clone() })
}
