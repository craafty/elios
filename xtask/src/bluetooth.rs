//! A pretend Intel Bluetooth controller for QEMU, so the Bluetooth stack
//! can be tested.
//!
//! It's attached the way the camera is (`camera.rs`: QEMU's `usb-redir`
//! over a Unix socket) and looks like the ThinkPad T440p's Intel 7260
//! controller (USB 8087:07dc, full speed, HCI on interface 0): the same
//! ROM version, so the kernel asks for the same patch file. It reads that
//! file too and checks every patch command the kernel sends against it,
//! answering with the events the file lists; only if all of them matched
//! and the kernel then asks for the patch to be switched on does it report
//! a patch loaded. A scan finds three devices: a keyboard and a mouse
//! that send their names with their inquiry answers, and a headset that
//! has to be asked for its name.
//!
//! The devices can be paired with and connected to as real ones are:
//! the keyboard pairs by passkey (it "types" the number the host shows),
//! the mouse and the headset "just work". Link keys are derived from the
//! address, so a device paired before accepts the host's stored key. The
//! keyboard and mouse then speak L2CAP and HID: once the host asks for
//! the boot protocol, the keyboard types `echo bluetooth keyboard works`
//! and Enter, and the mouse moves. When the host disconnects one, it
//! connects back by itself once, a second later, as a paired keyboard
//! does when a key is pressed. The headset is an A2DP sink: it answers
//! AVDTP's questions (one SBC sink endpoint), checks every audio packet's
//! SBC frames, and writes them to target/headset.sbc, which ffmpeg can
//! decode (`ffmpeg -f sbc -i target/headset.sbc out.wav`).

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::camera::{Writer, hello, send, string_descriptor};
use crate::handsfree::{self, HandsFree};

// usbredir packet types (usbredirproto.h).
const DEVICE_CONNECT: u32 = 1;
const INTERFACE_INFO: u32 = 4;
const EP_INFO: u32 = 5;
const SET_CONFIGURATION: u32 = 6;
const GET_CONFIGURATION: u32 = 7;
const CONFIGURATION_STATUS: u32 = 8;
const SET_ALT_SETTING: u32 = 9;
const GET_ALT_SETTING: u32 = 10;
const ALT_SETTING_STATUS: u32 = 11;
const START_INTERRUPT_RECEIVING: u32 = 15;
const STOP_INTERRUPT_RECEIVING: u32 = 16;
const INTERRUPT_RECEIVING_STATUS: u32 = 17;
const CANCEL_DATA_PACKET: u32 = 21;
const CONTROL_PACKET: u32 = 100;
const BULK_PACKET: u32 = 101;
const INTERRUPT_PACKET: u32 = 103;

const STATUS_SUCCESS: u8 = 0;
const STATUS_CANCELLED: u8 = 1;
const STATUS_STALL: u8 = 4;

const EVENTS: u8 = 0x81;
const ACL_IN: u8 = 0x82;
const ACL_OUT: u8 = 0x02;
const PACKET_SIZE: u16 = 64;

/// What Intel's "read version" says: hardware 37.7.10, firmware
/// 1.80.2.3.d (the T440p's).
const VERSION: [u8; 8] = [0x37, 0x07, 0x10, 0x01, 0x80, 0x02, 0x03, 0x0D];
const PATCH_FILE: &str = "rootfs/lib/firmware/intel/ibt-hw-37.7.10-fw-1.80.2.3.d.bseq";
/// The patch number reported once the patch is on.
const PATCHED: u8 = 0x42;
const ADDRESS: [u8; 6] = [0xCC, 0xBB, 0xAA, 0x22, 0x11, 0x00];

/// A device a scan finds.
struct Device {
    address: [u8; 6],
    class: u32,
    rssi: i8,
    name: &'static str,
    /// Input/output capabilities: 2 a keyboard only, 3 nothing.
    capability: u8,
    hid: Hid,
}

#[derive(Clone, Copy, PartialEq)]
enum Hid {
    None,
    Keyboard,
    Mouse,
}

const KEYBOARD: usize = 0;
const HEADSET: usize = 1;
const MOUSE: usize = 2;
const DEVICES: [Device; 3] = [
    Device {
        address: [0x01, 0x55, 0x44, 0x33, 0x22, 0x11],
        class: 0x002540,
        rssi: -50,
        name: "Test keyboard",
        capability: 2,
        hid: Hid::Keyboard,
    },
    Device {
        address: [0x02, 0x55, 0x44, 0x33, 0x22, 0x11],
        class: 0x240404,
        rssi: -65,
        name: "Test headset",
        capability: 3,
        hid: Hid::None,
    },
    Device {
        address: [0x03, 0x55, 0x44, 0x33, 0x22, 0x11],
        class: 0x002580,
        rssi: -55,
        name: "Test mouse",
        capability: 3,
        hid: Hid::Mouse,
    },
];

/// The passkey the keyboard "types".
const PASSKEY: u32 = 123456;
/// What the keyboard types once connected.
const TYPED: &str = "echo bluetooth keyboard works\n";

/// HID's L2CAP channels.
const PSM_CONTROL: u16 = 0x11;
const PSM_INTERRUPT: u16 = 0x13;
/// AVDTP: signaling, then (the second channel to the same PSM) audio,
/// which goes by a made-up PSM here.
const PSM_AVDTP: u16 = 0x19;
const PSM_MEDIA: u16 = 0x1019;
const PSM_SDP: u16 = 0x01;
const PSM_RFCOMM: u16 = 0x03;

/// A connection to one of the devices.
struct Link {
    handle: u16,
    device: usize,
    /// Whether the device connected to the host (it then opens the HID
    /// channels).
    incoming: bool,
    channels: Vec<Channel>,
    next_cid: u16,
    next_ident: u8,
    typing: bool,
    /// Audio packets received, and how many had broken SBC frames.
    audio_packets: u64,
    bad_audio: u64,
    /// The headset's hands-free side, once RFCOMM is in use.
    hands_free: Option<HandsFree>,
    /// The host's gateway channel, from its SDP server.
    gateway_channel: u8,
}

/// An L2CAP channel on the device's side: our CID and the host's.
struct Channel {
    psm: u16,
    ours: u16,
    theirs: u16,
    /// Whether the device opened it (not the host).
    outgoing: bool,
    /// Bit 0: we agreed to the host's configuration; bit 1: it to ours.
    configured: u8,
}

/// A patch file step: the command's bytes (opcode, length, parameters)
/// and the events that answer it.
struct Step {
    command: Vec<u8>,
    events: Vec<Vec<u8>>,
}

struct State {
    configuration: u8,
    /// Whether QEMU polls the event endpoint yet; events wait until it
    /// does.
    receiving: bool,
    waiting: VecDeque<Vec<u8>>,
    steps: Vec<Step>,
    /// The next patch step expected.
    step: usize,
    /// Whether a command didn't match the patch file.
    mismatch: bool,
    patch: u8,
    /// Bulk IN requests from QEMU, answered when there's ACL data, and
    /// ACL data waiting for one.
    acl_requests: VecDeque<u64>,
    acl_waiting: VecDeque<Vec<u8>>,
    links: Vec<Link>,
    next_handle: u16,
    /// Devices that have connected back by themselves already (they do it
    /// once).
    reconnected: Vec<usize>,
    /// Where the headset writes the SBC frames it receives.
    audio_file: PathBuf,
}

type Shared = Arc<Mutex<State>>;

/// Listens on `path` for QEMU's `usb-redir` device and serves the
/// controller on a thread. Returns the QEMU arguments that attach it.
pub fn start(path: &Path) -> Result<Vec<String>, String> {
    let file = crate::workspace_root().join(PATCH_FILE);
    let patch = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let steps = parse(&patch).ok_or_else(|| format!("{}: not a patch file", file.display()))?;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("Bluetooth socket {}: {e}", path.display()))?;
    let owned = path.to_path_buf();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = std::fs::remove_file(&owned);
            if let Err(error) = serve(stream, steps, owned.with_file_name("headset.sbc"))
                && error.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("xtask: bluetooth: {error}");
            }
        }
    });
    Ok(vec![
        "-chardev".into(),
        format!("socket,id=bluetooth,path={}", path.display()),
        "-device".into(),
        "usb-redir,chardev=bluetooth,bus=xhci.0".into(),
    ])
}

/// Reads a .bseq file: 0x01 and a command, then 0x02 and an event for
/// each answer.
fn parse(mut file: &[u8]) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    while let [0x01, _, _, length, ..] = file {
        let end = 4 + usize::from(*length);
        let command = file.get(1..end)?.to_vec();
        file = &file[end..];
        let mut events = Vec::new();
        while let [0x02, _, length, ..] = file {
            let end = 3 + usize::from(*length);
            events.push(file.get(1..end)?.to_vec());
            file = &file[end..];
        }
        steps.push(Step { command, events });
    }
    file.is_empty().then_some(steps)
}

fn serve(stream: UnixStream, steps: Vec<Step>, audio_file: PathBuf) -> io::Result<()> {
    let _ = std::fs::remove_file(&audio_file);
    let mut reader = stream.try_clone()?;
    let writer: Writer = Arc::new(Mutex::new(stream));
    hello(&writer, &mut reader, "eliOS Bluetooth")?;
    send(&writer, INTERFACE_INFO, 0, &interface_info(), &[])?;
    send(&writer, EP_INFO, 0, &ep_info(), &[])?;
    // Full speed, class 0xE0/1/1 (Bluetooth), Intel's vendor and the
    // 7260's product, version 0.01.
    let mut connect = vec![1, 0xE0, 1, 1];
    connect.extend_from_slice(&0x8087u16.to_le_bytes());
    connect.extend_from_slice(&0x07DCu16.to_le_bytes());
    connect.extend_from_slice(&0x0001u16.to_le_bytes());
    send(&writer, DEVICE_CONNECT, 0, &connect, &[])?;

    let state = Arc::new(Mutex::new(State {
        configuration: 0,
        receiving: false,
        waiting: VecDeque::new(),
        steps,
        step: 0,
        mismatch: false,
        patch: 0,
        acl_requests: VecDeque::new(),
        acl_waiting: VecDeque::new(),
        links: Vec::new(),
        next_handle: 0x0B,
        reconnected: Vec::new(),
        audio_file,
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
                let status = if body[1] == 0 { STATUS_SUCCESS } else { STATUS_STALL };
                send(&writer, ALT_SETTING_STATUS, id, &[status, body[0], body[1]], &[])?;
            }
            GET_ALT_SETTING => send(&writer, ALT_SETTING_STATUS, id, &[STATUS_SUCCESS, body[0], 0], &[])?,
            START_INTERRUPT_RECEIVING => {
                send(&writer, INTERRUPT_RECEIVING_STATUS, id, &[STATUS_SUCCESS, body[0]], &[])?;
                let mut locked = state.lock().unwrap();
                locked.receiving = true;
                while let Some(event) = locked.waiting.pop_front() {
                    send_event(&writer, &event)?;
                }
            }
            STOP_INTERRUPT_RECEIVING => {
                state.lock().unwrap().receiving = false;
                send(&writer, INTERRUPT_RECEIVING_STATUS, id, &[STATUS_SUCCESS, body[0]], &[])?;
            }
            BULK_PACKET if body.len() >= 10 => {
                let endpoint = body[0];
                let mut locked = state.lock().unwrap();
                if endpoint & 0x80 != 0 {
                    match locked.acl_waiting.pop_front() {
                        Some(packet) => send_bulk_in(&writer, id, &packet)?,
                        None => locked.acl_requests.push_back(id),
                    }
                } else {
                    // ACL data for a device, which the controller reports
                    // sent at once.
                    let mut reply = body[..10].to_vec();
                    reply[1] = STATUS_SUCCESS;
                    send(&writer, BULK_PACKET, id, &reply, &[])?;
                    let data = &body[10..];
                    if data.len() >= 4 {
                        let [low, high] = [data[0], data[1] & 0x0F];
                        queue_event(&mut locked, &writer, vec![0x13, 5, 1, low, high, 1, 0])?;
                        acl_from_host(&mut locked, &state, &writer, data)?;
                    }
                }
            }
            CANCEL_DATA_PACKET => {
                let mut locked = state.lock().unwrap();
                if let Some(index) = locked.acl_requests.iter().position(|&request| request == id) {
                    locked.acl_requests.remove(index);
                    send(&writer, BULK_PACKET, id, &[ACL_IN, STATUS_CANCELLED, 0, 0, 0, 0, 0, 0, 0, 0], &[])?;
                }
            }
            CONTROL_PACKET if body.len() >= 10 => {
                let (reply, data) = control(&state, &writer, &body[..10], &body[10..])?;
                send(&writer, CONTROL_PACKET, id, &reply, &data)?;
            }
            _ => {}
        }
    }
}

/// Answers a control request: the 10-byte header back with a status and
/// length, and any data for the host. HCI commands arrive as class
/// requests (type 0x20).
fn control(state: &Arc<Mutex<State>>, writer: &Writer, header: &[u8], data: &[u8]) -> io::Result<([u8; 10], Vec<u8>)> {
    let (request, request_type) = (header[1], header[2]);
    let value = u16::from_le_bytes([header[4], header[5]]);
    let length = usize::from(u16::from_le_bytes([header[8], header[9]]));
    let input = request_type & 0x80 != 0;
    let answer: Option<Vec<u8>> = match (request_type, request) {
        (0x80, 6) => match (value >> 8) as u8 {
            1 => Some(device_descriptor()),
            2 => Some(configuration_descriptor()),
            3 => match value as u8 {
                0 => Some(vec![4, 3, 0x09, 0x04]),
                1 => Some(string_descriptor("eliOS")),
                2 => Some(string_descriptor("Test Bluetooth")),
                _ => None,
            },
            _ => None,
        },
        (0x80..=0x82, 0) => Some(vec![0, 0]),
        (0x00..=0x02, 1 | 3) => Some(Vec::new()),
        (0x20, 0) if data.len() >= 3 => {
            command(state, writer, data)?;
            Some(Vec::new())
        }
        _ => None,
    };
    let mut reply = [0u8; 10];
    reply.copy_from_slice(header);
    Ok(match answer {
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
    })
}

/// Runs an HCI command: opcode, length, parameters.
fn command(state: &Arc<Mutex<State>>, writer: &Writer, packet: &[u8]) -> io::Result<()> {
    let opcode = u16::from_le_bytes([packet[0], packet[1]]);
    let parameters = &packet[3..];
    let mut locked = state.lock().unwrap();
    // The next patch step, if this is it.
    let step = locked.step;
    if locked.steps.get(step).is_some_and(|s| s.command == packet) {
        locked.step += 1;
        let events = locked.steps[step].events.clone();
        for event in events {
            queue_event(&mut locked, writer, event)?;
        }
        return Ok(());
    }
    let complete = |status: u8, values: &[u8]| {
        let mut event = vec![0x0E, 0, 1, packet[0], packet[1], status];
        event.extend_from_slice(values);
        event[1] = (event.len() - 2) as u8;
        event
    };
    let status_event = |status: u8| vec![0x0F, 4, status, 1, packet[0], packet[1]];
    let event = match opcode {
        // Intel's read version.
        0xFC05 => {
            let mut values = VERSION.to_vec();
            values.push(locked.patch);
            complete(0, &values)
        }
        // Manufacturer mode; leaving it with 2 switches the patch on, if
        // every step of it arrived as the file has it.
        0xFC11 if parameters.len() == 2 => {
            if parameters == [0, 2] && locked.step == locked.steps.len() && !locked.mismatch {
                locked.patch = PATCHED;
            }
            complete(0, &[])
        }
        // Patch data that isn't the next step.
        0xFC8E | 0xFC2F | 0xFC60 => {
            eprintln!("xtask: bluetooth: patch command {opcode:#06x} out of order (step {})", locked.step);
            locked.mismatch = true;
            complete(0x12, &[])
        }
        // Read local version: HCI 4.0 (6), revision, LMP 4.0, Intel (2),
        // LMP subversion.
        0x1001 => complete(0, &[6, 0x00, 0x01, 6, 0x02, 0x00, 0x02, 0x0D]),
        // Read buffer size: 1021-byte ACL packets, 64-byte SCO, 8 and 16
        // of them.
        0x1005 => complete(0, &[0xFD, 0x03, 64, 8, 0, 16, 0]),
        0x1009 => complete(0, &ADDRESS),
        // Inquiry: accepted; results come later.
        0x0401 => {
            queue_event(&mut locked, writer, status_event(0))?;
            let (state, writer) = (state.clone(), writer.clone());
            thread::spawn(move || {
                let _ = inquiry(&state, &writer);
            });
            return Ok(());
        }
        // Remote name request.
        0x0419 if parameters.len() >= 6 => {
            queue_event(&mut locked, writer, status_event(0))?;
            let mut event = vec![0x07, 255, 0];
            event.extend_from_slice(&parameters[..6]);
            let mut name = [0u8; 248];
            match device_at(parameters) {
                Some(device) => name[..DEVICES[device].name.len()].copy_from_slice(DEVICES[device].name.as_bytes()),
                // Page timeout: nobody answered.
                None => event[2] = 0x04,
            }
            event.extend_from_slice(&name);
            event
        }
        0x0405..=0x0434 => {
            return link_command(&mut locked, state, writer, opcode, parameters, &complete, &status_event);
        }
        _ => complete(0, &[]),
    };
    queue_event(&mut locked, writer, event)
}

/// The device at the start of `parameters` (an address), if it's one of
/// ours.
fn device_at(parameters: &[u8]) -> Option<usize> {
    DEVICES.iter().position(|device| parameters.get(..6) == Some(&device.address[..]))
}

/// The link key pairing with a device makes: made up, but always the same
/// for the same device.
fn link_key(device: usize) -> [u8; 16] {
    let address = DEVICES[device].address;
    std::array::from_fn(|index| address[index % 6].wrapping_mul(7).wrapping_add(index as u8 * 13))
}

fn connection_complete(status: u8, handle: u16, address: &[u8]) -> Vec<u8> {
    let mut event = vec![0x03, 11, status];
    event.extend_from_slice(&handle.to_le_bytes());
    event.extend_from_slice(&address[..6]);
    // ACL, not encrypted.
    event.extend_from_slice(&[1, 0]);
    event
}

fn with_address(code: u8, address: &[u8], rest: &[u8]) -> Vec<u8> {
    let mut event = vec![code, (6 + rest.len()) as u8];
    event.extend_from_slice(&address[..6]);
    event.extend_from_slice(rest);
    event
}

fn authentication_complete(status: u8, handle: u16) -> Vec<u8> {
    let [low, high] = handle.to_le_bytes();
    vec![0x06, 3, status, low, high]
}

/// Simple Pairing Complete, the link key, and Authentication Complete:
/// the end of pairing.
fn paired(state: &mut State, writer: &Writer, device: usize, key_type: u8) -> io::Result<()> {
    let address = DEVICES[device].address;
    let mut complete = vec![0x36, 7, 0];
    complete.extend_from_slice(&address);
    queue_event(state, writer, complete)?;
    let mut key = link_key(device).to_vec();
    key.push(key_type);
    queue_event(state, writer, with_address(0x18, &address, &key))?;
    let handle = state.links.iter().find(|link| link.device == device).map_or(0, |link| link.handle);
    queue_event(state, writer, authentication_complete(0, handle))
}

/// Pairing failed: Simple Pairing Complete and Authentication Complete
/// with `status`.
fn not_paired(state: &mut State, writer: &Writer, device: usize, status: u8) -> io::Result<()> {
    let address = DEVICES[device].address;
    let mut event = vec![0x36, 7, status];
    event.extend_from_slice(&address);
    queue_event(state, writer, event)?;
    let handle = state.links.iter().find(|link| link.device == device).map_or(0, |link| link.handle);
    queue_event(state, writer, authentication_complete(status, handle))
}

/// Runs `then` on the state `ms` milliseconds from now (on a thread).
fn later(
    state: &Shared,
    writer: &Writer,
    ms: u64,
    then: impl FnOnce(&mut State, &Shared, &Writer) -> io::Result<()> + Send + 'static,
) {
    let (state, writer) = (state.clone(), writer.clone());
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        let mut locked = state.lock().unwrap();
        let _ = then(&mut locked, &state, &writer);
    });
}

/// Commands about connections and pairing.
fn link_command(
    locked: &mut State,
    state: &Shared,
    writer: &Writer,
    opcode: u16,
    parameters: &[u8],
    complete: &dyn Fn(u8, &[u8]) -> Vec<u8>,
    status_event: &dyn Fn(u8) -> Vec<u8>,
) -> io::Result<()> {
    let handle =
        u16::from_le_bytes([parameters.first().copied().unwrap_or(0), parameters.get(1).copied().unwrap_or(0)]);
    let by_handle = locked.links.iter().position(|link| link.handle == handle);
    let device = device_at(parameters);
    match opcode {
        // Create Connection, Accept Connection Request.
        0x0405 | 0x0409 if parameters.len() >= 6 => {
            queue_event(locked, writer, status_event(0))?;
            let Some(device) = device else {
                // Page timeout.
                return queue_event(locked, writer, connection_complete(0x04, 0, parameters));
            };
            let handle = locked.next_handle;
            locked.next_handle += 1;
            locked.links.push(Link {
                handle,
                device,
                incoming: opcode == 0x0409,
                channels: Vec::new(),
                next_cid: 0x0050,
                next_ident: 1,
                typing: false,
                audio_packets: 0,
                bad_audio: 0,
                hands_free: None,
                gateway_channel: 0,
            });
            queue_event(locked, writer, connection_complete(0, handle, parameters))
        }
        // Reject Connection Request: the reason becomes the status.
        0x040A if parameters.len() >= 7 => {
            queue_event(locked, writer, status_event(0))?;
            queue_event(locked, writer, connection_complete(parameters[6], 0, parameters))
        }
        // Authentication Requested: the controller asks for a key.
        0x0411 => {
            let Some(index) = by_handle else { return queue_event(locked, writer, status_event(0x02)) };
            queue_event(locked, writer, status_event(0))?;
            let address = DEVICES[locked.links[index].device].address;
            queue_event(locked, writer, with_address(0x17, &address, &[]))
        }
        // Link Key Request Reply: the device checks it has the same key.
        0x040B if parameters.len() >= 22 => {
            queue_event(locked, writer, complete(0, &parameters[..6]))?;
            let Some(device) = device else { return Ok(()) };
            let handle = locked.links.iter().find(|link| link.device == device).map_or(0, |link| link.handle);
            let status = if parameters[6..22] == link_key(device) { 0 } else { 0x06 };
            queue_event(locked, writer, authentication_complete(status, handle))
        }
        // Link Key Request Negative Reply: pairing, starting with the
        // host's input/output capabilities.
        0x040C if parameters.len() >= 6 => {
            queue_event(locked, writer, complete(0, &parameters[..6]))?;
            queue_event(locked, writer, with_address(0x31, parameters, &[]))
        }
        // IO Capability Request Reply: ours in return, then the pairing
        // the two make possible.
        0x042B if parameters.len() >= 9 => {
            queue_event(locked, writer, complete(0, &parameters[..6]))?;
            let Some(device) = device else { return Ok(()) };
            let capability = DEVICES[device].capability;
            queue_event(locked, writer, with_address(0x32, parameters, &[capability, 0, 0x05]))?;
            if capability == 2 {
                // A keyboard and a host with a display: the host shows a
                // passkey, the user types it on the keyboard (here, in a
                // second and a half).
                queue_event(locked, writer, with_address(0x3B, parameters, &PASSKEY.to_le_bytes()))?;
                later(state, writer, 1500, move |locked, _, writer| paired(locked, writer, device, 5));
                Ok(())
            } else {
                // Just works: the host is asked to confirm.
                queue_event(locked, writer, with_address(0x33, parameters, &654321u32.to_le_bytes()))
            }
        }
        // IO Capability Request Negative Reply.
        0x0434 if parameters.len() >= 7 => {
            queue_event(locked, writer, complete(0, &parameters[..6]))?;
            match device {
                Some(device) => not_paired(locked, writer, device, parameters[6]),
                None => Ok(()),
            }
        }
        // User Confirmation Request (Negative) Reply.
        0x042C | 0x042D if parameters.len() >= 6 => {
            queue_event(locked, writer, complete(0, &parameters[..6]))?;
            match device {
                Some(device) if opcode == 0x042C => paired(locked, writer, device, 4),
                Some(device) => not_paired(locked, writer, device, 0x05),
                None => Ok(()),
            }
        }
        // Set Connection Encryption. A device that connected back then
        // opens its HID channels.
        0x0413 => {
            let Some(index) = by_handle else { return queue_event(locked, writer, status_event(0x02)) };
            queue_event(locked, writer, status_event(0))?;
            let [low, high] = handle.to_le_bytes();
            queue_event(locked, writer, vec![0x08, 4, 0, low, high, 1])?;
            let link = &locked.links[index];
            if link.incoming && link.device == HEADSET {
                // A headset coming back sets up hands-free: first it asks
                // where the host's gateway is.
                later(state, writer, 100, move |locked, _, writer| {
                    let Some(link) = locked.links.iter_mut().find(|link| link.handle == handle) else { return Ok(()) };
                    let request = open_channel(link, PSM_SDP);
                    send_signal(locked, writer, handle, request)
                });
            }
            if link.incoming && DEVICES[link.device].hid != Hid::None {
                later(state, writer, 100, move |locked, _, writer| {
                    let Some(link) = locked.links.iter_mut().find(|link| link.handle == handle) else { return Ok(()) };
                    let request = open_channel(link, PSM_CONTROL);
                    send_signal(locked, writer, handle, request)
                });
            }
            Ok(())
        }
        // Disconnect. A keyboard or mouse comes back once, a second later.
        0x0406 => {
            let Some(index) = by_handle else { return queue_event(locked, writer, status_event(0x02)) };
            queue_event(locked, writer, status_event(0))?;
            let link = locked.links.remove(index);
            let [low, high] = handle.to_le_bytes();
            queue_event(locked, writer, vec![0x05, 4, 0, low, high, 0x16])?;
            let device = link.device;
            if (DEVICES[device].hid != Hid::None || device == HEADSET) && !locked.reconnected.contains(&device) {
                locked.reconnected.push(device);
                later(state, writer, 1000, move |locked, _, writer| {
                    let class = DEVICES[device].class.to_le_bytes();
                    queue_event(
                        locked,
                        writer,
                        with_address(0x04, &DEVICES[device].address, &[class[0], class[1], class[2], 1]),
                    )
                });
            }
            Ok(())
        }
        _ => queue_event(locked, writer, complete(0, &[])),
    }
}

/// Starts opening a channel to `psm` from the device's side: returns the
/// signal to send.
fn open_channel(link: &mut Link, psm: u16) -> Vec<u8> {
    let ours = link.next_cid;
    link.next_cid += 1;
    link.channels.push(Channel { psm, ours, theirs: 0, outgoing: true, configured: 0 });
    signal(link, 0x02, &[psm.to_le_bytes(), ours.to_le_bytes()].concat())
}

/// An L2CAP signaling command with the link's next identifier.
fn signal(link: &mut Link, code: u8, body: &[u8]) -> Vec<u8> {
    let ident = link.next_ident;
    link.next_ident = link.next_ident.wrapping_add(1).max(1);
    response(code, ident, body)
}

fn response(code: u8, ident: u8, body: &[u8]) -> Vec<u8> {
    let mut command = vec![code, ident];
    command.extend_from_slice(&(body.len() as u16).to_le_bytes());
    command.extend_from_slice(body);
    command
}

/// Our configuration request for a channel: nothing but the default MTU.
fn configure(link: &mut Link, theirs: u16) -> Vec<u8> {
    signal(link, 0x04, &[&theirs.to_le_bytes()[..], &[0, 0, 1, 2], &672u16.to_le_bytes()].concat())
}

fn send_signal(state: &mut State, writer: &Writer, handle: u16, command: Vec<u8>) -> io::Result<()> {
    send_l2cap(state, writer, handle, 0x0001, &command)
}

/// Sends an L2CAP frame to the host in one ACL packet.
fn send_l2cap(state: &mut State, writer: &Writer, handle: u16, cid: u16, payload: &[u8]) -> io::Result<()> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    // A whole frame, flushable.
    packet.extend_from_slice(&(handle | 0x2000).to_le_bytes());
    packet.extend_from_slice(&(4 + payload.len() as u16).to_le_bytes());
    packet.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    packet.extend_from_slice(&cid.to_le_bytes());
    packet.extend_from_slice(payload);
    match state.acl_requests.pop_front() {
        Some(id) => send_bulk_in(writer, id, &packet),
        None => {
            state.acl_waiting.push_back(packet);
            Ok(())
        }
    }
}

/// Answers QEMU's bulk IN request `id` with an ACL packet.
fn send_bulk_in(writer: &Writer, id: u64, packet: &[u8]) -> io::Result<()> {
    let mut header = [ACL_IN, STATUS_SUCCESS, 0, 0, 0, 0, 0, 0, 0, 0];
    header[2..4].copy_from_slice(&(packet.len() as u16).to_le_bytes());
    send(writer, BULK_PACKET, id, &header, packet)
}

/// ACL data from the host: an L2CAP frame (the host's are small enough to
/// come whole) for one of the devices.
fn acl_from_host(locked: &mut State, state: &Shared, writer: &Writer, data: &[u8]) -> io::Result<()> {
    let handle = u16::from_le_bytes([data[0], data[1]]) & 0x0FFF;
    let Some(frame) = data.get(4..) else { return Ok(()) };
    let [low, high, cid_low, cid_high, payload @ ..] = frame else { return Ok(()) };
    let length = usize::from(u16::from_le_bytes([*low, *high]));
    let payload = &payload[..length.min(payload.len())];
    let cid = u16::from_le_bytes([*cid_low, *cid_high]);
    let Some(link) = locked.links.iter_mut().find(|link| link.handle == handle) else { return Ok(()) };
    if cid != 0x0001 {
        let channel = link.channels.iter().find(|channel| channel.ours == cid);
        let (psm, theirs) = (channel.map(|channel| channel.psm), channel.map_or(0, |channel| channel.theirs));
        let outgoing = channel.is_some_and(|channel| channel.outgoing);
        // What to send: (CID, payload), and signals.
        let mut sends = Vec::new();
        let mut signals = Vec::new();
        let mut start_typing = false;
        // SET_PROTOCOL (boot): handshake "successful", then input.
        if psm == Some(PSM_CONTROL) && payload.first() == Some(&0x70) {
            let control = link.channels.iter().find(|channel| channel.psm == PSM_CONTROL).map_or(0, |c| c.theirs);
            start_typing = !link.typing;
            link.typing = true;
            sends.push((control, vec![0x00]));
        }
        if psm == Some(PSM_SDP) && !outgoing {
            sends.push((theirs, handsfree::sdp_answer(payload)));
        }
        if psm == Some(PSM_SDP) && outgoing {
            match handsfree::gateway_channel(payload) {
                Some(channel) => {
                    link.gateway_channel = channel;
                    signals.push(open_channel(link, PSM_RFCOMM));
                }
                None => eprintln!("xtask: pretend headset: the host's SDP server has no hands-free gateway"),
            }
        }
        if psm == Some(PSM_RFCOMM) {
            let hands_free = link.hands_free.get_or_insert_with(HandsFree::responder);
            let frames = hands_free.receive(payload);
            let finished = hands_free.done.then(|| std::mem::take(&mut hands_free.answers));
            if let Some(answers) = finished.filter(|answers| !answers.is_empty()) {
                eprintln!("xtask: pretend headset: hands-free connected: {}", answers.join("; "));
            }
            sends.extend(frames.into_iter().map(|frame| (theirs, frame)));
        }
        for signal in signals {
            send_signal(locked, writer, handle, signal)?;
        }
        for (cid, payload) in sends {
            send_l2cap(locked, writer, handle, cid, &payload)?;
        }
        if start_typing {
            type_input(state, writer, handle);
        }
        if psm == Some(PSM_AVDTP)
            && let Some(reply) = avdtp_sink(payload)
        {
            send_l2cap(locked, writer, handle, theirs, &reply)?;
        }
        if psm == Some(PSM_MEDIA) {
            let frames = sbc_frames(payload);
            let Some(link) = locked.links.iter_mut().find(|link| link.handle == handle) else { return Ok(()) };
            link.audio_packets += 1;
            if frames.is_none() {
                link.bad_audio += 1;
            }
            let (count, bad) = (link.audio_packets, link.bad_audio);
            if count == 1 || count % 500 == 0 {
                eprintln!("xtask: pretend headset: {count} audio packets, {bad} broken");
            }
            if let Some(frames) = frames {
                use std::io::Write;
                let file = std::fs::OpenOptions::new().create(true).append(true).open(&locked.audio_file);
                file.and_then(|mut file| file.write_all(frames))?;
            }
        }
        return Ok(());
    }
    let mut commands = payload;
    let mut replies = Vec::new();
    let mut data = Vec::new();
    while let [code, ident, low, high, rest @ ..] = commands {
        let length = usize::from(u16::from_le_bytes([*low, *high])).min(rest.len());
        let body = &rest[..length];
        commands = &rest[length..];
        let word = |at: usize| body.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        match code {
            // Connection request: accepted, and our configuration.
            0x02 => {
                let (mut psm, theirs) = (word(0), word(2));
                if psm == PSM_AVDTP && link.channels.iter().any(|channel| channel.psm == PSM_AVDTP) {
                    psm = PSM_MEDIA;
                }
                let ours = link.next_cid;
                link.next_cid += 1;
                link.channels.push(Channel { psm, ours, theirs, outgoing: false, configured: 0 });
                replies.push(response(
                    0x03,
                    *ident,
                    &[ours.to_le_bytes(), theirs.to_le_bytes(), [0, 0], [0, 0]].concat(),
                ));
                replies.push(configure(link, theirs));
            }
            // Connection response to ours: configure it; after the
            // control channel, open the interrupt one.
            0x03 => {
                let (theirs, ours, result) = (word(0), word(2), word(4));
                if result != 0 {
                    continue;
                }
                let Some(channel) = link.channels.iter_mut().find(|channel| channel.ours == ours) else { continue };
                channel.theirs = theirs;
                let psm = channel.psm;
                replies.push(configure(link, theirs));
                if psm == PSM_CONTROL {
                    replies.push(open_channel(link, PSM_INTERRUPT));
                }
            }
            // Configuration request: fine.
            0x04 => {
                let ours = word(0);
                let theirs = link.channels.iter().find(|channel| channel.ours == ours).map_or(0, |c| c.theirs);
                replies.push(response(0x05, *ident, &[theirs.to_le_bytes(), [0, 0], [0, 0]].concat()));
                configured(link, ours, 1, &mut data);
            }
            // Configuration response: to ours.
            0x05 if word(4) == 0 => configured(link, word(0), 2, &mut data),
            // Disconnection request.
            0x06 => {
                let ours = word(0);
                link.channels.retain(|channel| channel.ours != ours);
                replies.push(response(0x07, *ident, &body[..4.min(body.len())]));
            }
            // Information request: not supported.
            0x0A => replies.push(response(0x0B, *ident, &[&body[..2.min(body.len())], &[1, 0][..]].concat())),
            // Responses, and command rejects.
            _ => {}
        }
    }
    for reply in replies {
        send_signal(locked, writer, handle, reply)?;
    }
    for (cid, payload) in data {
        send_l2cap(locked, writer, handle, cid, &payload)?;
    }
    Ok(())
}

/// Half of a channel's configuration is done (`bit`); once both are, a
/// channel the device opened starts being used: it asks the host's SDP
/// server about the gateway, or starts RFCOMM to it.
fn configured(link: &mut Link, ours: u16, bit: u8, data: &mut Vec<(u16, Vec<u8>)>) {
    let Some(channel) = link.channels.iter_mut().find(|channel| channel.ours == ours) else { return };
    let was_open = channel.configured == 3;
    channel.configured |= bit;
    if was_open || channel.configured != 3 || !channel.outgoing {
        return;
    }
    let (psm, theirs) = (channel.psm, channel.theirs);
    match psm {
        PSM_SDP => data.push((theirs, handsfree::sdp_query())),
        PSM_RFCOMM => {
            let (hands_free, first) = HandsFree::initiator(link.gateway_channel);
            link.hands_free = Some(hands_free);
            data.push((theirs, first));
        }
        _ => {}
    }
}

/// The headset's answer to an AVDTP command: one audio sink endpoint
/// (SEID 1) that takes any SBC settings with 8 subbands, bitpool 2 to 53.
fn avdtp_sink(message: &[u8]) -> Option<Vec<u8>> {
    let [header, signal, ..] = *message else { return None };
    if header & 0x03 != 0 {
        // A response (the host doesn't ask us to answer those).
        return None;
    }
    let accept = |body: &[u8]| [&[header & 0xF0 | 0x02, signal][..], body].concat();
    Some(match signal & 0x3F {
        // Discover: SEID 1, free, audio, sink.
        0x01 => accept(&[1 << 2, 0x08]),
        // Get (all) capabilities: media transport, SBC.
        0x02 | 0x0C => accept(&[0x01, 0x00, 0x07, 0x06, 0x00, 0x00, 0xFF, 0xFF, 2, 53]),
        // Set configuration, open, start, close, suspend, abort.
        0x03 | 0x06..=0x0A => accept(&[]),
        // Anything else: general reject.
        _ => vec![header & 0xF0 | 0x01, signal],
    })
}

/// The SBC frames in an A2DP audio packet (RTP header, frame count,
/// frames), if they're all whole and well-formed.
fn sbc_frames(packet: &[u8]) -> Option<&[u8]> {
    let count = usize::from(*packet.get(12)? & 0x0F);
    let frames = packet.get(13..)?;
    let mut rest = frames;
    for _ in 0..count {
        let [0x9C, settings, bitpool, ..] = *rest else { return None };
        let blocks = 4 * (usize::from(settings >> 4 & 3) + 1);
        let mode = settings >> 2 & 3;
        let subbands = if settings & 1 != 0 { 8 } else { 4 };
        let channels = if mode == 0 { 1 } else { 2 };
        let bitpool = usize::from(bitpool);
        let audio_bits = match mode {
            0 | 1 => blocks * channels * bitpool,
            2 => blocks * bitpool,
            _ => subbands + blocks * bitpool,
        };
        let length = 4 + 4 * subbands * channels / 8 + audio_bits.div_ceil(8);
        rest = rest.get(length..)?;
    }
    rest.is_empty().then_some(frames)
}

/// What a device sends once it's set up: the keyboard types a command,
/// the mouse moves right and down and clicks.
fn type_input(state: &Shared, writer: &Writer, handle: u16) {
    let (state, writer) = (state.clone(), writer.clone());
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        let reports: Vec<Vec<u8>> = {
            let locked = state.lock().unwrap();
            let Some(link) = locked.links.iter().find(|link| link.handle == handle) else { return };
            match DEVICES[link.device].hid {
                Hid::Keyboard => TYPED
                    .bytes()
                    .flat_map(|byte| {
                        let usage = match byte {
                            b'a'..=b'z' => 0x04 + byte - b'a',
                            b' ' => 0x2C,
                            _ => 0x28,
                        };
                        [vec![0xA1, 1, 0, 0, usage, 0, 0, 0, 0, 0], vec![0xA1, 1, 0, 0, 0, 0, 0, 0, 0, 0]]
                    })
                    .collect(),
                Hid::Mouse => {
                    let mut moves = vec![vec![0xA1, 2, 0, 10, 5, 0]; 5];
                    moves.push(vec![0xA1, 2, 1, 0, 0, 0]);
                    moves.push(vec![0xA1, 2, 0, 0, 0, 0]);
                    moves
                }
                Hid::None => Vec::new(),
            }
        };
        for report in reports {
            thread::sleep(Duration::from_millis(15));
            let mut locked = state.lock().unwrap();
            let Some(link) = locked.links.iter().find(|link| link.handle == handle) else { return };
            let Some(interrupt) = link.channels.iter().find(|channel| channel.psm == PSM_INTERRUPT) else { return };
            let theirs = interrupt.theirs;
            if send_l2cap(&mut locked, &writer, handle, theirs, &report).is_err() {
                return;
            }
        }
    });
}

/// Inquiry results, a little later: the keyboard and the mouse with
/// their names (extended inquiry results), the headset without (inquiry
/// result with RSSI).
fn inquiry(state: &Mutex<State>, writer: &Writer) -> io::Result<()> {
    for device in [KEYBOARD, MOUSE] {
        thread::sleep(Duration::from_millis(300));
        let Device { address, class, rssi, name, .. } = DEVICES[device];
        let mut event = vec![0x2F, 255, 1];
        event.extend_from_slice(&address);
        event.extend_from_slice(&[1, 0]);
        event.extend_from_slice(&class.to_le_bytes()[..3]);
        event.extend_from_slice(&[0x34, 0x12, rssi as u8]);
        let mut eir = vec![2, 0x01, 0x06, name.len() as u8 + 1, 0x09];
        eir.extend_from_slice(name.as_bytes());
        eir.resize(240, 0);
        event.extend_from_slice(&eir);
        queue_event(&mut state.lock().unwrap(), writer, event)?;
    }
    thread::sleep(Duration::from_millis(300));
    let Device { address, class, rssi, .. } = DEVICES[HEADSET];
    let mut event = vec![0x22, 15, 1];
    event.extend_from_slice(&address);
    event.extend_from_slice(&[1, 0]);
    event.extend_from_slice(&class.to_le_bytes()[..3]);
    event.extend_from_slice(&[0x78, 0x56, rssi as u8]);
    queue_event(&mut state.lock().unwrap(), writer, event)?;
    thread::sleep(Duration::from_millis(300));
    // Inquiry complete.
    queue_event(&mut state.lock().unwrap(), writer, vec![0x01, 1, 0])
}

/// Sends an event now if QEMU is polling for them, or later.
fn queue_event(state: &mut State, writer: &Writer, event: Vec<u8>) -> io::Result<()> {
    if !state.receiving {
        state.waiting.push_back(event);
        return Ok(());
    }
    send_event(writer, &event)
}

/// Sends an event in packets of at most the endpoint's size, as USB
/// controllers do.
fn send_event(writer: &Writer, event: &[u8]) -> io::Result<()> {
    for chunk in event.chunks(usize::from(PACKET_SIZE)) {
        let mut header = [EVENTS, STATUS_SUCCESS, 0, 0];
        header[2..4].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
        send(writer, INTERRUPT_PACKET, 0, &header, chunk)?;
    }
    Ok(())
}

/// One interface, 0: Bluetooth (class 0xE0/1/1).
fn interface_info() -> Vec<u8> {
    let mut info = vec![0u8; 4 + 32 * 4];
    info[0] = 1;
    info[4 + 32] = 0xE0;
    info[4 + 64] = 1;
    info[4 + 96] = 1;
    info
}

/// Endpoint types (0 control, 2 bulk, 3 interrupt), intervals,
/// interfaces and packet sizes, indexed OUT 0-15 then IN 16-31.
fn ep_info() -> Vec<u8> {
    let mut types = [255u8; 32];
    let mut intervals = [0u8; 32];
    let interfaces = [0u8; 32];
    let mut sizes = [0u16; 32];
    for (index, kind) in [(0, 0), (16, 0), (16 + usize::from(EVENTS & 0xF), 3), (16 + usize::from(ACL_IN & 0xF), 2)]
        .into_iter()
        .chain([(usize::from(ACL_OUT), 2)])
    {
        types[index] = kind;
        sizes[index] = PACKET_SIZE;
    }
    intervals[16 + usize::from(EVENTS & 0xF)] = 1;
    let mut info = Vec::new();
    info.extend_from_slice(&types);
    info.extend_from_slice(&intervals);
    info.extend_from_slice(&interfaces);
    sizes.iter().for_each(|size| info.extend_from_slice(&size.to_le_bytes()));
    info
}

fn device_descriptor() -> Vec<u8> {
    let mut d = vec![18, 1, 0x00, 0x02, 0xE0, 1, 1, 64];
    d.extend_from_slice(&0x8087u16.to_le_bytes());
    d.extend_from_slice(&0x07DCu16.to_le_bytes());
    d.extend_from_slice(&0x0001u16.to_le_bytes());
    d.extend_from_slice(&[1, 2, 0, 1]);
    d
}

/// One configuration: the HCI interface with its event endpoint and bulk
/// endpoints (the real one also has an interface for voice, unused here).
fn configuration_descriptor() -> Vec<u8> {
    let mut d = vec![9, 2, 0, 0, 1, 1, 0, 0xE0, 50];
    d.extend_from_slice(&[9, 4, 0, 0, 3, 0xE0, 1, 1, 0]);
    let size = PACKET_SIZE.to_le_bytes();
    d.extend_from_slice(&[7, 5, EVENTS, 3, size[0], size[1], 1]);
    d.extend_from_slice(&[7, 5, ACL_IN, 2, size[0], size[1], 0]);
    d.extend_from_slice(&[7, 5, ACL_OUT, 2, size[0], size[1], 0]);
    let total = (d.len() as u16).to_le_bytes();
    d[2..4].copy_from_slice(&total);
    d
}
