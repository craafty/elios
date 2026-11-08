//! The stack once the controller is set up: scanning, connections,
//! pairing, and what connections carry (L2CAP channels, HID), all driven
//! by the controller's events.
//!
//! Everything runs on the adapter's thread, one event at a time. Commands
//! are still sent one by one, waiting for the controller's Command
//! Complete or Command Status (milliseconds); what they start (a
//! connection, an inquiry, pairing) finishes in later events, handled as
//! they come, so a keyboard keeps typing while another device pairs.
//!
//! Pairing (Secure Simple Pairing, Bluetooth 2.1) as the host sees it:
//! connect, ask for authentication; the controller asks for a link key,
//! and without one (or when pairing afresh) for our input/output
//! capabilities. We say we have a display and yes/no buttons, so a
//! keyboard gets a passkey to type (the controller tells us the number;
//! the user types it on the keyboard), and a device without buttons or a
//! display (most mice and headsets) pairs without asking ("just works";
//! we confirm). The controllers then compute a link key, which we keep
//! (`keys.rs`). Then encryption, then the HID channels.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::hid::{self, Control, Hid};
use super::keys::{Keys, Paired};
use super::l2cap::{self, L2cap};
use super::*;
use crate::keyboard::{self, Key};

/// How long pairing may take, the user typing a passkey included.
const PAIRING_TIMEOUT_MS: u64 = 60_000;
/// How long a scan may take: the inquiry is about 10 s, then names take
/// up to about 5 s each.
const SCAN_TIMEOUT_MS: u64 = 90_000;

// Commands.
const CREATE_CONNECTION: u16 = 0x0405;
const DISCONNECT: u16 = 0x0406;
const ACCEPT_CONNECTION_REQUEST: u16 = 0x0409;
const REJECT_CONNECTION_REQUEST: u16 = 0x040A;
const LINK_KEY_REQUEST_REPLY: u16 = 0x040B;
const LINK_KEY_REQUEST_NEGATIVE_REPLY: u16 = 0x040C;
const PIN_CODE_REQUEST_REPLY: u16 = 0x040D;
const PIN_CODE_REQUEST_NEGATIVE_REPLY: u16 = 0x040E;
const AUTHENTICATION_REQUESTED: u16 = 0x0411;
const SET_CONNECTION_ENCRYPTION: u16 = 0x0413;
const IO_CAPABILITY_REQUEST_REPLY: u16 = 0x042B;
const USER_CONFIRMATION_REQUEST_REPLY: u16 = 0x042C;
const USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY: u16 = 0x042D;
const USER_PASSKEY_REQUEST_NEGATIVE_REPLY: u16 = 0x042F;
const IO_CAPABILITY_REQUEST_NEGATIVE_REPLY: u16 = 0x0434;

// Events.
const EVENT_CONNECTION_COMPLETE: u8 = 0x03;
const EVENT_CONNECTION_REQUEST: u8 = 0x04;
const EVENT_DISCONNECTION_COMPLETE: u8 = 0x05;
const EVENT_AUTHENTICATION_COMPLETE: u8 = 0x06;
const EVENT_ENCRYPTION_CHANGE: u8 = 0x08;
const EVENT_NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
const EVENT_PIN_CODE_REQUEST: u8 = 0x16;
const EVENT_LINK_KEY_REQUEST: u8 = 0x17;
const EVENT_LINK_KEY_NOTIFICATION: u8 = 0x18;
const EVENT_IO_CAPABILITY_REQUEST: u8 = 0x31;
const EVENT_IO_CAPABILITY_RESPONSE: u8 = 0x32;
const EVENT_USER_CONFIRMATION_REQUEST: u8 = 0x33;
const EVENT_USER_PASSKEY_REQUEST: u8 = 0x34;
const EVENT_SIMPLE_PAIRING_COMPLETE: u8 = 0x36;
const EVENT_USER_PASSKEY_NOTIFICATION: u8 = 0x3B;

/// Disconnection reasons.
const REMOTE_USER_ENDED: u8 = 0x13;
const AUTHENTICATION_FAILURE: u8 = 0x05;
/// Why a connection from an unknown device is refused.
const UNACCEPTABLE_ADDRESS: u8 = 0x0F;
/// Why pairing is refused when nobody asked for it.
const PAIRING_NOT_ALLOWED: u8 = 0x18;

/// Our input/output capabilities: a display and yes/no buttons.
const DISPLAY_YES_NO: u8 = 0x01;
/// General bonding (keep the key), with protection against someone in
/// the middle where the devices can manage it.
const GENERAL_BONDING_MITM: u8 = 0x05;

/// The services devices may open channels to.
const SERVED: &[u16] = &[hid::CONTROL, hid::INTERRUPT];

struct Connection {
    handle: u16,
    address: [u8; 6],
    class: u32,
    name: String,
    /// Whether the device connected to us (a paired device coming back):
    /// then it opens the HID channels, not us.
    incoming: bool,
    encrypted: bool,
    l2cap: L2cap,
    hid: Option<Hid>,
    /// ACL packets sent that the controller hasn't reported done.
    in_flight: u16,
    /// An L2CAP frame arriving in pieces.
    partial: Vec<u8>,
}

/// Pairing (or connecting to a paired device), which the user asked for.
struct Pairing {
    address: [u8; 6],
    /// Pairing afresh, not using a key kept from before.
    fresh: bool,
    class: u32,
    name: String,
    deadline: u64,
}

struct Scan {
    deadline: u64,
    /// Devices still to ask for their names, after the inquiry.
    nameless: VecDeque<[u8; 6]>,
    /// Whether the inquiry is over and a name request is out.
    asking_names: bool,
}

pub(super) struct Stack {
    hci: Hci,
    connections: Vec<Connection>,
    keys: Keys,
    pairing: Option<Pairing>,
    scan: Option<Scan>,
    /// ACL packets the controller can take now, and how big each may be.
    credits: u16,
    acl_size: usize,
    /// ACL packets waiting for the controller to have room: handle and
    /// packet.
    acl_waiting: VecDeque<(u16, Vec<u8>)>,
    /// Keys typed on Bluetooth keyboards, for the terminal.
    typed: Vec<Key>,
}

enum Work {
    Removed,
    Request(Request),
    Packets,
}

impl Stack {
    pub(super) fn new(hci: Hci) -> Self {
        let (size, count) = STATUS.lock().acl_buffers;
        Self {
            hci,
            connections: Vec::new(),
            keys: Keys::new(),
            pairing: None,
            scan: None,
            credits: count.max(1),
            acl_size: usize::from(size.max(27)),
            acl_waiting: VecDeque::new(),
            typed: Vec::new(),
        }
    }

    /// Serves requests and events until the adapter is removed.
    pub(super) fn run(mut self) {
        let adapter = self.hci.adapter.clone();
        loop {
            self.keys.load_if_ready();
            self.publish();
            let deadline = self.next_deadline();
            let stash_waiting = !self.hci.stash.is_empty();
            let work = adapter.arrived.wait_until_deadline(deadline, || {
                if adapter.removed.load(Ordering::Acquire) {
                    return Some(Work::Removed);
                }
                if let Some(request) = adapter.requests.lock().pop_front() {
                    return Some(Work::Request(request));
                }
                (stash_waiting || !adapter.incoming.lock().is_empty()).then_some(Work::Packets)
            });
            match work {
                Some(Work::Removed) => {
                    info!("bluetooth: adapter removed");
                    self.connections.clear();
                    let mut status = STATUS.lock();
                    status.present = false;
                    status.state = "absent";
                    status.devices.clear();
                    status.paired.clear();
                    status.pairing = None;
                    return;
                }
                Some(Work::Request(request)) => self.request(request),
                Some(Work::Packets) => {
                    while let Some(packet) = self.hci.next(0) {
                        match packet {
                            Packet::Event(event) => self.event(&event),
                            Packet::Acl(data) => self.acl(&data),
                        }
                    }
                }
                None => {}
            }
            self.timers();
        }
    }

    /// The soonest thing to do without being woken: a key repeat, a
    /// timeout, another look for the saved keys.
    fn next_deadline(&self) -> Option<u64> {
        let repeats = self.connections.iter().filter_map(|c| c.hid.as_ref()?.next_repeat());
        repeats
            .chain(self.pairing.as_ref().map(|p| p.deadline))
            .chain(self.scan.as_ref().map(|s| s.deadline))
            .chain(self.keys.next_try())
            .min()
    }

    fn timers(&mut self) {
        let now = time::ticks();
        for connection in &mut self.connections {
            if let Some(hid) = connection.hid.as_mut() {
                while let Some(key) = hid.repeat(now) {
                    self.typed.push(key);
                }
            }
        }
        self.typed.drain(..).for_each(keyboard::send_key);
        if self.pairing.as_ref().is_some_and(|p| now >= p.deadline) {
            self.pairing_failed(String::from("timed out"));
        }
        if self.scan.as_ref().is_some_and(|s| now >= s.deadline) {
            self.finish_scan(Some(String::from("the scan didn't finish")));
        }
    }

    /// Shows paired devices and their connections in `/proc/bluetooth`.
    fn publish(&self) {
        let paired: Vec<PairedView> = self
            .keys
            .list
            .iter()
            .map(|paired| {
                let connection = self.connections.iter().find(|c| c.address == paired.address);
                let state = match connection {
                    None => String::from("not connected"),
                    Some(Connection { hid: Some(hid), .. }) => {
                        format!("connected, {}, {} reports", hid.kinds(), hid.reports)
                    }
                    Some(connection) if connection.encrypted => String::from("connected"),
                    Some(_) => String::from("connecting"),
                };
                PairedView { address: paired.address, name: paired.name.clone(), state }
            })
            .collect();
        STATUS.lock().paired = paired;
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::Scan => {
                if let Err(error) = self.start_scan() {
                    self.finish_scan(Some(error));
                }
            }
            Request::Pair { address, fresh } => {
                if let Err(error) = self.pair(address, fresh) {
                    if self.pairing.is_some() {
                        self.pairing_failed(error);
                    } else {
                        // Refused before it started.
                        let mut status = STATUS.lock();
                        status.pairing = None;
                        status.error = Some(error);
                    }
                }
            }
            Request::Disconnect(address) => self.disconnect(&address, REMOTE_USER_ENDED),
            Request::Forget(address) => {
                if self.keys.remove(&address) {
                    info!("bluetooth: forgot {}", format_address(&address));
                }
                self.disconnect(&address, REMOTE_USER_ENDED);
            }
        }
    }

    fn dispatch(&mut self, event: &[u8]) -> Result<(), String> {
        let (code, parameters) = (event[0], &event[2..]);
        match code {
            EVENT_INQUIRY_COMPLETE => {
                self.inquiry_complete();
                Ok(())
            }
            EVENT_INQUIRY_RESULT | EVENT_INQUIRY_RESULT_RSSI | EVENT_EXTENDED_INQUIRY_RESULT => {
                self.found(code, parameters);
                Ok(())
            }
            EVENT_REMOTE_NAME_REQUEST_COMPLETE => {
                self.name_complete(parameters);
                Ok(())
            }
            EVENT_CONNECTION_REQUEST => self.connection_request(parameters),
            EVENT_CONNECTION_COMPLETE => self.connection_complete(parameters),
            EVENT_DISCONNECTION_COMPLETE => {
                self.disconnection_complete(parameters);
                Ok(())
            }
            EVENT_AUTHENTICATION_COMPLETE => self.authentication_complete(parameters),
            EVENT_ENCRYPTION_CHANGE => self.encryption_change(parameters),
            EVENT_NUMBER_OF_COMPLETED_PACKETS => {
                self.completed_packets(parameters);
                Ok(())
            }
            EVENT_LINK_KEY_REQUEST => self.link_key_request(parameters),
            EVENT_LINK_KEY_NOTIFICATION => {
                self.link_key_notification(parameters);
                Ok(())
            }
            EVENT_PIN_CODE_REQUEST => self.pin_code_request(parameters),
            EVENT_IO_CAPABILITY_REQUEST => self.io_capability_request(parameters),
            EVENT_IO_CAPABILITY_RESPONSE => {
                if let (Some(address), Some(&[capability, _, authentication])) =
                    (address_at(parameters, 0), parameters.get(6..9))
                {
                    info!(
                        "bluetooth: {} has {} (authentication {authentication:#04x})",
                        format_address(&address),
                        capability_text(capability)
                    );
                }
                Ok(())
            }
            EVENT_USER_CONFIRMATION_REQUEST => self.user_confirmation_request(parameters),
            EVENT_USER_PASSKEY_REQUEST => {
                // We have no keyboard to type the device's number on (we
                // said so); this shouldn't come.
                let address = address_at(parameters, 0).ok_or_else(short)?;
                self.hci.command(USER_PASSKEY_REQUEST_NEGATIVE_REPLY, &address).map(drop)
            }
            EVENT_USER_PASSKEY_NOTIFICATION => {
                let (address, passkey) = (address_at(parameters, 0).ok_or_else(short)?, word32_at(parameters, 6));
                info!("bluetooth: type {passkey:06} on {}, then Enter", format_address(&address));
                if let Some(pairing) = STATUS.lock().pairing.as_mut().filter(|p| p.address == address) {
                    pairing.passkey = Some(passkey);
                    pairing.step = "waiting for the passkey";
                }
                Ok(())
            }
            EVENT_SIMPLE_PAIRING_COMPLETE => {
                let address = address_at(parameters, 1).ok_or_else(short)?;
                match parameters[0] {
                    0 => debug!("bluetooth: simple pairing with {} done", format_address(&address)),
                    status => {
                        warn!("bluetooth: pairing with {} failed: {}", format_address(&address), error_text(status))
                    }
                }
                Ok(())
            }
            _ => {
                unexpected_event(event);
                Ok(())
            }
        }
    }

    fn event(&mut self, event: &[u8]) {
        let result = self.dispatch(event);
        if let Err(error) = result {
            warn!("bluetooth: {error}");
            if self.pairing.is_some() {
                self.pairing_failed(error);
            }
        }
    }

    // Scanning.

    fn start_scan(&mut self) -> Result<(), String> {
        STATUS.lock().devices.clear();
        // The general inquiry access code (0x9E8B33), 8 x 1.28 s, any
        // number of answers.
        self.hci.command_status(INQUIRY, &[0x33, 0x8B, 0x9E, 8, 0])?;
        let deadline = time::ticks() + SCAN_TIMEOUT_MS;
        self.scan = Some(Scan { deadline, nameless: VecDeque::new(), asking_names: false });
        Ok(())
    }

    fn found(&mut self, code: u8, parameters: &[u8]) {
        for found in parse_inquiry_result(code, parameters) {
            debug!("bluetooth: found {} class {:#08x}", format_address(&found.address), found.class);
            let mut status = STATUS.lock();
            match status.devices.iter_mut().find(|d| d.address == found.address) {
                Some(known) => {
                    known.rssi = found.rssi.or(known.rssi);
                    known.name = found.name.or(known.name.take());
                }
                None => status.devices.push(found),
            }
        }
    }

    /// The inquiry is over: ask the devices that didn't say their names.
    fn inquiry_complete(&mut self) {
        let Some(scan) = self.scan.as_mut() else { return };
        scan.nameless = STATUS.lock().devices.iter().filter(|d| d.name.is_none()).map(|d| d.address).collect();
        self.next_name();
    }

    fn next_name(&mut self) {
        while let Some(scan) = self.scan.as_mut() {
            scan.asking_names = false;
            let Some(address) = scan.nameless.pop_front() else {
                self.finish_scan(None);
                return;
            };
            let Some(device) = STATUS.lock().devices.iter().find(|d| d.address == address).cloned() else { continue };
            let mut parameters = Vec::from(device.address);
            parameters.extend_from_slice(&[device.scan_mode, 0]);
            parameters.extend_from_slice(&(device.clock_offset | 0x8000).to_le_bytes());
            if self.hci.command_status(REMOTE_NAME_REQUEST, &parameters).is_ok()
                && let Some(scan) = self.scan.as_mut()
            {
                scan.asking_names = true;
                return;
            }
        }
    }

    fn name_complete(&mut self, parameters: &[u8]) {
        let Some(address) = address_at(parameters, 1) else { return };
        if parameters[0] == 0 {
            let name = text(&parameters[7..]);
            if let Some(known) = STATUS.lock().devices.iter_mut().find(|d| d.address == address) {
                known.name = Some(name.clone());
            }
            for connection in self.connections.iter_mut().filter(|c| c.address == address && c.name.is_empty()) {
                connection.name = name.clone();
            }
        } else {
            debug!("bluetooth: no name from {} ({})", format_address(&address), error_text(parameters[0]));
        }
        if self.scan.as_ref().is_some_and(|scan| scan.asking_names) {
            self.next_name();
        }
    }

    fn finish_scan(&mut self, error: Option<String>) {
        self.scan = None;
        let mut status = STATUS.lock();
        info!("bluetooth: scan found {} devices", status.devices.len());
        status.state = "ready";
        status.error = error;
    }

    // Connections and pairing.

    /// Connects to `address` to pair with it (`fresh`), or to use the key
    /// kept from pairing before.
    fn pair(&mut self, address: [u8; 6], fresh: bool) -> Result<(), String> {
        let found = STATUS.lock().devices.iter().find(|d| d.address == address).cloned();
        let kept = self.keys.find(&address);
        if !fresh && kept.is_none() {
            return Err(String::from("not paired"));
        }
        let class = found.as_ref().map(|f| f.class).or(kept.map(|k| k.class)).unwrap_or(0);
        let name = found.as_ref().and_then(|f| f.name.clone()).or(kept.map(|k| k.name.clone())).unwrap_or_default();
        info!(
            "bluetooth: {} {} {name}",
            if fresh { "pairing with" } else { "connecting to" },
            format_address(&address)
        );
        self.pairing = Some(Pairing { address, fresh, class, name, deadline: time::ticks() + PAIRING_TIMEOUT_MS });
        // Already connected (it came back by itself): authenticate it
        // again, which pairs afresh.
        if let Some(connection) = self.connections.iter().find(|c| c.address == address) {
            if !fresh {
                self.pairing_done();
                return Ok(());
            }
            let handle = connection.handle;
            set_pairing_step(&address, "authenticating");
            return self.hci.command_status(AUTHENTICATION_REQUESTED, &handle.to_le_bytes());
        }
        set_pairing_step(&address, "connecting");
        let mut parameters = Vec::from(address);
        // Packet types: DM1, DH1, DM3, DH3, DM5, DH5.
        parameters.extend_from_slice(&0xCC18u16.to_le_bytes());
        let (scan_mode, clock_offset) = found.map_or((1, 0), |f| (f.scan_mode, f.clock_offset | 0x8000));
        parameters.extend_from_slice(&[scan_mode, 0]);
        parameters.extend_from_slice(&clock_offset.to_le_bytes());
        // Allow a role switch.
        parameters.push(1);
        self.hci.command_status(CREATE_CONNECTION, &parameters)
    }

    /// A device wants to connect: a paired one coming back (or the one
    /// being paired) may.
    fn connection_request(&mut self, parameters: &[u8]) -> Result<(), String> {
        let address = address_at(parameters, 0).ok_or_else(short)?;
        let link_type = parameters.get(9).copied().unwrap_or(0);
        let wanted = self.keys.find(&address).is_some() || self.pairing.as_ref().is_some_and(|p| p.address == address);
        if link_type == 1 && wanted {
            info!("bluetooth: {} is connecting", format_address(&address));
            // Remain the peripheral (as Linux does by default).
            self.hci.command_status(ACCEPT_CONNECTION_REQUEST, &[&address[..], &[0x01]].concat())
        } else {
            info!("bluetooth: refused a connection from {} (not paired)", format_address(&address));
            self.hci.command_status(REJECT_CONNECTION_REQUEST, &[&address[..], &[UNACCEPTABLE_ADDRESS]].concat())
        }
    }

    fn connection_complete(&mut self, parameters: &[u8]) -> Result<(), String> {
        let (status, handle) = (parameters[0], word_at(parameters, 1) & 0x0FFF);
        let address = address_at(parameters, 3).ok_or_else(short)?;
        if parameters.get(9) != Some(&1) {
            // A voice link: nothing asks for those yet.
            return Ok(());
        }
        if status != 0 {
            self.pairing_failed_for(&address, format!("can't connect: {}", error_text(status)));
            return Ok(());
        }
        let pairing = self.pairing.as_ref().filter(|p| p.address == address);
        let kept = self.keys.find(&address);
        let class = pairing.map(|p| p.class).or(kept.map(|k| k.class)).unwrap_or(0);
        let name = pairing.map(|p| p.name.clone()).or(kept.map(|k| k.name.clone())).unwrap_or_default();
        let incoming = pairing.is_none();
        info!("bluetooth: connected to {} {name} (handle {handle:#05x})", format_address(&address));
        let unnamed = name.is_empty();
        self.connections.push(Connection {
            handle,
            address,
            class,
            name,
            incoming,
            encrypted: false,
            l2cap: L2cap::new(SERVED),
            hid: None,
            in_flight: 0,
            partial: Vec::new(),
        });
        set_pairing_step(&address, "authenticating");
        self.hci.command_status(AUTHENTICATION_REQUESTED, &handle.to_le_bytes())?;
        if unnamed {
            let parameters = [&address[..], &[1, 0, 0, 0]].concat();
            let _ = self.hci.command_status(REMOTE_NAME_REQUEST, &parameters);
        }
        Ok(())
    }

    fn disconnection_complete(&mut self, parameters: &[u8]) {
        if parameters.first() != Some(&0) {
            return;
        }
        let handle = word_at(parameters, 1) & 0x0FFF;
        let reason = parameters.get(3).copied().unwrap_or(0);
        let Some(index) = self.connections.iter().position(|c| c.handle == handle) else { return };
        // Dropping it removes its input devices.
        let connection = self.connections.remove(index);
        self.credits += connection.in_flight;
        self.acl_waiting.retain(|(waiting, _)| *waiting != handle);
        info!(
            "bluetooth: {} {} disconnected: {}",
            format_address(&connection.address),
            connection.name,
            error_text(reason)
        );
        self.pairing_failed_for(&connection.address, format!("disconnected: {}", error_text(reason)));
    }

    fn authentication_complete(&mut self, parameters: &[u8]) -> Result<(), String> {
        let (status, handle) = (parameters[0], word_at(parameters, 1) & 0x0FFF);
        let Some(connection) = self.connections.iter().find(|c| c.handle == handle) else { return Ok(()) };
        let address = connection.address;
        if status != 0 {
            let mut error = format!("authentication failed: {}", error_text(status));
            if status == 0x06 && self.keys.find(&address).is_some() {
                error.push_str(" (the device forgot the pairing: pair it again)");
            }
            warn!("bluetooth: {}: {error}", format_address(&address));
            self.pairing_failed_for(&address, error);
            self.disconnect(&address, AUTHENTICATION_FAILURE);
            return Ok(());
        }
        set_pairing_step(&address, "encrypting");
        let [low, high] = handle.to_le_bytes();
        self.hci.command_status(SET_CONNECTION_ENCRYPTION, &[low, high, 1])
    }

    fn encryption_change(&mut self, parameters: &[u8]) -> Result<(), String> {
        let (status, handle) = (parameters[0], word_at(parameters, 1) & 0x0FFF);
        let enabled = parameters.get(3).copied().unwrap_or(0);
        let Some(index) = self.connections.iter().position(|c| c.handle == handle) else { return Ok(()) };
        let address = self.connections[index].address;
        if status != 0 || enabled == 0 {
            let error = format!("encryption failed: {}", error_text(status));
            self.pairing_failed_for(&address, error.clone());
            self.disconnect(&address, AUTHENTICATION_FAILURE);
            return Err(error);
        }
        let connection = &mut self.connections[index];
        if connection.encrypted {
            // A key refresh.
            return Ok(());
        }
        connection.encrypted = true;
        connection.l2cap.set_secure();
        debug!("bluetooth: {} encrypted", format_address(&address));
        if hid::is_hid(connection.class) {
            set_pairing_step(&address, "connecting HID");
            if !connection.incoming {
                connection.l2cap.connect(hid::CONTROL);
            }
        } else if self.pairing.as_ref().is_some_and(|p| p.address == address) {
            // Paired; nothing more to connect to it yet (audio is for
            // later).
            self.pairing_done();
        }
        self.service(index);
        Ok(())
    }

    fn link_key_request(&mut self, parameters: &[u8]) -> Result<(), String> {
        let address = address_at(parameters, 0).ok_or_else(short)?;
        let fresh = self.pairing.as_ref().is_some_and(|p| p.address == address && p.fresh);
        match self.keys.find(&address).filter(|_| !fresh) {
            Some(paired) => {
                let parameters = [&address[..], &paired.key[..]].concat();
                self.hci.command(LINK_KEY_REQUEST_REPLY, &parameters).map(drop)
            }
            None => self.hci.command(LINK_KEY_REQUEST_NEGATIVE_REPLY, &address).map(drop),
        }
    }

    fn link_key_notification(&mut self, parameters: &[u8]) {
        let (Some(address), Some(key)) = (address_at(parameters, 0), parameters.get(6..22)) else { return };
        let key_type = parameters.get(22).copied().unwrap_or(0);
        let connection = self.connections.iter().find(|c| c.address == address);
        let pairing = self.pairing.as_ref().filter(|p| p.address == address);
        let class = connection.map(|c| c.class).or(pairing.map(|p| p.class)).unwrap_or(0);
        let name = connection
            .map(|c| c.name.clone())
            .filter(|name| !name.is_empty())
            .or(pairing.map(|p| p.name.clone()))
            .unwrap_or_default();
        info!("bluetooth: paired with {} {name} (key type {key_type})", format_address(&address));
        self.keys.store(Paired { address, key: key.try_into().unwrap(), key_type, class, name });
    }

    /// Pairing an older device (before Bluetooth 2.1), which wants a PIN:
    /// "0000", which headsets and mice use.
    fn pin_code_request(&mut self, parameters: &[u8]) -> Result<(), String> {
        let address = address_at(parameters, 0).ok_or_else(short)?;
        if !self.pairing.as_ref().is_some_and(|p| p.address == address) {
            return self.hci.command(PIN_CODE_REQUEST_NEGATIVE_REPLY, &address).map(drop);
        }
        let mut parameters = Vec::from(address);
        parameters.push(4);
        parameters.extend_from_slice(b"0000");
        parameters.resize(6 + 1 + 16, 0);
        self.hci.command(PIN_CODE_REQUEST_REPLY, &parameters).map(drop)
    }

    fn io_capability_request(&mut self, parameters: &[u8]) -> Result<(), String> {
        let address = address_at(parameters, 0).ok_or_else(short)?;
        if self.pairing.as_ref().is_some_and(|p| p.address == address) {
            // No out-of-band data.
            let parameters = [&address[..], &[DISPLAY_YES_NO, 0, GENERAL_BONDING_MITM]].concat();
            self.hci.command(IO_CAPABILITY_REQUEST_REPLY, &parameters).map(drop)
        } else {
            info!("bluetooth: refused to pair with {} (not asked to)", format_address(&address));
            let parameters = [&address[..], &[PAIRING_NOT_ALLOWED]].concat();
            self.hci.command(IO_CAPABILITY_REQUEST_NEGATIVE_REPLY, &parameters).map(drop)
        }
    }

    /// Numeric comparison, or "just works" when the device can't show or
    /// confirm a number: the user asked to pair with it, so yes.
    fn user_confirmation_request(&mut self, parameters: &[u8]) -> Result<(), String> {
        let address = address_at(parameters, 0).ok_or_else(short)?;
        if self.pairing.as_ref().is_some_and(|p| p.address == address) {
            debug!("bluetooth: confirming pairing with {} ({:06})", format_address(&address), word32_at(parameters, 6));
            self.hci.command(USER_CONFIRMATION_REQUEST_REPLY, &address).map(drop)
        } else {
            self.hci.command(USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY, &address).map(drop)
        }
    }

    fn disconnect(&mut self, address: &[u8; 6], reason: u8) {
        let Some(connection) = self.connections.iter().find(|c| c.address == *address) else { return };
        let [low, high] = connection.handle.to_le_bytes();
        if let Err(error) = self.hci.command_status(DISCONNECT, &[low, high, reason]) {
            warn!("bluetooth: {error}");
        }
    }

    fn pairing_done(&mut self) {
        let Some(pairing) = self.pairing.take() else { return };
        info!("bluetooth: {} {} ready", format_address(&pairing.address), pairing.name);
        let mut status = STATUS.lock();
        status.pairing = None;
        status.error = None;
    }

    fn pairing_failed(&mut self, error: String) {
        let Some(pairing) = self.pairing.take() else { return };
        warn!("bluetooth: {} failed: {error}", format_address(&pairing.address));
        {
            let mut status = STATUS.lock();
            status.pairing = None;
            status.error = Some(error);
        }
        // A connection made for it that didn't get anywhere.
        if self.connections.iter().any(|c| c.address == pairing.address && c.hid.is_none()) {
            self.disconnect(&pairing.address, REMOTE_USER_ENDED);
        }
    }

    fn pairing_failed_for(&mut self, address: &[u8; 6], error: String) {
        if self.pairing.as_ref().is_some_and(|p| p.address == *address) {
            self.pairing_failed(error);
        }
    }

    // ACL data and what it carries.

    fn completed_packets(&mut self, parameters: &[u8]) {
        let count = usize::from(parameters.first().copied().unwrap_or(0));
        for entry in parameters[1..].as_chunks::<4>().0.iter().take(count) {
            let handle = u16::from_le_bytes([entry[0], entry[1]]) & 0x0FFF;
            let done = u16::from_le_bytes([entry[2], entry[3]]);
            self.credits += done;
            if let Some(connection) = self.connections.iter_mut().find(|c| c.handle == handle) {
                connection.in_flight = connection.in_flight.saturating_sub(done);
            }
        }
        self.flush_acl();
    }

    /// An ACL packet: a whole L2CAP frame, or a piece of one.
    fn acl(&mut self, data: &[u8]) {
        let Some(header) = data.get(..4) else { return };
        let word = u16::from_le_bytes([header[0], header[1]]);
        let (handle, boundary) = (word & 0x0FFF, word >> 12 & 0x3);
        let Some(index) = self.connections.iter().position(|c| c.handle == handle) else { return };
        let connection = &mut self.connections[index];
        match boundary {
            // A continuation.
            0b01 => connection.partial.extend_from_slice(&data[4..]),
            _ => connection.partial = data[4..].to_vec(),
        }
        let partial = &connection.partial;
        if partial.len() < 4 {
            return;
        }
        let length = 4 + usize::from(u16::from_le_bytes([partial[0], partial[1]]));
        if partial.len() < length {
            return;
        }
        let mut frame = core::mem::take(&mut connection.partial);
        frame.truncate(length);
        let cid = u16::from_le_bytes([frame[2], frame[3]]);
        let payload = &frame[4..];
        if cid == l2cap::SIGNALING {
            connection.l2cap.receive_signal(payload);
        } else {
            match (connection.l2cap.open_psm(cid), connection.hid.as_mut()) {
                (Some(hid::CONTROL), Some(hid)) => {
                    if let Control::Unplugged = hid.control(payload) {
                        let address = connection.address;
                        info!("bluetooth: {} asked to be forgotten", format_address(&address));
                        self.keys.remove(&address);
                        self.disconnect(&address, REMOTE_USER_ENDED);
                        return;
                    }
                }
                (Some(hid::INTERRUPT), Some(hid)) => {
                    if let Some(leds) = hid.interrupt(payload, &mut self.typed) {
                        connection.l2cap.send(hid::INTERRUPT, leds);
                    }
                    self.typed.drain(..).for_each(keyboard::send_key);
                }
                _ => {}
            }
        }
        self.service(index);
    }

    /// After something changed on connection `index`: reports L2CAP's
    /// errors, opens the next HID channel or starts HID, and sends what
    /// L2CAP queued.
    fn service(&mut self, index: usize) {
        let connection = &mut self.connections[index];
        let address = connection.address;
        let errors: Vec<String> = core::mem::take(&mut connection.l2cap.errors);
        if connection.hid.is_none() && connection.encrypted && hid::is_hid(connection.class) {
            let l2cap = &mut connection.l2cap;
            if l2cap.is_open(hid::CONTROL) && l2cap.is_open(hid::INTERRUPT) {
                let (hid, control, interrupt) = Hid::new(&connection.name, connection.class);
                info!("bluetooth: {} {}: {}", format_address(&address), connection.name, hid.kinds());
                l2cap.send(hid::CONTROL, control);
                l2cap.send(hid::INTERRUPT, interrupt);
                connection.hid = Some(hid);
                if self.pairing.as_ref().is_some_and(|p| p.address == address) {
                    self.pairing_done();
                }
            } else if !connection.incoming && l2cap.is_open(hid::CONTROL) && l2cap.channel(hid::INTERRUPT).is_none() {
                l2cap.connect(hid::INTERRUPT);
            }
        }
        let connection = &mut self.connections[index];
        let handle = connection.handle;
        let outbox: Vec<(u16, Vec<u8>)> = core::mem::take(&mut connection.l2cap.outbox);
        for (cid, payload) in outbox {
            self.send_l2cap(handle, cid, &payload);
        }
        for error in errors {
            warn!("bluetooth: {}: {error}", format_address(&address));
            self.pairing_failed_for(&address, error);
        }
    }

    /// Sends an L2CAP frame, in as many ACL packets as it takes.
    fn send_l2cap(&mut self, handle: u16, cid: u16, payload: &[u8]) {
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        frame.extend_from_slice(&cid.to_le_bytes());
        frame.extend_from_slice(payload);
        for (index, piece) in frame.chunks(self.acl_size).enumerate() {
            // Packet boundary: the start of a frame (flushable), or a
            // continuation.
            let boundary: u16 = if index == 0 { 0b10 } else { 0b01 };
            let mut packet = Vec::with_capacity(4 + piece.len());
            packet.extend_from_slice(&(handle | boundary << 12).to_le_bytes());
            packet.extend_from_slice(&(piece.len() as u16).to_le_bytes());
            packet.extend_from_slice(piece);
            self.acl_waiting.push_back((handle, packet));
        }
        self.flush_acl();
    }

    /// Sends waiting ACL packets while the controller has room for them.
    fn flush_acl(&mut self) {
        while self.credits > 0 {
            let Some((handle, packet)) = self.acl_waiting.pop_front() else { break };
            self.credits -= 1;
            if let Some(connection) = self.connections.iter_mut().find(|c| c.handle == handle) {
                connection.in_flight += 1;
            }
            self.hci.adapter.send_acl(packet);
        }
    }
}

fn set_pairing_step(address: &[u8; 6], step: &'static str) {
    if let Some(pairing) = STATUS.lock().pairing.as_mut().filter(|p| p.address == *address) {
        pairing.step = step;
    }
}

fn capability_text(capability: u8) -> &'static str {
    match capability {
        0 => "a display only",
        1 => "a display and yes/no buttons",
        2 => "a keyboard only",
        3 => "no display or keyboard",
        _ => "unknown input/output",
    }
}

fn short() -> String {
    String::from("short event")
}

fn address_at(parameters: &[u8], at: usize) -> Option<[u8; 6]> {
    parameters.get(at..at + 6)?.try_into().ok()
}

fn word_at(parameters: &[u8], at: usize) -> u16 {
    parameters.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

fn word32_at(parameters: &[u8], at: usize) -> u32 {
    parameters.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}
