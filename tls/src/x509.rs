//! Just enough X.509 (RFC 5280) to use a server's certificate: its names,
//! its public key, and the parts a signature check needs.

use alloc::string::String;
use alloc::vec::Vec;

/// One DER element: (tag, contents, what follows).
pub fn der(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
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
    let contents = data.get(header..header.checked_add(length)?)?;
    Some((tag, contents, &data[header + length..]))
}

const SEQUENCE: u8 = 0x30;
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Rsa,
    EllipticCurve,
    Other,
}

pub struct Certificate<'a> {
    /// The whole certificate, as sent.
    pub der: &'a [u8],
    pub subject: String,
    pub issuer: String,
    /// SubjectPublicKeyInfo, whole.
    pub public_key: &'a [u8],
    pub key_type: KeyType,
    /// DNS names from the subject alternative name extension.
    pub dns_names: Vec<String>,
}

/// A name's common name (or "?" if it has none).
fn common_name(name: &[u8]) -> String {
    let mut sets = name;
    while let Some((_, set, rest)) = der(sets) {
        sets = rest;
        let Some((_, pair, _)) = der(set) else { continue };
        let Some((_, oid, value)) = der(pair) else { continue };
        if oid == OID_COMMON_NAME
            && let Some((_, text, _)) = der(value)
        {
            return String::from_utf8_lossy(text).into_owned();
        }
    }
    String::from("?")
}

fn dns_names(mut extensions: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    while let Some((_, extension, rest)) = der(extensions) {
        extensions = rest;
        let Some((_, oid, mut fields)) = der(extension) else { continue };
        if oid != OID_SUBJECT_ALT_NAME {
            continue;
        }
        // An optional "critical" BOOLEAN, then the OCTET STRING.
        if fields.first() == Some(&0x01) {
            fields = der(fields).map_or(&[][..], |(_, _, rest)| rest);
        }
        let Some((_, octets, _)) = der(fields) else { continue };
        let Some((_, mut general_names, _)) = der(octets) else { continue };
        while let Some((tag, name, rest)) = der(general_names) {
            general_names = rest;
            // [2] dNSName
            if tag == 0x82 {
                names.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    names
}

impl<'a> Certificate<'a> {
    pub fn parse(certificate: &'a [u8]) -> Option<Self> {
        let (_, body, _) = der(certificate)?;
        let (_, tbs, _) = der(body)?;
        let mut fields = tbs;
        // [0] EXPLICIT version, if present.
        if fields.first() == Some(&0xA0) {
            fields = der(fields)?.2;
        }
        let (_, _serial, rest) = der(fields)?;
        let (_, _signature, rest) = der(rest)?;
        let (_, issuer, rest) = der(rest)?;
        let (_, _validity, rest) = der(rest)?;
        let (_, subject, rest) = der(rest)?;
        let (_, info, mut extensions) = der(rest)?;
        let public_key = &rest[..rest.len() - extensions.len()];
        let (_, algorithm, _) = der(info)?;
        let (_, oid, _) = der(algorithm)?;
        let key_type = match oid {
            OID_RSA => KeyType::Rsa,
            OID_EC => KeyType::EllipticCurve,
            _ => KeyType::Other,
        };
        let mut names = Vec::new();
        while let Some((tag, contents, next)) = der(extensions) {
            extensions = next;
            // [3] EXPLICIT extensions
            if tag == 0xA3
                && let Some((SEQUENCE, extensions, _)) = der(contents)
            {
                names = dns_names(extensions);
            }
        }
        Some(Self {
            der: certificate,
            subject: common_name(subject),
            issuer: common_name(issuer),
            public_key,
            key_type,
            dns_names: names,
        })
    }
}
