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
//! a patch loaded. A scan finds two devices: one that sends its name with
//! its inquiry answer and one that has to be asked for it.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::camera::{Writer, hello, send, string_descriptor};

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

/// The devices a scan finds: address, class, RSSI, name.
const KEYBOARD: ([u8; 6], u32, i8, &str) = ([0x01, 0x55, 0x44, 0x33, 0x22, 0x11], 0x002540, -50, "Test keyboard");
const HEADSET: ([u8; 6], u32, i8, &str) = ([0x02, 0x55, 0x44, 0x33, 0x22, 0x11], 0x240404, -65, "Test headset");

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
    /// Bulk IN requests from QEMU, by id (answered when there's data,
    /// which there isn't without connections), so cancels find them.
    bulk_requests: HashMap<u64, u8>,
}

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
            if let Err(error) = serve(stream, steps)
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

fn serve(stream: UnixStream, steps: Vec<Step>) -> io::Result<()> {
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
        bulk_requests: HashMap::new(),
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
                if endpoint & 0x80 != 0 {
                    state.lock().unwrap().bulk_requests.insert(id, endpoint);
                } else {
                    // ACL data sent to us: taken (and dropped; nothing's
                    // connected).
                    let mut reply = body[..10].to_vec();
                    reply[1] = STATUS_SUCCESS;
                    send(&writer, BULK_PACKET, id, &reply, &[])?;
                }
            }
            CANCEL_DATA_PACKET => {
                let endpoint = state.lock().unwrap().bulk_requests.remove(&id);
                if let Some(endpoint) = endpoint {
                    send(&writer, BULK_PACKET, id, &[endpoint, STATUS_CANCELLED, 0, 0, 0, 0, 0, 0, 0, 0], &[])?;
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
            let known = [KEYBOARD, HEADSET].into_iter().find(|device| device.0 == parameters[..6]);
            match known {
                Some(device) => name[..device.3.len()].copy_from_slice(device.3.as_bytes()),
                // Page timeout: nobody answered.
                None => event[2] = 0x04,
            }
            event.extend_from_slice(&name);
            event
        }
        _ => complete(0, &[]),
    };
    queue_event(&mut locked, writer, event)
}

/// Inquiry results, a little later: the keyboard with its name (extended
/// inquiry result), the headset without (inquiry result with RSSI).
fn inquiry(state: &Mutex<State>, writer: &Writer) -> io::Result<()> {
    thread::sleep(Duration::from_millis(300));
    let (address, class, rssi, name) = KEYBOARD;
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
    thread::sleep(Duration::from_millis(300));
    let (address, class, rssi, _) = HEADSET;
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
