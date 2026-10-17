//! 802.11 frames: reading access points' beacons, and building the frames
//! a station sends to join a network (authentication, association) and to
//! carry Ethernet traffic.
//!
//! A frame starts with a header: frame control (type and flags), duration,
//! up to four addresses and a sequence number. Management frames carry
//! "elements" (id, length, data); data frames carry an LLC/SNAP header
//! naming the Ethernet type, then the packet.

use alloc::vec::Vec;

use super::commands::{DEFAULT_EDCA, Edca};

// Frame control, first byte: type and subtype.
pub const ASSOCIATION_REQUEST: u8 = 0x00;
pub const ASSOCIATION_RESPONSE: u8 = 0x10;
pub const PROBE_RESPONSE: u8 = 0x50;
pub const BEACON: u8 = 0x80;
pub const DISASSOCIATION: u8 = 0xA0;
pub const AUTHENTICATION: u8 = 0xB0;
pub const DEAUTHENTICATION: u8 = 0xC0;
pub const ACTION: u8 = 0xD0;
pub const DATA: u8 = 0x08;
pub const QOS_DATA: u8 = 0x88;
// Second byte: flags.
pub const TO_DS: u8 = 0x01;
pub const FROM_DS: u8 = 0x02;
pub const PROTECTED: u8 = 0x40;
pub const ORDER: u8 = 0x80;

// Element ids.
const SSID: u8 = 0;
const SUPPORTED_RATES: u8 = 1;
const DS_PARAMETERS: u8 = 3;
const TIM: u8 = 5;
const ERP: u8 = 42;
const HT_CAPABILITIES: u8 = 45;
const RSN: u8 = 48;
const EXTENDED_RATES: u8 = 50;
const HT_OPERATION: u8 = 61;
const VENDOR: u8 = 221;

pub const ETHERTYPE_EAPOL: u16 = 0x888E;
const LLC_SNAP: [u8; 6] = [0xAA, 0xAA, 0x03, 0, 0, 0];

/// The cipher suite and key management suites eliOS supports: CCMP (AES)
/// with a pre-shared key or 802.1X (enterprise sign-in), as `00-0F-AC:4`,
/// `00-0F-AC:2` and `00-0F-AC:1`.
const SUITE_CCMP: [u8; 4] = [0x00, 0x0F, 0xAC, 4];
const SUITE_TKIP: [u8; 4] = [0x00, 0x0F, 0xAC, 2];
const SUITE_8021X: [u8; 4] = [0x00, 0x0F, 0xAC, 1];
const SUITE_PSK: [u8; 4] = [0x00, 0x0F, 0xAC, 2];
const SUITE_SAE: [u8; 4] = [0x00, 0x0F, 0xAC, 8];

/// Our RSN element: version 1, CCMP for group and pairwise traffic, PSK,
/// no capabilities. It must be the same in the association request and
/// the handshake's second message.
pub const OUR_RSN: [u8; 22] =
    [RSN, 20, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 2, 0, 0];

/// Our RSN element for a network: [`OUR_RSN`], with 802.1X instead of PSK
/// on enterprise networks.
pub fn our_rsn(security: Security) -> [u8; 22] {
    let mut rsn = OUR_RSN;
    if security == Security::Wpa2Enterprise {
        rsn[19] = SUITE_8021X[3];
    }
    rsn
}

/// A network's security, as far as eliOS can join it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Open,
    Wpa2Personal,
    /// WPA2 with 802.1X: a user name and password checked by a RADIUS
    /// server (PEAP with MS-CHAPv2).
    Wpa2Enterprise,
    /// Something eliOS can't join yet, and why.
    Unsupported(&'static str),
}

impl Security {
    pub fn name(self) -> &'static str {
        match self {
            Security::Open => "open",
            Security::Wpa2Personal => "WPA2",
            Security::Wpa2Enterprise => "WPA2-EAP",
            Security::Unsupported(what) => what,
        }
    }
}

/// An access point, from its beacon or probe response.
#[derive(Clone, Debug)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    /// Signal strength in dBm.
    pub signal: i8,
    pub beacon_interval: u16,
    pub capability: u16,
    /// Rates in units of 500 kbit/s; bit 7 marks the required ("basic")
    /// ones.
    pub rates: Vec<u8>,
    pub security: Security,
    /// Two spatial streams (it supports HT MCS 8-15).
    pub ht: Option<Ht>,
    /// WMM (QoS) parameters, in the firmware's order.
    pub wmm: Option<[Edca; 4]>,
    pub use_protection: bool,
    pub dtim_period: u8,
    /// The RSN element, whole (the handshake checks the access point
    /// repeats it).
    pub rsn: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Ht {
    pub two_streams: bool,
    /// Protection for older stations is on (HT operation mode bits).
    pub protection: bool,
}

impl Bss {
    pub fn short_preamble(&self) -> bool {
        self.capability & 1 << 5 != 0
    }

    pub fn short_slot(&self) -> bool {
        self.capability & 1 << 10 != 0
    }

    pub fn ssid_text(&self) -> alloc::string::String {
        alloc::string::String::from_utf8_lossy(&self.ssid).into_owned()
    }
}

/// The elements in a management frame's body.
pub fn elements(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    core::iter::from_fn(move || {
        if data.len() < 2 || data.len() < 2 + usize::from(data[1]) {
            return None;
        }
        let (id, len) = (data[0], usize::from(data[1]));
        let value = &data[2..2 + len];
        data = &data[2 + len..];
        Some((id, value))
    })
}

/// Reads a beacon or probe response (the whole frame). `channel` and
/// `signal` come from the card, for when the frame doesn't say.
pub fn parse_bss(frame: &[u8], channel: u8, signal: i8) -> Option<Bss> {
    if frame.len() < 36 {
        return None;
    }
    let mut bss = Bss {
        bssid: frame[16..22].try_into().unwrap(),
        ssid: Vec::new(),
        channel,
        signal,
        beacon_interval: u16::from_le_bytes([frame[32], frame[33]]),
        capability: u16::from_le_bytes([frame[34], frame[35]]),
        rates: Vec::new(),
        security: Security::Open,
        ht: None,
        wmm: None,
        use_protection: false,
        dtim_period: 1,
        rsn: Vec::new(),
    };
    let privacy = bss.capability & 1 << 4 != 0;
    let mut rsn = None;
    let mut wpa1 = false;
    for (id, value) in elements(&frame[36..]) {
        match id {
            SSID => bss.ssid = value.to_vec(),
            SUPPORTED_RATES | EXTENDED_RATES => bss.rates.extend_from_slice(value),
            DS_PARAMETERS if !value.is_empty() => bss.channel = value[0],
            TIM if value.len() >= 2 => bss.dtim_period = value[1].max(1),
            ERP if !value.is_empty() => bss.use_protection = value[0] & 1 << 1 != 0,
            HT_CAPABILITIES if value.len() >= 7 => {
                let ht = bss.ht.get_or_insert(Ht { two_streams: false, protection: false });
                ht.two_streams = value[4] != 0;
            }
            HT_OPERATION if value.len() >= 4 => {
                let ht = bss.ht.get_or_insert(Ht { two_streams: false, protection: false });
                ht.protection = value[2] & 0x3 != 0;
            }
            RSN => {
                rsn = Some(value);
                bss.rsn = [&[RSN, value.len() as u8], value].concat();
            }
            VENDOR if value.starts_with(&[0x00, 0x50, 0xF2, 1]) => wpa1 = true,
            VENDOR if value.len() >= 24 && value.starts_with(&[0x00, 0x50, 0xF2, 2, 1]) => {
                bss.wmm = Some(parse_wmm(&value[8..24]));
            }
            VENDOR if value.starts_with(&[0x00, 0x50, 0xF2, 2, 0]) => bss.wmm = Some(DEFAULT_EDCA),
            _ => {}
        }
    }
    bss.security = match rsn {
        Some(rsn) => parse_rsn(rsn),
        None if wpa1 => Security::Unsupported("WPA1"),
        None if privacy => Security::Unsupported("WEP"),
        None => Security::Open,
    };
    Some(bss)
}

/// Decides from an RSN element whether eliOS can join: it needs CCMP for
/// both pairwise and group traffic, a pre-shared key or 802.1X, and
/// management frame protection not required.
fn parse_rsn(rsn: &[u8]) -> Security {
    let suite = |offset: usize| -> Option<[u8; 4]> { rsn.get(offset..offset + 4)?.try_into().ok() };
    let count = |offset: usize| rsn.get(offset..offset + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])));
    let Some(group) = suite(2) else { return Security::Unsupported("bad RSN") };
    let Some(pairwise_count) = count(6) else { return Security::Unsupported("bad RSN") };
    let pairwise: Vec<_> = (0..pairwise_count).filter_map(|index| suite(8 + index * 4)).collect();
    let akm_offset = 8 + pairwise_count * 4;
    let Some(akm_count) = count(akm_offset) else { return Security::Unsupported("bad RSN") };
    let akms: Vec<_> = (0..akm_count).filter_map(|index| suite(akm_offset + 2 + index * 4)).collect();
    let capabilities = count(akm_offset + 2 + akm_count * 4).unwrap_or(0);
    // A network offering both prefers its pre-shared key.
    let security = if akms.contains(&SUITE_PSK) {
        Security::Wpa2Personal
    } else if akms.contains(&SUITE_8021X) {
        Security::Wpa2Enterprise
    } else {
        return Security::Unsupported(if akms.contains(&SUITE_SAE) { "WPA3" } else { "unknown key management" });
    };
    if !pairwise.contains(&SUITE_CCMP) {
        return Security::Unsupported("TKIP");
    }
    if group == SUITE_TKIP {
        return Security::Unsupported("WPA2 with a TKIP group key");
    }
    if group != SUITE_CCMP {
        return Security::Unsupported("unknown cipher");
    }
    if capabilities & 1 << 6 != 0 {
        return Security::Unsupported("protected management frames required");
    }
    security
}

/// The four access categories' records from a WMM parameter element, in
/// the firmware's order (background, best effort, video, voice).
fn parse_wmm(records: &[u8]) -> [Edca; 4] {
    let mut edca = DEFAULT_EDCA;
    for record in records.chunks(4) {
        // WMM's ACI: 0 best effort, 1 background, 2 video, 3 voice.
        let index = match record[0] >> 5 & 0x3 {
            0 => 1,
            1 => 0,
            other => usize::from(other),
        };
        edca[index] = Edca {
            aifsn: record[0] & 0xF,
            cw_min: (1 << (record[1] & 0xF)) - 1,
            cw_max: (1 << (record[1] >> 4)) - 1,
            txop: u16::from_le_bytes([record[2], record[3]]),
        };
    }
    edca
}

/// Supported rates elements: 2.4 GHz has the old 802.11b rates (1, 2,
/// 5.5, 11 Mbit/s) and 802.11g's; 5 GHz only the latter.
pub fn supported_rates(two_ghz: bool) -> Vec<u8> {
    let ofdm = [12, 18, 24, 36, 48, 72, 96, 108];
    if two_ghz {
        let mut out = alloc::vec![SUPPORTED_RATES, 8, 2, 4, 11, 22, 12, 18, 24, 36];
        out.extend_from_slice(&[EXTENDED_RATES, 4, 48, 72, 96, 108]);
        out
    } else {
        let mut out = alloc::vec![SUPPORTED_RATES, 8];
        out.extend_from_slice(&ofdm);
        out
    }
}

/// HT capabilities: 20 MHz, short guard interval, receiving MCS 0-15 (two
/// streams, as the 7260 has two antennas) and one-stream STBC; no
/// aggregation is used, but the fields are required.
pub fn ht_capabilities() -> Vec<u8> {
    let mut out = alloc::vec![HT_CAPABILITIES, 26];
    // Capabilities: SM power save disabled (3 << 2), short GI at 20 MHz
    // (1 << 5), RX STBC one stream (1 << 8).
    out.extend_from_slice(&(3u16 << 2 | 1 << 5 | 1 << 8).to_le_bytes());
    // A-MPDU: 64 KiB, 8 us spacing.
    out.push(3 | 6 << 2);
    let mut mcs = [0u8; 16];
    mcs[0] = 0xFF;
    mcs[1] = 0xFF;
    // "TX MCS set defined", the same as for receiving.
    mcs[12] = 1;
    out.extend_from_slice(&mcs);
    out.extend_from_slice(&[0; 7]);
    out
}

/// The 24-byte header of a management frame to the access point.
fn management_header(subtype: u8, bssid: &[u8; 6], mac: &[u8; 6]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(128);
    frame.extend_from_slice(&[subtype, 0, 0, 0]);
    frame.extend_from_slice(bssid);
    frame.extend_from_slice(mac);
    frame.extend_from_slice(bssid);
    frame.extend_from_slice(&[0, 0]);
    frame
}

/// Open System authentication, request (sequence 1).
pub fn authentication(bssid: &[u8; 6], mac: &[u8; 6]) -> Vec<u8> {
    let mut frame = management_header(AUTHENTICATION, bssid, mac);
    frame.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    frame
}

pub fn deauthentication(bssid: &[u8; 6], mac: &[u8; 6], reason: u16) -> Vec<u8> {
    let mut frame = management_header(DEAUTHENTICATION, bssid, mac);
    frame.extend_from_slice(&reason.to_le_bytes());
    frame
}

/// An association request for `bss`, offering what both sides support.
pub fn association_request(bss: &Bss, mac: &[u8; 6], listen_interval: u16) -> Vec<u8> {
    let mut frame = management_header(ASSOCIATION_REQUEST, &bss.bssid, mac);
    let two_ghz = bss.channel <= 14;
    // ESS, and short preamble and slot time on 2.4 GHz.
    let capability: u16 = 1 | if two_ghz { 1 << 5 | 1 << 10 } else { 0 };
    frame.extend_from_slice(&capability.to_le_bytes());
    frame.extend_from_slice(&listen_interval.to_le_bytes());
    frame.extend_from_slice(&[SSID, bss.ssid.len() as u8]);
    frame.extend_from_slice(&bss.ssid);
    frame.extend_from_slice(&supported_rates(two_ghz));
    if matches!(bss.security, Security::Wpa2Personal | Security::Wpa2Enterprise) {
        frame.extend_from_slice(&our_rsn(bss.security));
    }
    if bss.ht.is_some() && bss.wmm.is_some() {
        frame.extend_from_slice(&ht_capabilities());
    }
    if bss.wmm.is_some() {
        // A WMM information element: version 1, no power save.
        frame.extend_from_slice(&[VENDOR, 7, 0x00, 0x50, 0xF2, 2, 0, 1, 0]);
    }
    frame
}

/// The status code in an authentication or association response, and the
/// association id from the latter.
pub fn response_status(frame: &[u8]) -> Option<(u16, u16)> {
    let read = |offset: usize| frame.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    match frame.first()? & 0xFC {
        AUTHENTICATION => Some((read(28)?, 0)),
        ASSOCIATION_RESPONSE => Some((read(26)?, read(28)? & 0x3FFF)),
        _ => None,
    }
}

/// The header of a data frame to the access point. QoS data frames carry
/// a traffic class (TID 0, "best effort") after the sequence number.
pub fn data_header(bssid: &[u8; 6], mac: &[u8; 6], destination: &[u8; 6], qos: bool, sequence: u16) -> Vec<u8> {
    let mut header = Vec::with_capacity(26);
    header.extend_from_slice(&[if qos { QOS_DATA } else { DATA }, TO_DS, 0, 0]);
    header.extend_from_slice(bssid);
    header.extend_from_slice(mac);
    header.extend_from_slice(destination);
    header.extend_from_slice(&(sequence << 4).to_le_bytes());
    if qos {
        header.extend_from_slice(&[0, 0]);
    }
    header
}

/// The LLC/SNAP header that names a data frame's Ethernet type.
pub fn llc_snap(ethertype: u16) -> [u8; 8] {
    let mut out = [0; 8];
    out[..6].copy_from_slice(&LLC_SNAP);
    out[6..].copy_from_slice(&ethertype.to_be_bytes());
    out
}

/// Where a data frame's parts are.
pub struct DataFrame {
    pub header_len: usize,
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub tid: u8,
    pub amsdu: bool,
    pub protected: bool,
}

/// Reads a data frame from the access point; None for frames that carry
/// nothing (null data) or aren't from an access point.
pub fn parse_data(frame: &[u8]) -> Option<DataFrame> {
    let (kind, flags) = (*frame.first()?, *frame.get(1)?);
    if kind & 0x0C != DATA || kind & 0x40 != 0 || flags & (TO_DS | FROM_DS) != FROM_DS {
        return None;
    }
    let qos = kind & 0x80 != 0;
    let mut header_len = 24;
    let (mut tid, mut amsdu) = (0, false);
    if qos {
        let control = *frame.get(24)?;
        tid = control & 0xF;
        amsdu = control & 0x80 != 0;
        header_len += 2;
        if flags & ORDER != 0 {
            header_len += 4;
        }
    }
    if frame.len() < header_len {
        return None;
    }
    Some(DataFrame {
        header_len,
        destination: frame[4..10].try_into().unwrap(),
        source: frame[16..22].try_into().unwrap(),
        tid,
        amsdu,
        protected: flags & PROTECTED != 0,
    })
}

/// Splits an A-MSDU (several packets in one frame) into (destination,
/// source, LLC payload) parts.
pub fn amsdu_parts(mut data: &[u8]) -> Vec<([u8; 6], [u8; 6], &[u8])> {
    let mut parts = Vec::new();
    while data.len() >= 14 {
        let len = usize::from(u16::from_be_bytes([data[12], data[13]]));
        let Some(body) = data.get(14..14 + len) else { break };
        parts.push((data[..6].try_into().unwrap(), data[6..12].try_into().unwrap(), body));
        let next = (14 + len).next_multiple_of(4);
        data = data.get(next..).unwrap_or(&[]);
    }
    parts
}

/// The Ethernet type and payload of an LLC/SNAP-encapsulated packet.
pub fn strip_llc(data: &[u8]) -> Option<(u16, &[u8])> {
    if data.len() < 8 || data[..6] != LLC_SNAP {
        return None;
    }
    Some((u16::from_be_bytes([data[6], data[7]]), &data[8..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn beacons_are_parsed() {
            let mut frame = alloc::vec![BEACON, 0, 0, 0];
            frame.extend_from_slice(&[0xFF; 6]);
            frame.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
            frame.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
            frame.extend_from_slice(&[0, 0]);
            frame.extend_from_slice(&[0; 8]);
            frame.extend_from_slice(&100u16.to_le_bytes());
            frame.extend_from_slice(&(1u16 | 1 << 4).to_le_bytes());
            frame.extend_from_slice(&[SSID, 4]);
            frame.extend_from_slice(b"home");
            frame.extend_from_slice(&[SUPPORTED_RATES, 4, 0x82, 0x84, 0x8B, 0x96]);
            frame.extend_from_slice(&[DS_PARAMETERS, 1, 6]);
            frame.extend_from_slice(&[TIM, 4, 0, 3, 0, 0]);
            frame.extend_from_slice(&OUR_RSN);
            let bss = parse_bss(&frame, 1, -40).unwrap();
            assert_eq!(bss.ssid, b"home");
            assert_eq!(bss.channel, 6);
            assert_eq!(bss.dtim_period, 3);
            assert_eq!(bss.security, Security::Wpa2Personal);
            assert_eq!(bss.rates, [0x82, 0x84, 0x8B, 0x96]);
            // WPA3-only and enterprise networks are recognized as such.
            let mut sae = OUR_RSN;
            sae[19] = 8;
            assert_eq!(parse_rsn(&sae[2..]), Security::Unsupported("WPA3"));
            let enterprise = our_rsn(Security::Wpa2Enterprise);
            assert_eq!(parse_rsn(&enterprise[2..]), Security::Wpa2Enterprise);
            assert_eq!(ht_capabilities().len(), 28);
        }

        fn data_frames_are_parsed() {
            let mut frame = alloc::vec![QOS_DATA, FROM_DS | PROTECTED, 0, 0];
            frame.extend_from_slice(&[1; 6]);
            frame.extend_from_slice(&[2; 6]);
            frame.extend_from_slice(&[3; 6]);
            frame.extend_from_slice(&[0, 0, 0x85, 0]);
            let data = parse_data(&frame).unwrap();
            assert_eq!(data.header_len, 26);
            assert_eq!((data.tid, data.amsdu, data.protected), (5, true, true));
            assert_eq!(data.source, [3; 6]);
            let mut amsdu = Vec::new();
            for length in [3u16, 5] {
                amsdu.extend_from_slice(&[1; 6]);
                amsdu.extend_from_slice(&[2; 6]);
                amsdu.extend_from_slice(&length.to_be_bytes());
                amsdu.resize(amsdu.len() + usize::from(length), 7);
                amsdu.resize(amsdu.len().next_multiple_of(4), 0);
            }
            let parts = amsdu_parts(&amsdu);
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[1].2, [7; 5]);
        }
    }
}
