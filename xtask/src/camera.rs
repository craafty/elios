//! A pretend USB webcam for QEMU, so the UVC driver can be tested.
//!
//! QEMU has no camera, but its `usb-redir` device forwards a USB device
//! over a socket using the usbredir protocol (the one SPICE uses to pass
//! real devices to virtual machines). This is the other end of that
//! socket: a high-speed USB Video Class 1.0 camera with one YUY2 format in
//! three sizes, sending a moving test pattern over an isochronous
//! endpoint, as laptop webcams do.
//!
//! The picture is chosen so programs can check it: the luma of pixel
//! (x, y) in frame n is `(x + y + 2n) & 0xFF`, and the chroma makes eight
//! colored bars. `user/c/videotest.c` checks every frame it gets.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// usbredir packet types (usbredirproto.h).
const HELLO: u32 = 0;
const DEVICE_CONNECT: u32 = 1;
const RESET: u32 = 3;
const INTERFACE_INFO: u32 = 4;
const EP_INFO: u32 = 5;
const SET_CONFIGURATION: u32 = 6;
const GET_CONFIGURATION: u32 = 7;
const CONFIGURATION_STATUS: u32 = 8;
const SET_ALT_SETTING: u32 = 9;
const GET_ALT_SETTING: u32 = 10;
const ALT_SETTING_STATUS: u32 = 11;
const START_ISO_STREAM: u32 = 12;
const STOP_ISO_STREAM: u32 = 13;
const ISO_STREAM_STATUS: u32 = 14;
const CONTROL_PACKET: u32 = 100;
const ISO_PACKET: u32 = 102;

const STATUS_SUCCESS: u8 = 0;
const STATUS_STALL: u8 = 4;

/// Capabilities we offer: the device version in `device_connect`, packet
/// sizes in `ep_info`, 64-bit ids and 32-bit bulk lengths (QEMU's xHCI
/// insists on the last three). With 64-bit ids, headers are 16 bytes.
const CAPABILITIES: u32 = 1 << 1 | 1 << 4 | 1 << 5 | 1 << 6;

const ENDPOINT: u8 = 0x81;
/// The frame sizes offered, in the order of their frame indexes.
const SIZES: [(u16, u16); 3] = [(640, 480), (320, 240), (160, 120)];
/// Frame intervals offered, in 100 ns units: 30 and 15 frames a second.
const INTERVALS: [u32; 2] = [333_333, 666_666];
/// Bytes per isochronous packet in alternate settings 1 and 2 (one, then
/// three 1024-byte transactions per microframe).
const PACKET_SIZES: [usize; 2] = [1024, 3072];
const HEADER_LENGTH: usize = 12;

/// The video streaming parameters the host negotiates (UVC 1.0's 26-byte
/// probe and commit control).
#[derive(Clone, Copy)]
struct Parameters {
    format: u8,
    frame: u8,
    interval: u32,
}

impl Parameters {
    fn encode(self) -> [u8; 26] {
        let (width, height) = SIZES[usize::from(self.frame.clamp(1, 3) - 1)];
        let frame_size = u32::from(width) * u32::from(height) * 2;
        // The smallest packet that carries the frame rate.
        let payload = if width >= 640 { PACKET_SIZES[1] } else { PACKET_SIZES[0] } as u32;
        let mut bytes = [0u8; 26];
        bytes[2] = self.format;
        bytes[3] = self.frame;
        bytes[4..8].copy_from_slice(&self.interval.to_le_bytes());
        bytes[18..22].copy_from_slice(&frame_size.to_le_bytes());
        bytes[22..26].copy_from_slice(&payload.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Self {
        let frame = bytes.get(3).copied().unwrap_or(1).clamp(1, SIZES.len() as u8);
        let wanted = bytes.get(4..8).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        // The nearest interval offered.
        let interval = *INTERVALS.iter().min_by_key(|&&i| i.abs_diff(wanted)).unwrap();
        Self { format: 1, frame, interval }
    }
}

struct State {
    probe: Parameters,
    commit: Parameters,
    configuration: u8,
    alternate: u8,
    streaming: bool,
}

/// Packets go out whole from two threads (replies, and the stream).
type Writer = Arc<Mutex<UnixStream>>;

/// Listens on `path` for QEMU's `usb-redir` device and serves the camera
/// to it on a thread. Returns the QEMU arguments that attach it.
pub fn start(path: &Path) -> Result<Vec<String>, String> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("camera socket {}: {e}", path.display()))?;
    let owned = path.to_path_buf();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = std::fs::remove_file(&owned);
            if let Err(error) = serve(stream)
                && error.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("xtask: camera: {error}");
            }
        }
    });
    Ok(vec![
        "-chardev".into(),
        format!("socket,id=camera,path={}", path.display()),
        "-device".into(),
        "usb-redir,chardev=camera,bus=xhci.0".into(),
    ])
}

fn serve(stream: UnixStream) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    let writer: Writer = Arc::new(Mutex::new(stream));
    let mut hello = [0u8; 68];
    hello[..12].copy_from_slice(b"eliOS camera");
    hello[64..].copy_from_slice(&CAPABILITIES.to_le_bytes());
    // Hellos have 32-bit ids: neither side knows the other's capabilities
    // yet.
    let mut packet = Vec::new();
    packet.extend_from_slice(&HELLO.to_le_bytes());
    packet.extend_from_slice(&(hello.len() as u32).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&hello);
    writer.lock().unwrap().write_all(&packet)?;
    let mut header = [0u8; 12];
    reader.read_exact(&mut header)?;
    let mut theirs = vec![0u8; u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize];
    reader.read_exact(&mut theirs)?;
    let capabilities = theirs.get(64..68).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != HELLO || capabilities & CAPABILITIES != CAPABILITIES {
        return Err(io::Error::other("QEMU's usb-redir lacks the capabilities needed"));
    }
    send(&writer, INTERFACE_INFO, 0, &interface_info(), &[])?;
    send(&writer, EP_INFO, 0, &ep_info(0), &[])?;
    // High speed, class 0xEF/2/1 (interface association), the vendor and
    // product, version 1.00.
    let mut connect = vec![2, 0xEF, 2, 1];
    connect.extend_from_slice(&0x1209u16.to_le_bytes());
    connect.extend_from_slice(&0x0001u16.to_le_bytes());
    connect.extend_from_slice(&0x0100u16.to_le_bytes());
    send(&writer, DEVICE_CONNECT, 0, &connect, &[])?;

    let default = Parameters { format: 1, frame: 1, interval: INTERVALS[0] };
    let state = Arc::new(Mutex::new(State {
        probe: default,
        commit: default,
        configuration: 0,
        alternate: 0,
        streaming: false,
    }));
    loop {
        let mut header = [0u8; 16];
        reader.read_exact(&mut header)?;
        let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let id = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        match kind {
            RESET => {
                let mut state = state.lock().unwrap();
                state.streaming = false;
                state.alternate = 0;
                state.configuration = 0;
            }
            SET_CONFIGURATION => {
                let configuration = body.first().copied().unwrap_or(0);
                state.lock().unwrap().configuration = configuration;
                send(&writer, CONFIGURATION_STATUS, id, &[STATUS_SUCCESS, configuration], &[])?;
            }
            GET_CONFIGURATION => {
                let configuration = state.lock().unwrap().configuration;
                send(&writer, CONFIGURATION_STATUS, id, &[STATUS_SUCCESS, configuration], &[])?;
            }
            SET_ALT_SETTING => {
                let (interface, alternate) = (body[0], body[1]);
                let ok = (interface == 0 && alternate == 0) || (interface == 1 && alternate <= 2);
                if ok && interface == 1 {
                    let mut state = state.lock().unwrap();
                    state.alternate = alternate;
                    if alternate == 0 {
                        state.streaming = false;
                    }
                    drop(state);
                    send(&writer, EP_INFO, 0, &ep_info(alternate), &[])?;
                }
                let status = if ok { STATUS_SUCCESS } else { STATUS_STALL };
                send(&writer, ALT_SETTING_STATUS, id, &[status, interface, alternate], &[])?;
            }
            GET_ALT_SETTING => {
                let interface = body[0];
                let alternate = if interface == 1 { state.lock().unwrap().alternate } else { 0 };
                send(&writer, ALT_SETTING_STATUS, id, &[STATUS_SUCCESS, interface, alternate], &[])?;
            }
            START_ISO_STREAM => {
                let endpoint = body[0];
                let mut locked = state.lock().unwrap();
                let ok = endpoint == ENDPOINT && locked.alternate != 0;
                if ok && !locked.streaming {
                    locked.streaming = true;
                    let (state, writer) = (state.clone(), writer.clone());
                    thread::spawn(move || {
                        let _ = stream_video(&state, &writer);
                    });
                }
                drop(locked);
                let status = if ok { STATUS_SUCCESS } else { STATUS_STALL };
                send(&writer, ISO_STREAM_STATUS, id, &[status, endpoint], &[])?;
            }
            STOP_ISO_STREAM => {
                state.lock().unwrap().streaming = false;
                send(&writer, ISO_STREAM_STATUS, id, &[STATUS_SUCCESS, body[0]], &[])?;
            }
            CONTROL_PACKET if body.len() >= 10 => {
                let (reply, data) = control(&state, &body[..10], &body[10..]);
                send(&writer, CONTROL_PACKET, id, &reply, &data)?;
            }
            _ => {}
        }
    }
}

/// Answers a control request: the 10-byte header back with a status and
/// length, and any data for the host.
fn control(state: &Mutex<State>, header: &[u8], data: &[u8]) -> ([u8; 10], Vec<u8>) {
    let (request, request_type) = (header[1], header[2]);
    let value = u16::from_le_bytes([header[4], header[5]]);
    let index = u16::from_le_bytes([header[6], header[7]]);
    let length = usize::from(u16::from_le_bytes([header[8], header[9]]));
    let input = request_type & 0x80 != 0;
    let answer: Option<Vec<u8>> = match (request_type, request) {
        // GET_DESCRIPTOR
        (0x80, 6) => match (value >> 8) as u8 {
            1 => Some(device_descriptor()),
            2 => Some(configuration_descriptor()),
            3 => match value as u8 {
                0 => Some(vec![4, 3, 0x09, 0x04]),
                1 => Some(string_descriptor("eliOS")),
                2 => Some(string_descriptor("Test camera")),
                _ => None,
            },
            // A device qualifier: high speed only, so stall.
            _ => None,
        },
        // GET_STATUS
        (0x80..=0x82, 0) => Some(vec![0, 0]),
        // CLEAR_FEATURE / SET_FEATURE on anything.
        (0x00..=0x02, 1 | 3) => Some(Vec::new()),
        // Video streaming requests: probe (1) and commit (2) controls.
        (0x21 | 0xA1, _) if index & 0xFF == 1 => {
            let selector = (value >> 8) as u8;
            let mut state = state.lock().unwrap();
            match (request, selector) {
                (0x01, 1) => {
                    state.probe = Parameters::decode(data);
                    Some(Vec::new())
                }
                (0x01, 2) => {
                    state.commit = Parameters::decode(data);
                    Some(Vec::new())
                }
                (0x81, 1) => Some(state.probe.encode().to_vec()),
                (0x81, 2) => Some(state.commit.encode().to_vec()),
                // GET_MIN, GET_MAX and GET_DEF of the probe control.
                (0x82 | 0x87, 1) => Some(Parameters { format: 1, frame: 1, interval: INTERVALS[0] }.encode().to_vec()),
                (0x83, 1) => Some(Parameters { format: 1, frame: 3, interval: INTERVALS[1] }.encode().to_vec()),
                // GET_LEN, GET_INFO
                (0x85, 1 | 2) => Some(26u16.to_le_bytes().to_vec()),
                (0x86, 1 | 2) => Some(vec![3]),
                _ => None,
            }
        }
        _ => None,
    };
    let mut reply = [0u8; 10];
    reply.copy_from_slice(header);
    match answer {
        Some(mut bytes) => {
            if input {
                bytes.truncate(length);
            } else {
                bytes = Vec::new();
            }
            reply[3] = STATUS_SUCCESS;
            let sent = if input { bytes.len() } else { data.len() };
            reply[8..10].copy_from_slice(&(sent as u16).to_le_bytes());
            (reply, bytes)
        }
        None => {
            reply[3] = STATUS_STALL;
            reply[8..10].copy_from_slice(&[0, 0]);
            (reply, Vec::new())
        }
    }
}

/// Sends isochronous packets at a microframe's pace while streaming: each
/// frame split into payloads with a UVC header, and empty packets between
/// frames.
fn stream_video(state: &Mutex<State>, writer: &Writer) -> io::Result<()> {
    let (parameters, alternate) = {
        let state = state.lock().unwrap();
        (state.commit, state.alternate)
    };
    let (width, height) = SIZES[usize::from(parameters.frame - 1)];
    let packet = PACKET_SIZES[usize::from(alternate.clamp(1, 2) - 1)];
    let interval = Duration::from_nanos(u64::from(parameters.interval) * 100);
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut number: u32 = 0;
    let mut next_frame = start;
    let mut frame: Vec<u8> = Vec::new();
    let mut position = 0;
    let mut toggle = 0u8;
    while state.lock().unwrap().streaming {
        // A microframe is 125 µs; send what's due, then wait a little.
        let due = start.elapsed().as_micros() as u64 / 125;
        while sent < due {
            sent += 1;
            if position >= frame.len() && Instant::now() >= next_frame {
                frame = picture(width, height, number);
                number = number.wrapping_add(1);
                position = 0;
                next_frame += interval;
                toggle ^= 1;
            }
            if position >= frame.len() {
                send(writer, ISO_PACKET, sent, &[ENDPOINT, STATUS_SUCCESS, 0, 0], &[])?;
                continue;
            }
            let chunk = (frame.len() - position).min(packet - HEADER_LENGTH);
            let end = position + chunk == frame.len();
            // Header: length, then end of header | has SCR | has PTS | end
            // of frame | frame toggle; then PTS and SCR.
            let mut payload = vec![HEADER_LENGTH as u8, 0x80 | 0x08 | 0x04 | u8::from(end) << 1 | toggle];
            payload.extend_from_slice(&number.to_le_bytes());
            payload.extend_from_slice(&[0; 6]);
            payload.extend_from_slice(&frame[position..position + chunk]);
            position += chunk;
            let mut header = [ENDPOINT, STATUS_SUCCESS, 0, 0];
            header[2..4].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            send(writer, ISO_PACKET, sent, &header, &payload)?;
        }
        thread::sleep(Duration::from_micros(500));
    }
    Ok(())
}

/// Frame `number` of the test pattern, in YUY2 (Y0 U Y1 V for each pair
/// of pixels).
fn picture(width: u16, height: u16, number: u32) -> Vec<u8> {
    // Eight bars' (U, V): white, yellow, cyan, green, magenta, red, blue,
    // black-ish.
    const BARS: [(u8, u8); 8] =
        [(128, 128), (16, 146), (166, 16), (54, 34), (202, 222), (90, 240), (240, 110), (128, 128)];
    let mut bytes = Vec::with_capacity(usize::from(width) * usize::from(height) * 2);
    for y in 0..u32::from(height) {
        for x in (0..u32::from(width)).step_by(2) {
            let luma = |x: u32| ((x + y + 2 * number) & 0xFF) as u8;
            let (u, v) = BARS[(x * 8 / u32::from(width)) as usize];
            bytes.extend_from_slice(&[luma(x), u, luma(x + 1), v]);
        }
    }
    bytes
}

fn send(writer: &Writer, kind: u32, id: u64, header: &[u8], data: &[u8]) -> io::Result<()> {
    let mut packet = Vec::with_capacity(16 + header.len() + data.len());
    packet.extend_from_slice(&kind.to_le_bytes());
    packet.extend_from_slice(&((header.len() + data.len()) as u32).to_le_bytes());
    packet.extend_from_slice(&id.to_le_bytes());
    packet.extend_from_slice(header);
    packet.extend_from_slice(data);
    writer.lock().unwrap().write_all(&packet)
}

/// Interface 0 is video control, 1 video streaming (class 0x0E).
fn interface_info() -> Vec<u8> {
    let mut info = vec![0u8; 4 + 32 * 4];
    info[0] = 2;
    info[4] = 0;
    info[5] = 1;
    info[4 + 32] = 0x0E;
    info[4 + 33] = 0x0E;
    info[4 + 64] = 1;
    info[4 + 65] = 2;
    info
}

/// Endpoint types, intervals, interfaces and packet sizes, indexed like
/// usbredir does (OUT endpoints 0-15, IN 16-31); the isochronous endpoint
/// exists only in alternate settings 1 and 2.
fn ep_info(alternate: u8) -> Vec<u8> {
    let mut types = [255u8; 32];
    let mut intervals = [0u8; 32];
    let mut interfaces = [0u8; 32];
    let mut sizes = [0u16; 32];
    for index in [0, 16] {
        types[index] = 0;
        sizes[index] = 64;
    }
    if alternate != 0 {
        let index = 16 + usize::from(ENDPOINT & 0xF);
        types[index] = 1;
        intervals[index] = 1;
        interfaces[index] = 1;
        sizes[index] = iso_packet_size(alternate);
    }
    let mut info = Vec::new();
    info.extend_from_slice(&types);
    info.extend_from_slice(&intervals);
    info.extend_from_slice(&interfaces);
    sizes.iter().for_each(|size| info.extend_from_slice(&size.to_le_bytes()));
    info
}

/// wMaxPacketSize for an alternate setting: 1024 bytes, times one or three
/// transactions (bits 11-12).
fn iso_packet_size(alternate: u8) -> u16 {
    if alternate == 2 { 1024 | 2 << 11 } else { 1024 }
}

fn device_descriptor() -> Vec<u8> {
    let mut d = vec![18, 1, 0x00, 0x02, 0xEF, 2, 1, 64];
    d.extend_from_slice(&0x1209u16.to_le_bytes());
    d.extend_from_slice(&0x0001u16.to_le_bytes());
    d.extend_from_slice(&0x0100u16.to_le_bytes());
    d.extend_from_slice(&[1, 2, 0, 1]);
    d
}

fn string_descriptor(text: &str) -> Vec<u8> {
    let mut d = vec![0, 3];
    text.encode_utf16().for_each(|unit| d.extend_from_slice(&unit.to_le_bytes()));
    d[0] = d.len() as u8;
    d
}

fn configuration_descriptor() -> Vec<u8> {
    let mut d = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 250];
    // Interface association: interfaces 0 and 1 are one video function.
    d.extend_from_slice(&[8, 0x0B, 0, 2, 0x0E, 3, 0, 2]);
    // Video control interface, its header, camera terminal and output
    // terminal.
    d.extend_from_slice(&[9, 4, 0, 0, 0, 0x0E, 1, 0, 2]);
    let control_start = d.len();
    d.extend_from_slice(&[13, 0x24, 1, 0x00, 0x01, 0, 0]);
    d.extend_from_slice(&48_000_000u32.to_le_bytes());
    d.extend_from_slice(&[1, 1]);
    d.extend_from_slice(&[18, 0x24, 2, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]);
    d.extend_from_slice(&[9, 0x24, 3, 2, 0x01, 0x01, 0, 1, 0]);
    let control_length = (d.len() - control_start) as u16;
    d[control_start + 5..control_start + 7].copy_from_slice(&control_length.to_le_bytes());

    // Video streaming interface, alternate setting 0 (no bandwidth).
    d.extend_from_slice(&[9, 4, 1, 0, 0, 0x0E, 2, 0, 0]);
    let streaming_start = d.len();
    d.extend_from_slice(&[14, 0x24, 1, 1, 0, 0, ENDPOINT, 0, 2, 0, 0, 0, 1, 0]);
    // YUY2: its GUID, 16 bits a pixel, the first frame size by default.
    d.extend_from_slice(&[27, 0x24, 4, 1, SIZES.len() as u8]);
    d.extend_from_slice(&[b'Y', b'U', b'Y', b'2', 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
    d.extend_from_slice(&[16, 1, 0, 0, 0, 0]);
    for (index, &(width, height)) in SIZES.iter().enumerate() {
        let bytes = u32::from(width) * u32::from(height) * 2;
        d.extend_from_slice(&[26 + 4 * INTERVALS.len() as u8, 0x24, 5, index as u8 + 1, 0]);
        d.extend_from_slice(&width.to_le_bytes());
        d.extend_from_slice(&height.to_le_bytes());
        d.extend_from_slice(&(bytes * 8 * 15).to_le_bytes());
        d.extend_from_slice(&(bytes * 8 * 30).to_le_bytes());
        d.extend_from_slice(&bytes.to_le_bytes());
        d.extend_from_slice(&INTERVALS[0].to_le_bytes());
        d.push(INTERVALS.len() as u8);
        INTERVALS.iter().for_each(|interval| d.extend_from_slice(&interval.to_le_bytes()));
    }
    // Color matching: BT.709 primaries, BT.709 transfer, SMPTE 170M matrix.
    d.extend_from_slice(&[6, 0x24, 0x0D, 1, 1, 4]);
    let streaming_length = (d.len() - streaming_start) as u16;
    d[streaming_start + 4..streaming_start + 6].copy_from_slice(&streaming_length.to_le_bytes());
    // Alternate settings 1 and 2, each with the isochronous endpoint.
    for alternate in 1..=2u8 {
        d.extend_from_slice(&[9, 4, 1, alternate, 1, 0x0E, 2, 0, 0]);
        d.extend_from_slice(&[7, 5, ENDPOINT, 0x05]);
        d.extend_from_slice(&iso_packet_size(alternate).to_le_bytes());
        d.push(1);
    }
    let total = d.len() as u16;
    d[2..4].copy_from_slice(&total.to_le_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_have_consistent_lengths() {
        let configuration = configuration_descriptor();
        assert_eq!(usize::from(u16::from_le_bytes([configuration[2], configuration[3]])), configuration.len());
        let mut offset = 0;
        while offset < configuration.len() {
            let length = usize::from(configuration[offset]);
            assert!(length >= 2);
            offset += length;
        }
        assert_eq!(offset, configuration.len());
        assert_eq!(device_descriptor().len(), 18);
    }

    #[test]
    fn the_picture_is_a_whole_frame() {
        let frame = picture(160, 120, 3);
        assert_eq!(frame.len(), 160 * 120 * 2);
        assert_eq!(frame[0], 6);
        assert_eq!(frame[2], 7);
    }

    #[test]
    fn probing_picks_an_offered_interval() {
        let mut bytes = [0u8; 26];
        bytes[3] = 2;
        bytes[4..8].copy_from_slice(&600_000u32.to_le_bytes());
        let parameters = Parameters::decode(&bytes);
        assert_eq!((parameters.frame, parameters.interval), (2, 666_666));
    }
}
