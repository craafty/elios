//! Bluetooth: the host side of the Host Controller Interface (HCI).
//!
//! A Bluetooth controller (a USB device on laptops) runs the radio and the
//! link layer itself; the host talks to it in HCI packets: commands to it,
//! events from it, and ACL data both ways (what connections carry). The
//! USB driver (`drivers/usb/xhci.rs`) only moves those packets: commands
//! as control requests, events from the interrupt endpoint, ACL data over
//! the bulk endpoints. It hands them to an [`Adapter`], and this module's
//! thread runs everything else: loading the controller's firmware patch
//! (Intel's, in `intel.rs`) and setting it up; then the stack
//! (`stack.rs`): finding devices nearby, connecting and pairing, keeping
//! link keys (`keys.rs`), L2CAP channels (`l2cap.rs`) and keyboards and
//! mice (`hid.rs`).
//!
//! `/proc/bluetooth` shows the adapter (address, firmware, devices found,
//! paired devices) and takes requests ("scan", "pair ADDRESS", ...) for
//! the `bluetooth` command, as `/proc/wifi` does for Wi-Fi.

mod a2dp;
mod hid;
mod intel;
mod keys;
mod l2cap;
mod sdp;
mod stack;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::errno::Errno;
use crate::sync::SpinLock;
use crate::task::{self, WaitQueue};
use crate::time;

/// A packet from the controller.
pub enum Packet {
    /// An event: code, parameter length, parameters.
    Event(Vec<u8>),
    /// ACL data: handle and flags, length, data.
    Acl(Vec<u8>),
}

/// What the stack asks of the adapter's thread.
enum Request {
    Scan,
    /// Pair with a device afresh, or connect to one paired before.
    Pair {
        address: [u8; 6],
        fresh: bool,
    },
    Disconnect([u8; 6]),
    Forget([u8; 6]),
}

/// A controller, as its transport (the USB driver) and the stack share it.
/// Everything here is behind spinlocks: the transport runs with its
/// controller's lock held and must never wait for the stack.
pub struct Adapter {
    pub vendor: u16,
    pub product: u16,
    /// HCI commands (opcode, length, parameters) for the transport to send.
    commands: SpinLock<VecDeque<Vec<u8>>>,
    /// ACL packets for the transport to send.
    acl_out: SpinLock<VecDeque<Vec<u8>>>,
    /// Packets from the controller, for the stack's thread.
    incoming: SpinLock<VecDeque<Packet>>,
    requests: SpinLock<VecDeque<Request>>,
    /// Wakes the stack's thread: packets or requests arrived.
    arrived: WaitQueue,
    /// Wakes the transport: there's something to send.
    wake_transport: fn(),
    removed: AtomicBool,
}

impl Adapter {
    pub fn new(vendor: u16, product: u16, wake_transport: fn()) -> Self {
        Self {
            vendor,
            product,
            commands: SpinLock::new(VecDeque::new()),
            acl_out: SpinLock::new(VecDeque::new()),
            incoming: SpinLock::new(VecDeque::new()),
            requests: SpinLock::new(VecDeque::new()),
            arrived: WaitQueue::new(),
            wake_transport,
            removed: AtomicBool::new(false),
        }
    }

    /// For the transport: a packet arrived.
    pub fn receive(&self, packet: Packet) {
        self.incoming.lock().push_back(packet);
        self.arrived.wake_all();
    }

    /// For the transport: the next command to send.
    pub fn next_command(&self) -> Option<Vec<u8>> {
        self.commands.lock().pop_front()
    }

    /// For the transport: the next ACL packet to send.
    pub fn next_acl(&self) -> Option<Vec<u8>> {
        self.acl_out.lock().pop_front()
    }

    /// For the transport: the device is gone.
    pub fn remove(&self) {
        self.removed.store(true, Ordering::Release);
        self.arrived.wake_all();
    }

    fn send_acl(&self, packet: Vec<u8>) {
        self.acl_out.lock().push_back(packet);
        (self.wake_transport)();
    }

    fn send_command(&self, opcode: u16, parameters: &[u8]) {
        let mut packet = Vec::with_capacity(3 + parameters.len());
        packet.extend_from_slice(&opcode.to_le_bytes());
        packet.push(parameters.len() as u8);
        packet.extend_from_slice(parameters);
        self.commands.lock().push_back(packet);
        (self.wake_transport)();
    }
}

/// A device found by a scan.
#[derive(Clone)]
struct Found {
    address: [u8; 6],
    class: u32,
    rssi: Option<i8>,
    name: Option<String>,
    /// For a name request: page scan repetition mode and clock offset.
    scan_mode: u8,
    clock_offset: u16,
}

/// What `/proc/bluetooth` shows.
struct Status {
    present: bool,
    state: &'static str,
    error: Option<String>,
    usb_id: (u16, u16),
    address: [u8; 6],
    /// HCI version, manufacturer, LMP subversion.
    version: (u8, u16, u16),
    firmware: Option<String>,
    /// ACL packet size and how many the controller buffers.
    acl_buffers: (u16, u16),
    devices: Vec<Found>,
    paired: Vec<PairedView>,
    pairing: Option<PairingView>,
}

/// A paired device, and how its connection is.
struct PairedView {
    address: [u8; 6],
    name: String,
    state: String,
}

/// Pairing (or connecting) under way.
struct PairingView {
    address: [u8; 6],
    step: &'static str,
    /// The number to type on a keyboard being paired.
    passkey: Option<u32>,
}

static STATUS: SpinLock<Status> = SpinLock::new(Status {
    present: false,
    state: "absent",
    error: None,
    usb_id: (0, 0),
    address: [0; 6],
    version: (0, 0, 0),
    firmware: None,
    acl_buffers: (0, 0),
    devices: Vec::new(),
    paired: Vec::new(),
    pairing: None,
});

/// The adapter in use (the first one found).
static ADAPTER: SpinLock<Option<Arc<Adapter>>> = SpinLock::new(None);

/// Wakes the adapter's thread (audio for a headset came).
fn wake() {
    if let Some(adapter) = ADAPTER.lock().clone() {
        adapter.arrived.wake_all();
    }
}

/// Called by the transport for each controller found: starts its thread.
pub fn register(adapter: Arc<Adapter>) {
    {
        let mut current = ADAPTER.lock();
        if current.as_ref().is_some_and(|a| !a.removed.load(Ordering::Acquire)) {
            info!("bluetooth: another adapter ({:04x}:{:04x}) left unused", adapter.vendor, adapter.product);
            return;
        }
        *current = Some(adapter.clone());
    }
    {
        let mut status = STATUS.lock();
        status.present = true;
        status.state = "starting";
        status.error = None;
        status.usb_id = (adapter.vendor, adapter.product);
        status.devices.clear();
    }
    task::spawn("bluetooth", move || run(adapter));
}

pub fn present() -> bool {
    STATUS.lock().present
}

fn set_state(state: &'static str) {
    STATUS.lock().state = state;
    debug!("bluetooth: {state}");
}

/// The adapter's thread: sets the controller up, then serves requests.
fn run(adapter: Arc<Adapter>) {
    let mut hci = Hci { adapter: adapter.clone(), stash: VecDeque::new() };
    if let Err(error) = start(&mut hci) {
        warn!("bluetooth: {error}");
        let mut status = STATUS.lock();
        status.state = "failed";
        status.error = Some(error);
        return;
    }
    set_state("ready");
    stack::Stack::new(hci).run();
}

/// Logs an event nobody waited for.
fn unexpected_event(event: &[u8]) {
    debug!("bluetooth: event {:02x?}", &event[..event.len().min(32)]);
}

/// Resets the controller, patches its firmware if it wants that, and sets
/// it up the way the rest of the stack expects.
fn start(hci: &mut Hci) -> Result<(), String> {
    // Some controllers answer their first command saying no more can be
    // sent; a reset starts the count afresh (Linux does this first too).
    hci.command(RESET, &[])?;
    if hci.adapter.vendor == 0x8087 {
        set_state("patching");
        let firmware = intel::setup(hci)?;
        STATUS.lock().firmware = Some(firmware);
    }
    set_state("starting");
    hci.command(RESET, &[])?;
    let version = hci.command(READ_LOCAL_VERSION, &[])?;
    let address = hci.command(READ_BD_ADDR, &[])?;
    let buffers = hci.command(READ_BUFFER_SIZE, &[])?;
    let field = |data: &[u8], index: usize| data.get(index).copied().unwrap_or(0);
    let word = |data: &[u8], index: usize| u16::from_le_bytes([field(data, index), field(data, index + 1)]);
    {
        let mut status = STATUS.lock();
        status.version = (field(&version, 1), word(&version, 5), word(&version, 7));
        status.address.copy_from_slice(address.get(1..7).ok_or("short BD_ADDR")?);
        status.acl_buffers = (word(&buffers, 1), word(&buffers, 4));
        info!(
            "bluetooth: address {}, HCI version {} ({}), manufacturer {:#06x}, LMP subversion {:#06x}, \
             {} ACL buffers of {} bytes",
            format_address(&status.address),
            status.version.0,
            core_version(status.version.0),
            status.version.1,
            status.version.2,
            status.acl_buffers.1,
            status.acl_buffers.0
        );
    }
    // Every event a BR/EDR host uses (Linux's default mask for 2.0 and
    // later controllers).
    hci.command(SET_EVENT_MASK, &[0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D])?;
    // Secure Simple Pairing, and inquiry results with RSSI and extended
    // data (names, mostly).
    hci.command(WRITE_SIMPLE_PAIRING_MODE, &[1])?;
    hci.command(WRITE_INQUIRY_MODE, &[2])?;
    // A laptop computer (major class 1, minor 3).
    hci.command(WRITE_CLASS_OF_DEVICE, &[0x0C, 0x01, 0x00])?;
    let mut name = [0u8; 248];
    name[..5].copy_from_slice(b"eliOS");
    hci.command(WRITE_LOCAL_NAME, &name)?;
    Ok(())
}

/// Bluetooth core specification versions, by HCI version number.
fn core_version(version: u8) -> &'static str {
    match version {
        0 => "1.0b",
        1 => "1.1",
        2 => "1.2",
        3 => "2.0",
        4 => "2.1",
        5 => "3.0",
        6 => "4.0",
        7 => "4.1",
        8 => "4.2",
        9 => "5.0",
        10 => "5.1",
        11 => "5.2",
        12 => "5.3",
        13 => "5.4",
        _ => "?",
    }
}

/// The devices in an inquiry result event's parameters (after the event
/// header).
fn parse_inquiry_result(code: u8, parameters: &[u8]) -> Vec<Found> {
    let count = usize::from(parameters.first().copied().unwrap_or(0));
    let mut found = Vec::new();
    let address = |bytes: &[u8]| -> [u8; 6] { bytes[..6].try_into().unwrap() };
    let class = |bytes: &[u8]| u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16;
    match code {
        // Address, page scan repetition mode, reserved, class, clock
        // offset, RSSI; one after another (as Linux reads them).
        EVENT_INQUIRY_RESULT_RSSI => {
            for entry in parameters[1..].as_chunks::<14>().0.iter().take(count) {
                found.push(Found {
                    address: address(entry),
                    scan_mode: entry[6],
                    class: class(&entry[8..11]),
                    clock_offset: u16::from_le_bytes([entry[11], entry[12]]),
                    rssi: Some(entry[13] as i8),
                    name: None,
                });
            }
        }
        // The same without RSSI, and two reserved bytes.
        EVENT_INQUIRY_RESULT => {
            for entry in parameters[1..].as_chunks::<14>().0.iter().take(count) {
                found.push(Found {
                    address: address(entry),
                    scan_mode: entry[6],
                    class: class(&entry[9..12]),
                    clock_offset: u16::from_le_bytes([entry[12], entry[13]]),
                    rssi: None,
                    name: None,
                });
            }
        }
        // One device, with its extended inquiry response (EIR).
        EVENT_EXTENDED_INQUIRY_RESULT if parameters.len() >= 15 => {
            let entry = &parameters[1..];
            found.push(Found {
                address: address(entry),
                scan_mode: entry[6],
                class: class(&entry[8..11]),
                clock_offset: u16::from_le_bytes([entry[11], entry[12]]),
                rssi: Some(entry[13] as i8),
                name: eir_name(&entry[14..]),
            });
        }
        _ => {}
    }
    found
}

/// The name in extended inquiry response data: a list of (length, type,
/// data) fields; type 9 is the complete name, 8 a shortened one.
fn eir_name(mut data: &[u8]) -> Option<String> {
    let mut short = None;
    while let [length, rest @ ..] = data {
        let length = usize::from(*length);
        if length == 0 || length > rest.len() {
            break;
        }
        let (field, next) = rest.split_at(length);
        match field[0] {
            0x09 => return Some(text(&field[1..])),
            0x08 => short = Some(text(&field[1..])),
            _ => {}
        }
        data = next;
    }
    short
}

/// UTF-8 text up to the first NUL.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Bluetooth addresses travel least significant byte first, and are
/// written most significant first.
fn format_address(address: &[u8; 6]) -> String {
    let parts: Vec<String> = address.iter().rev().map(|b| format!("{b:02X}")).collect();
    parts.join(":")
}

/// An address as written (`11:22:33:44:55:66`), in the order it travels.
fn parse_address(text: &str) -> Option<[u8; 6]> {
    let mut address = [0u8; 6];
    let mut parts = text.split(':');
    for byte in address.iter_mut().rev() {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(address)
}

/// The text of `/proc/bluetooth`.
pub fn status_text() -> String {
    let status = STATUS.lock();
    let mut text = String::new();
    let _ = writeln!(text, "state: {}", status.state);
    let _ = writeln!(text, "usb: {:04x}:{:04x}", status.usb_id.0, status.usb_id.1);
    if status.version.0 != 0 || status.address != [0; 6] {
        let _ = writeln!(text, "address: {}", format_address(&status.address));
        let _ = writeln!(
            text,
            "version: Bluetooth {} (HCI {}), manufacturer {:#06x}, LMP subversion {:#06x}",
            core_version(status.version.0),
            status.version.0,
            status.version.1,
            status.version.2
        );
    }
    if let Some(firmware) = &status.firmware {
        let _ = writeln!(text, "firmware: {firmware}");
    }
    if let Some(error) = &status.error {
        let _ = writeln!(text, "error: {error}");
    }
    let _ = writeln!(text, "devices:");
    for device in &status.devices {
        let _ = write!(text, "{}  class {:06x}", format_address(&device.address), device.class);
        if let Some(rssi) = device.rssi {
            let _ = write!(text, "  {rssi} dBm");
        }
        let _ = writeln!(text, "  {}", device.name.as_deref().unwrap_or("(no name)"));
    }
    let _ = writeln!(text, "paired:");
    for paired in &status.paired {
        let _ = writeln!(text, "{}  {}  {}", format_address(&paired.address), paired.name, paired.state);
    }
    if let Some(pairing) = &status.pairing {
        let _ = write!(text, "pairing: {} {}", format_address(&pairing.address), pairing.step);
        if let Some(passkey) = pairing.passkey {
            let _ = write!(text, " passkey {passkey:06}");
        }
        let _ = writeln!(text);
    }
    text
}

/// Requests written to `/proc/bluetooth`.
pub fn control(request: &[u8]) -> Result<(), Errno> {
    let request = core::str::from_utf8(request).map_err(|_| Errno::EINVAL)?.trim();
    let adapter = ADAPTER.lock().clone().ok_or(Errno::ENODEV)?;
    let (verb, argument) = request.split_once(' ').unwrap_or((request, ""));
    let address = || parse_address(argument.trim()).ok_or(Errno::EINVAL);
    let request = {
        let mut status = STATUS.lock();
        let idle = status.state == "ready" && status.pairing.is_none();
        let request = match verb {
            "scan" if idle => {
                // Set now, so a reader right after the write sees it.
                status.state = "scanning";
                Request::Scan
            }
            "pair" | "connect" if idle => {
                let address = address()?;
                status.pairing = Some(PairingView { address, step: "starting", passkey: None });
                Request::Pair { address, fresh: verb == "pair" }
            }
            "scan" | "pair" | "connect" => return Err(Errno::EBUSY),
            "disconnect" => Request::Disconnect(address()?),
            "forget" => Request::Forget(address()?),
            _ => return Err(Errno::EINVAL),
        };
        status.error = None;
        request
    };
    adapter.requests.lock().push_back(request);
    adapter.arrived.wake_all();
    Ok(())
}

// Commands (opcode group << 10 | command).
const INQUIRY: u16 = 0x0401;
const REMOTE_NAME_REQUEST: u16 = 0x0419;
const SET_EVENT_MASK: u16 = 0x0C01;
const RESET: u16 = 0x0C03;
const WRITE_LOCAL_NAME: u16 = 0x0C13;
const WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
const WRITE_INQUIRY_MODE: u16 = 0x0C45;
const WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0C56;
const READ_LOCAL_VERSION: u16 = 0x1001;
const READ_BUFFER_SIZE: u16 = 0x1005;
const READ_BD_ADDR: u16 = 0x1009;

// Events.
const EVENT_INQUIRY_RESULT: u8 = 0x02;
const EVENT_INQUIRY_COMPLETE: u8 = 0x01;
const EVENT_REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;
const EVENT_COMMAND_COMPLETE: u8 = 0x0E;
const EVENT_COMMAND_STATUS: u8 = 0x0F;
const EVENT_INQUIRY_RESULT_RSSI: u8 = 0x22;
const EVENT_EXTENDED_INQUIRY_RESULT: u8 = 0x2F;

/// How long a controller may take to answer a command.
const COMMAND_TIMEOUT_MS: u64 = 5000;

/// The stack's end of an adapter: sends commands and waits for events.
struct Hci {
    adapter: Arc<Adapter>,
    /// Packets that arrived while waiting for something else, oldest
    /// first.
    stash: VecDeque<Packet>,
}

impl Hci {
    /// The next packet, waiting up to `timeout_ms` for one.
    fn next(&mut self, timeout_ms: u64) -> Option<Packet> {
        if let Some(packet) = self.stash.pop_front() {
            return Some(packet);
        }
        let adapter = &self.adapter;
        adapter.arrived.wait_until_deadline(Some(time::ticks() + timeout_ms), || adapter.incoming.lock().pop_front())
    }

    /// Waits up to `timeout_ms` for an event `wanted` accepts; others are
    /// kept, in order, for later.
    fn wait_event(&mut self, timeout_ms: u64, mut wanted: impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
        let deadline = time::ticks() + timeout_ms;
        let mut skipped = VecDeque::new();
        // Look through what's kept first.
        let mut result = None;
        while let Some(packet) = self.stash.pop_front() {
            match packet {
                Packet::Event(event) if event.len() >= 2 && wanted(&event) => {
                    result = Some(event);
                    break;
                }
                other => skipped.push_back(other),
            }
        }
        while result.is_none() && !self.adapter.removed.load(Ordering::Acquire) {
            let now = time::ticks();
            if now >= deadline {
                break;
            }
            let adapter = &self.adapter;
            let packet = adapter.arrived.wait_until_deadline(Some(deadline), || {
                adapter
                    .incoming
                    .lock()
                    .pop_front()
                    .map(Some)
                    .or_else(|| adapter.removed.load(Ordering::Acquire).then_some(None))
            });
            match packet {
                Some(Some(Packet::Event(event))) if event.len() >= 2 && wanted(&event) => result = Some(event),
                Some(Some(other)) => skipped.push_back(other),
                _ => break,
            }
        }
        skipped.append(&mut self.stash);
        self.stash = skipped;
        result
    }

    /// Sends a command and waits for its Command Complete event; returns
    /// the return parameters (starting with the status, which must be 0).
    fn command(&mut self, opcode: u16, parameters: &[u8]) -> Result<Vec<u8>, String> {
        let event = self.command_event(opcode, parameters, EVENT_COMMAND_COMPLETE)?;
        let result = event[5..].to_vec();
        match result.first() {
            Some(0) | None => Ok(result),
            Some(status) => Err(format!("command {opcode:#06x} failed: {}", error_text(*status))),
        }
    }

    /// Sends a command the controller answers with Command Status (it
    /// reports later, in other events), and checks it was accepted.
    fn command_status(&mut self, opcode: u16, parameters: &[u8]) -> Result<(), String> {
        let event = self.command_event(opcode, parameters, EVENT_COMMAND_STATUS)?;
        match event[2] {
            0 => Ok(()),
            status => Err(format!("command {opcode:#06x} refused: {}", error_text(status))),
        }
    }

    /// Sends a command and waits for an event with `code` (for Command
    /// Complete and Command Status, about this command): returns the
    /// whole event. A Command Status refusing the command is an error.
    fn command_event(&mut self, opcode: u16, parameters: &[u8], code: u8) -> Result<Vec<u8>, String> {
        self.adapter.send_command(opcode, parameters);
        let opcode_bytes = opcode.to_le_bytes();
        let mut refused = None;
        let event = self.wait_event(COMMAND_TIMEOUT_MS, |event| {
            let about_this = |at: usize| event.get(at..at + 2) == Some(&opcode_bytes[..]);
            match event[0] {
                EVENT_COMMAND_STATUS if about_this(4) && code != EVENT_COMMAND_STATUS && event[2] != 0 => {
                    refused = Some(event[2]);
                    true
                }
                EVENT_COMMAND_COMPLETE if code == EVENT_COMMAND_COMPLETE => about_this(3),
                EVENT_COMMAND_STATUS if code == EVENT_COMMAND_STATUS => about_this(4),
                other => other == code && !matches!(code, EVENT_COMMAND_COMPLETE | EVENT_COMMAND_STATUS),
            }
        });
        if let Some(status) = refused {
            return Err(format!("command {opcode:#06x} refused: {}", error_text(status)));
        }
        let event = event.ok_or_else(|| format!("no answer to command {opcode:#06x}"))?;
        let minimum = match code {
            EVENT_COMMAND_COMPLETE => 5,
            EVENT_COMMAND_STATUS => 6,
            _ => 2,
        };
        if event.len() < minimum {
            return Err(format!("short event {:02x?} for command {opcode:#06x}", event));
        }
        Ok(event)
    }
}

/// The HCI error codes seen most.
fn error_text(status: u8) -> String {
    let text = match status {
        0x01 => "unknown command",
        0x02 => "unknown connection",
        0x04 => "page timeout",
        0x05 => "authentication failure",
        0x06 => "PIN or key missing",
        0x08 => "connection timeout",
        0x0C => "command disallowed",
        0x11 => "unsupported feature or parameter",
        0x12 => "invalid parameters",
        0x13 => "remote user ended the connection",
        0x16 => "connection ended by the local host",
        0x1F => "unspecified error",
        _ => return format!("error {status:#04x}"),
    };
    format!("{text} ({status:#04x})")
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn inquiry_results_and_names_are_parsed() {
            // An extended inquiry result: one device, its address, mode,
            // class 0x002540 (a keyboard), RSSI -60, and EIR data with
            // flags and a complete name.
            let mut event = alloc::vec![1, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 1, 0, 0x40, 0x25, 0x00, 0x34, 0x12, 0xC4];
            event.extend_from_slice(&[2, 0x01, 0x06, 9, 0x09]);
            event.extend_from_slice(b"Keyboard");
            event.extend_from_slice(&[0; 20]);
            let found = parse_inquiry_result(EVENT_EXTENDED_INQUIRY_RESULT, &event);
            assert_eq!(found.len(), 1);
            assert_eq!(format_address(&found[0].address), "11:22:33:44:55:66");
            assert_eq!(found[0].class, 0x002540);
            assert_eq!(found[0].rssi, Some(-60));
            assert_eq!(found[0].clock_offset, 0x1234);
            assert_eq!(found[0].name.as_deref(), Some("Keyboard"));
            // A shortened name is used when there's no complete one.
            assert_eq!(eir_name(&[4, 0x08, b'a', b'b', b'c', 0, 0]).as_deref(), Some("abc"));
            assert_eq!(eir_name(&[0, 0, 0]), None);
            // Inquiry results with RSSI: two devices.
            let mut event = alloc::vec![2];
            for n in 0..2u8 {
                event.extend_from_slice(&[n, 0, 0, 0, 0, 0, 1, 0, 0x0C, 0x01, 0x00, 0, 0, 0xD0]);
            }
            let found = parse_inquiry_result(EVENT_INQUIRY_RESULT_RSSI, &event);
            assert_eq!(found.len(), 2);
            assert_eq!(found[1].address[0], 1);
            assert_eq!(found[1].class, 0x00010C);
            assert_eq!(found[1].rssi, Some(-48));
        }
    }

    /// Waits up to 15 s for `/proc/bluetooth` to satisfy `wanted`;
    /// returns its text.
    fn wait_for(what: &str, wanted: impl Fn(&str) -> bool) -> String {
        let deadline = time::ticks() + 15_000;
        loop {
            let text = status_text();
            if wanted(&text) {
                return text;
            }
            assert!(time::ticks() < deadline, "waiting for {what}:\n{text}");
            task::sleep_ms(20);
        }
    }

    crate::ktest! {
        fn the_pretend_controller_is_patched_and_scans() {
            // QEMU gets xtask's pretend Intel controller: it checks each
            // patch command against the same file, and knows three devices.
            let text = wait_for("the controller", |t| t.contains("state: ready") || t.contains("state: failed"));
            assert!(text.contains("state: ready"), "{text}");
            // Switched on only if every command matched the file.
            assert!(text.contains("patch 0x42 (intel/ibt-hw-37.7.10-fw-1.80.2.3.d.bseq)"), "{text}");
            control(b"scan").unwrap();
            assert_eq!(control(b"scan"), Err(Errno::EBUSY));
            let text = wait_for("the scan", |t| t.contains("state: ready"));
            assert!(!text.contains("error:"), "{text}");
            assert!(text.contains("Test keyboard"), "{text}");
            // Its name had to be asked for.
            assert!(text.contains("Test headset"), "{text}");
            assert!(text.contains("Test mouse"), "{text}");
        }
    }

    crate::ktest! {
        fn bluetooth_keyboards_and_mice_pair_and_come_back() {
            let keyboard = "11:22:33:44:55:01";
            let mouse = "11:22:33:44:55:03";
            wait_for("the controller", |t| t.contains("state: ready"));
            // Found by a scan first (the tests run in any order), which
            // tells what each device is.
            if !status_text().contains("Test mouse") {
                control(b"scan").unwrap();
                wait_for("the scan", |t| t.contains("state: ready"));
            }
            // A device that wasn't scanned for (or isn't there) can't be
            // reached.
            control(b"pair 11:22:33:44:55:99").unwrap();
            let text = wait_for("a failed pairing", |t| !t.contains("pairing:"));
            assert!(text.contains("error: can't connect: page timeout"), "{text}");
            // The keyboard: it gets a passkey to type, then is a keyboard.
            control(format!("pair {keyboard}").as_bytes()).unwrap();
            assert_eq!(control(b"scan"), Err(Errno::EBUSY));
            wait_for("the passkey", |t| t.contains("passkey 123456") || !t.contains("pairing:"));
            let text = wait_for("pairing", |t| !t.contains("pairing:"));
            assert!(!text.contains("error:"), "{text}");
            let line = format!("{keyboard}  Test keyboard  connected, keyboard, ");
            wait_for("keyboard reports", |t| t.contains(&line) && !t.contains(&format!("{line}0 reports")));
            assert!(crate::input::find("Test keyboard").is_some());
            // The mouse: just works.
            control(format!("pair {mouse}").as_bytes()).unwrap();
            let text = wait_for("pairing the mouse", |t| !t.contains("pairing:"));
            assert!(!text.contains("error:"), "{text}");
            wait_for("the mouse", |t| t.contains(&format!("{mouse}  Test mouse  connected, mouse, ")));
            assert!(crate::input::find("Test mouse").is_some());
            // Disconnected, the keyboard comes back by itself, with the key
            // kept from pairing.
            control(format!("disconnect {keyboard}").as_bytes()).unwrap();
            wait_for("the keyboard to go", |t| t.contains(&format!("{keyboard}  Test keyboard  not connected")));
            wait_for("the keyboard to come back", |t| t.contains(&line));
            // Forgotten devices are gone, input devices and all.
            control(format!("forget {mouse}").as_bytes()).unwrap();
            control(format!("forget {keyboard}").as_bytes()).unwrap();
            wait_for("forgetting", |t| !t.contains("Test mouse  ") && !t.contains("Test keyboard  "));
            let deadline = time::ticks() + 5000;
            while (crate::input::find("Test mouse").is_some() || crate::input::find("Test keyboard").is_some()) && time::ticks() < deadline {
                task::sleep_ms(20);
            }
            assert!(crate::input::find("Test mouse").is_none() && crate::input::find("Test keyboard").is_none());
            // The mouse tries to come back a second after it was
            // disconnected (and is refused): let that pass before other
            // tests count memory.
            task::sleep_ms(1500);
        }
    }
}
