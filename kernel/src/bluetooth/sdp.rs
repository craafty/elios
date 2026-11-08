//! SDP, the service discovery protocol: how a device finds out what the
//! other end offers. Headsets look us up after connecting (to see that we
//! can send audio, and how); without an answer some give up on the
//! connection. We answer with one service record, an A2DP audio source.
//!
//! SDP speaks in "data elements": a header byte (type in the top five
//! bits, size in the low three), then the value. A record is a list of
//! attribute IDs and values; searches name UUIDs that a record must
//! contain, and which attributes to send back.

use alloc::vec::Vec;

/// SDP's PSM.
pub(super) const PSM: u16 = 0x0001;

/// PDUs.
const ERROR_RESPONSE: u8 = 0x01;
const SERVICE_SEARCH_REQUEST: u8 = 0x02;
const SERVICE_SEARCH_RESPONSE: u8 = 0x03;
const SERVICE_ATTRIBUTE_REQUEST: u8 = 0x04;
const SERVICE_ATTRIBUTE_RESPONSE: u8 = 0x05;
const SERVICE_SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
const SERVICE_SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;

/// Error codes.
const INVALID_RECORD_HANDLE: u16 = 0x0002;
const INVALID_SYNTAX: u16 = 0x0003;
const INVALID_CONTINUATION: u16 = 0x0005;

/// UUIDs (16-bit forms of the Bluetooth base UUID).
const AUDIO_SOURCE: u16 = 0x110A;
const ADVANCED_AUDIO_DISTRIBUTION: u16 = 0x110D;
const L2CAP: u16 = 0x0100;
const AVDTP: u16 = 0x0019;
const PUBLIC_BROWSE_ROOT: u16 = 0x1002;

const A2DP_SOURCE_HANDLE: u32 = 0x0001_0001;

/// The 16-bit forms of UUIDs that are short forms of the base UUID
/// 0000xxxx-0000-1000-8000-00805F9B34FB.
const BASE_UUID_TAIL: [u8; 12] = [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB];

/// A data element value.
enum Element {
    U16(u16),
    U32(u32),
    Uuid(u16),
    List(Vec<Element>),
}

impl Element {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Element::U16(value) => {
                out.push(0x09);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Element::U32(value) => {
                out.push(0x0A);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Element::Uuid(value) => {
                out.push(0x19);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Element::List(items) => {
                let mut body = Vec::new();
                for item in items {
                    item.encode(&mut body);
                }
                sequence(&body, out);
            }
        }
    }

    fn uuids(&self, found: &mut Vec<u16>) {
        match self {
            Element::Uuid(uuid) => found.push(*uuid),
            Element::List(items) => items.iter().for_each(|item| item.uuids(found)),
            _ => {}
        }
    }
}

/// Wraps `body` as a sequence.
fn sequence(body: &[u8], out: &mut Vec<u8>) {
    if body.len() < 256 {
        out.extend_from_slice(&[0x35, body.len() as u8]);
    } else {
        out.push(0x36);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(body);
}

/// Our one record, attribute by attribute, in ID order.
fn a2dp_source() -> Vec<(u16, Element)> {
    use Element::*;
    alloc::vec![
        (0x0000, U32(A2DP_SOURCE_HANDLE)),
        (0x0001, List(alloc::vec![Uuid(AUDIO_SOURCE)])),
        (
            0x0004,
            List(alloc::vec![List(alloc::vec![Uuid(L2CAP), U16(AVDTP)]), List(alloc::vec![Uuid(AVDTP), U16(0x0103)])]),
        ),
        (0x0005, List(alloc::vec![Uuid(PUBLIC_BROWSE_ROOT)])),
        (0x0009, List(alloc::vec![List(alloc::vec![Uuid(ADVANCED_AUDIO_DISTRIBUTION), U16(0x0103)])])),
        // Supported features: a player.
        (0x0311, U16(0x0001)),
    ]
}

/// Answers one request PDU.
pub(super) fn respond(request: &[u8]) -> Vec<u8> {
    let Some(&[pdu, t0, t1, _, _]) = request.get(..5).map(|h| <&[u8; 5]>::try_from(h).unwrap()) else {
        return pdu_with(ERROR_RESPONSE, [0, 0], &INVALID_SYNTAX.to_be_bytes());
    };
    let transaction = [t0, t1];
    match answer(pdu, &request[5..]) {
        Ok((code, body)) => pdu_with(code, transaction, &body),
        Err(error) => pdu_with(ERROR_RESPONSE, transaction, &error.to_be_bytes()),
    }
}

fn pdu_with(code: u8, transaction: [u8; 2], body: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec![code, transaction[0], transaction[1]];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn answer(pdu: u8, mut parameters: &[u8]) -> Result<(u8, Vec<u8>), u16> {
    let record = a2dp_source();
    let mut uuids = Vec::new();
    record.iter().for_each(|(_, value)| value.uuids(&mut uuids));
    match pdu {
        SERVICE_SEARCH_REQUEST => {
            let pattern = read_uuids(&mut parameters)?;
            let matches = pattern.iter().all(|uuid| uuid.is_some_and(|uuid| uuids.contains(&uuid)));
            let mut body = Vec::new();
            let count = u16::from(matches);
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            if matches {
                body.extend_from_slice(&A2DP_SOURCE_HANDLE.to_be_bytes());
            }
            body.push(0);
            Ok((SERVICE_SEARCH_RESPONSE, body))
        }
        SERVICE_ATTRIBUTE_REQUEST => {
            let handle = take(&mut parameters, 4)?;
            if handle != A2DP_SOURCE_HANDLE.to_be_bytes() {
                return Err(INVALID_RECORD_HANDLE);
            }
            let maximum = u16::from_be_bytes(take(&mut parameters, 2)?.try_into().unwrap());
            let ranges = read_ranges(&mut parameters)?;
            let list = attributes(&record, &ranges);
            Ok((SERVICE_ATTRIBUTE_RESPONSE, continued(&list, maximum, parameters)?))
        }
        SERVICE_SEARCH_ATTRIBUTE_REQUEST => {
            let pattern = read_uuids(&mut parameters)?;
            let maximum = u16::from_be_bytes(take(&mut parameters, 2)?.try_into().unwrap());
            let ranges = read_ranges(&mut parameters)?;
            let mut lists = Vec::new();
            if pattern.iter().all(|uuid| uuid.is_some_and(|uuid| uuids.contains(&uuid))) {
                lists = attributes(&record, &ranges);
            }
            let mut all = Vec::new();
            sequence(&lists, &mut all);
            Ok((SERVICE_SEARCH_ATTRIBUTE_RESPONSE, continued(&all, maximum, parameters)?))
        }
        _ => Err(INVALID_SYNTAX),
    }
}

/// A record's attributes in `ranges`, as a sequence of ID, value pairs.
fn attributes(record: &[(u16, Element)], ranges: &[(u16, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in record {
        if ranges.iter().any(|&(low, high)| (low..=high).contains(id)) {
            Element::U16(*id).encode(&mut body);
            value.encode(&mut body);
        }
    }
    let mut list = Vec::new();
    sequence(&body, &mut list);
    list
}

/// At most `maximum` bytes of `whole`, from where the continuation state
/// says (a 2-byte offset of ours), with the state for the rest.
fn continued(whole: &[u8], maximum: u16, mut parameters: &[u8]) -> Result<Vec<u8>, u16> {
    let length = usize::from(*take(&mut parameters, 1)?.first().unwrap());
    let offset = match length {
        0 => 0,
        2 => usize::from(u16::from_be_bytes(take(&mut parameters, 2)?.try_into().unwrap())),
        _ => return Err(INVALID_CONTINUATION),
    };
    if offset > whole.len() {
        return Err(INVALID_CONTINUATION);
    }
    let end = whole.len().min(offset + usize::from(maximum.max(7)));
    let mut body = Vec::new();
    body.extend_from_slice(&((end - offset) as u16).to_be_bytes());
    body.extend_from_slice(&whole[offset..end]);
    if end < whole.len() {
        body.push(2);
        body.extend_from_slice(&(end as u16).to_be_bytes());
    } else {
        body.push(0);
    }
    Ok(body)
}

fn take<'a>(data: &mut &'a [u8], count: usize) -> Result<&'a [u8], u16> {
    let taken = data.get(..count).ok_or(INVALID_SYNTAX)?;
    *data = &data[count..];
    Ok(taken)
}

/// A sequence element's body.
fn read_sequence<'a>(data: &mut &'a [u8]) -> Result<&'a [u8], u16> {
    let header = take(data, 1)?[0];
    if header >> 3 != 6 {
        return Err(INVALID_SYNTAX);
    }
    let length = match header & 7 {
        5 => usize::from(take(data, 1)?[0]),
        6 => usize::from(u16::from_be_bytes(take(data, 2)?.try_into().unwrap())),
        7 => u32::from_be_bytes(take(data, 4)?.try_into().unwrap()) as usize,
        _ => return Err(INVALID_SYNTAX),
    };
    take(data, length)
}

/// A search pattern: UUIDs, as 16-bit forms (`None` for UUIDs that have
/// none, which match nothing of ours).
fn read_uuids(data: &mut &[u8]) -> Result<Vec<Option<u16>>, u16> {
    let mut body = read_sequence(data)?;
    let mut uuids = Vec::new();
    while !body.is_empty() {
        let header = take(&mut body, 1)?[0];
        let uuid = match header {
            0x19 => Some(u16::from_be_bytes(take(&mut body, 2)?.try_into().unwrap())),
            0x1A => {
                let value = take(&mut body, 4)?;
                (value[..2] == [0, 0]).then(|| u16::from_be_bytes([value[2], value[3]]))
            }
            0x1C => {
                let value = take(&mut body, 16)?;
                (value[..2] == [0, 0] && value[4..] == BASE_UUID_TAIL).then(|| u16::from_be_bytes([value[2], value[3]]))
            }
            _ => return Err(INVALID_SYNTAX),
        };
        uuids.push(uuid);
    }
    Ok(uuids)
}

/// An attribute ID list: single IDs and ranges.
fn read_ranges(data: &mut &[u8]) -> Result<Vec<(u16, u16)>, u16> {
    let mut body = read_sequence(data)?;
    let mut ranges = Vec::new();
    while !body.is_empty() {
        match take(&mut body, 1)?[0] {
            0x09 => {
                let id = u16::from_be_bytes(take(&mut body, 2)?.try_into().unwrap());
                ranges.push((id, id));
            }
            0x0A => {
                let range = take(&mut body, 4)?;
                ranges.push((u16::from_be_bytes([range[0], range[1]]), u16::from_be_bytes([range[2], range[3]])));
            }
            _ => return Err(INVALID_SYNTAX),
        }
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn sdp_finds_the_audio_source() {
            // Search for Audio Source, all attributes, room for 0x200 bytes.
            let request = [
                SERVICE_SEARCH_ATTRIBUTE_REQUEST, 0, 7, 0, 15,
                0x35, 3, 0x19, 0x11, 0x0A,
                0x02, 0x00,
                0x35, 5, 0x0A, 0x00, 0x00, 0xFF, 0xFF,
                0,
            ];
            let response = respond(&request);
            assert_eq!(&response[..3], &[SERVICE_SEARCH_ATTRIBUTE_RESPONSE, 0, 7]);
            let count = usize::from(u16::from_be_bytes([response[5], response[6]]));
            let lists = &response[7..7 + count];
            assert_eq!(*response.last().unwrap(), 0, "no continuation");
            // One list holding the record, which starts with its handle.
            assert_eq!(lists[0], 0x35);
            assert_eq!(&lists[4..12], &[0x09, 0x00, 0x00, 0x0A, 0x00, 0x01, 0x00, 0x01]);

            // A small maximum splits the answer; continuing gets the rest.
            let mut small = request;
            small[11] = 0x00;
            small[10] = 0x00;
            small[11] = 0x10;
            let first = respond(&small);
            let first_count = usize::from(u16::from_be_bytes([first[5], first[6]]));
            assert_eq!(first_count, 16);
            let state = &first[7 + first_count..];
            assert_eq!(state[0], 2);
            let mut next = small[..small.len() - 1].to_vec();
            next.extend_from_slice(state);
            next[4] = (next.len() - 5) as u8;
            let second = respond(&next);
            let second_count = usize::from(u16::from_be_bytes([second[5], second[6]]));
            let mut joined = first[7..7 + first_count].to_vec();
            joined.extend_from_slice(&second[7..7 + second_count]);
            assert!(lists.starts_with(&joined));

            // Hands-free (0x111E) isn't offered.
            let mut other = request;
            other[8..10].copy_from_slice(&[0x11, 0x1E]);
            let response = respond(&other);
            assert_eq!(&response[5..9], &[0, 2, 0x35, 0]);
        }
    }
}
