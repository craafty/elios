//! The runtime firmware: scanning, joining a network, and carrying its
//! traffic as the `wlan0` network interface.
//!
//! One kernel thread owns the card. It takes requests ("scan", "connect",
//! "disconnect") written to /proc/wifi, and Ethernet frames the network
//! stack sends through [`Wlan`]; everything the card reports comes back
//! through the receive ring, which the thread polls.
//!
//! Joining a network, as Linux's iwlwifi does with this firmware:
//! 1. Tell the firmware about the channel (PHY context), our station role
//!    (MAC context), tie them together (binding), and give them the radio's
//!    time (quota).
//! 2. Add the access point as a station, with transmit queues.
//! 3. Ask the firmware to stay on the channel (a time event), then
//!    authenticate and associate with ordinary 802.11 management frames.
//! 4. For WPA2, do the 4-way handshake (`wpa`), after which every data
//!    frame is encrypted with AES-CCMP, in software.
//! 5. Hand `wlan0` to the network stack, which gets an address by DHCP.
//!
//! When anything goes wrong mid-way, or to disconnect, the firmware is
//! simply restarted: it takes about a second and leaves nothing behind.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use super::commands::{self as cmd, Channel, MacContext, SmartFifo, Station, TxFrame, rate};
use super::crypto::{self, Aes128};
use super::firmware::{Firmware, Image};
use super::ieee80211::{self as wlan, Bss, Security};
use super::transport::{Error, Packet, Transport};
use super::wpa::{self, GroupKey, Handshake, Step};
use crate::errno::{Errno, Result as SysResult};
use crate::net::{self, NetDevice};
use crate::sync::SpinLock;
use crate::{random, task, time};
use tls::peap::{Peer, Reply};

/// What the init image found out, which the runtime image needs.
pub struct InitResults {
    pub mac: [u8; 6],
    /// CALIBRATION_RESULT payloads, for the PHY database.
    pub calibration: Vec<Vec<u8>>,
    /// The channels the card may use here (from its NVM).
    pub channels: Vec<Channel>,
}

/// EAPOL packet types (802.1X): EAP, and asking the access point to start.
const EAPOL_EAP: u8 = 0;
const EAPOL_START: u8 = 1;

/// A certificate for the log and /proc/wifi: subject, issuer and SHA-256
/// fingerprint, so the owner can compare it with what the network's IT
/// department publishes.
fn describe_certificate(der: &[u8]) -> String {
    let fingerprint = apk::hash::hex(&apk::hash::sha256(der));
    match tls::x509::Certificate::parse(der) {
        Some(certificate) => {
            alloc::format!("{} (issued by {}, SHA-256 {fingerprint})", certificate.subject, certificate.issuer)
        }
        None => alloc::format!("unreadable (SHA-256 {fingerprint})"),
    }
}

/// A request from /proc/wifi.
enum Request {
    Scan,
    /// With an identity (user name) for enterprise networks.
    Connect {
        ssid: Vec<u8>,
        password: Vec<u8>,
        identity: Vec<u8>,
    },
    Disconnect,
}

/// What the rest of the kernel sees of the card.
struct Shared {
    present: bool,
    name: &'static str,
    mac: [u8; 6],
    state: String,
    /// Why the last request failed, until the next one.
    error: Option<String>,
    networks: Vec<Bss>,
    connected: Option<Bss>,
    /// The RADIUS server's certificate on an enterprise network.
    certificate: Option<String>,
    requests: VecDeque<Request>,
    /// Ethernet frames from the network stack.
    outgoing: VecDeque<Vec<u8>>,
    sent: u64,
    received: u64,
    failed: u64,
}

static SHARED: SpinLock<Shared> = SpinLock::new(Shared {
    present: false,
    name: "",
    mac: [0; 6],
    state: String::new(),
    error: None,
    networks: Vec::new(),
    connected: None,
    certificate: None,
    requests: VecDeque::new(),
    outgoing: VecDeque::new(),
    sent: 0,
    received: 0,
    failed: 0,
});

fn set_state(state: &str) {
    info!("wifi: {state}");
    SHARED.lock().state = String::from(state);
}

fn fail(error: String) {
    warn!("wifi: {error}");
    SHARED.lock().error = Some(error);
}

/// Whether there is a Wi-Fi card (so /proc/wifi exists).
pub fn present() -> bool {
    SHARED.lock().present
}

pub fn announce(name: &'static str) {
    let mut shared = SHARED.lock();
    shared.present = true;
    shared.name = name;
    shared.state = String::from("starting");
}

/// /proc/wifi's text.
pub fn status_text() -> String {
    let shared = SHARED.lock();
    let mut out = String::new();
    let _ = writeln!(out, "card: {}", shared.name);
    let _ = writeln!(out, "address: {}", net::format_mac(&shared.mac));
    let _ = writeln!(out, "state: {}", shared.state);
    if let Some(error) = &shared.error {
        let _ = writeln!(out, "error: {error}");
    }
    if let Some(bss) = &shared.connected {
        let _ = writeln!(out, "network: {}", bss.ssid_text());
        let _ = writeln!(
            out,
            "access point: {} channel {} {} dBm {}{}",
            net::format_mac(&bss.bssid),
            bss.channel,
            bss.signal,
            bss.security.name(),
            match bss.ht {
                Some(ht) if ht.two_streams => " 802.11n 2x2",
                Some(_) => " 802.11n",
                None => "",
            }
        );
        let _ = writeln!(out, "frames: {} sent, {} received, {} failed", shared.sent, shared.received, shared.failed);
    }
    if let Some(certificate) = &shared.certificate {
        let _ = writeln!(out, "server certificate: {certificate}");
    }
    let _ = writeln!(out, "networks:");
    for bss in &shared.networks {
        let _ = writeln!(
            out,
            "{:>4} dBm  channel {:>3}  {:<8}  {}",
            bss.signal,
            bss.channel,
            bss.security.name(),
            bss.ssid_text()
        );
    }
    out
}

/// A write to /proc/wifi: "scan", "connect\nSSID\nPASSWORD",
/// "connect\nSSID\nPASSWORD\nUSER" (enterprise networks) or "disconnect".
pub fn control(bytes: &[u8]) -> SysResult<()> {
    let mut lines = bytes.split(|&byte| byte == b'\n');
    let request = match lines.next().unwrap_or_default().trim_ascii() {
        b"scan" => Request::Scan,
        b"disconnect" => Request::Disconnect,
        b"connect" => {
            let ssid = lines.next().unwrap_or_default().to_vec();
            let password = lines.next().unwrap_or_default().to_vec();
            let identity = lines.next().unwrap_or_default().to_vec();
            let password_ok = if identity.is_empty() {
                password.is_empty() || (8..=63).contains(&password.len())
            } else {
                !password.is_empty() && password.len() <= 256 && identity.len() <= 256
            };
            if ssid.is_empty() || ssid.len() > 32 || !password_ok {
                return Err(Errno::EINVAL);
            }
            Request::Connect { ssid, password, identity }
        }
        _ => return Err(Errno::EINVAL),
    };
    let mut shared = SHARED.lock();
    if !shared.present {
        return Err(Errno::ENODEV);
    }
    shared.error = None;
    // So the `wifi` command, waiting for "idle" or "connected", doesn't see
    // the state from before its request.
    shared.state = String::from("working");
    shared.requests.push_back(request);
    Ok(())
}

/// The network stack's view of the card.
struct Wlan {
    mac: [u8; 6],
}

impl NetDevice for Wlan {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&self) -> bool {
        SHARED.lock().connected.is_some()
    }

    fn transmit(&self, frame: &[u8]) {
        let mut shared = SHARED.lock();
        if shared.connected.is_some() && shared.outgoing.len() < 256 {
            shared.outgoing.push_back(frame.to_vec());
        }
    }
}

// Firmware commands this module uses, besides those in `commands`.
const SCAN_ITERATION_COMPLETE: u8 = 0xE7;
const TIME_EVENT_START: u32 = 1 << 0;
/// Missed beacons in a row before the network counts as gone.
const MISSED_BEACONS_LIMIT: u32 = 20;
/// How long to stay on the channel while joining (time units, ~1 ms).
const JOIN_TIME: u32 = 1000;

static INTERFACE_ADDED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// What the card said about a received frame.
#[derive(Clone, Copy, Default)]
struct RxInfo {
    channel: u8,
    signal: i8,
    /// The access point's clock (TSF) and the card's, when the frame
    /// arrived.
    tsf: u64,
    device_time: u32,
}

enum Received {
    Frame(Vec<u8>, RxInfo),
    Other(Packet),
}

/// Encryption state for a WPA2 link.
struct Keys {
    pairwise: Aes128,
    group: [Option<Aes128>; 4],
    /// Our next packet number.
    next_pn: u64,
    /// The last packet number accepted per traffic class (16 for group
    /// traffic), against replays.
    received_pn: [u64; 17],
}

/// Signing in to an enterprise network, before the 4-way handshake.
struct Enterprise {
    peer: Peer,
    /// Whether the access point has started EAP (else we ask it to).
    started: bool,
    /// Whether the server's certificate has been logged.
    certificate_logged: bool,
    /// Why signing in failed.
    failure: Option<String>,
}

/// The network we're joined to.
struct Link {
    bss: Bss,
    qos: bool,
    association_id: u16,
    /// Created once the pairwise master key is known: at once from a
    /// passphrase, after EAP on enterprise networks.
    handshake: Option<Handshake>,
    enterprise: Option<Enterprise>,
    keys: Option<Keys>,
    sequence: u16,
    /// Whether the firmware knows the DTIM timing yet (from a beacon).
    dtim_known: bool,
    /// Rates, best first, and where in that list the firmware starts.
    rates: Vec<u32>,
    first_rate: usize,
    successes: u32,
    last_rate_change: u64,
}

struct Radio<'a> {
    card: &'a mut Transport,
    firmware: &'a Firmware,
    init: InitResults,
    phy: RxInfo,
    deferred: VecDeque<Packet>,
    link: Option<Link>,
    networks: Vec<Bss>,
}

/// Brings up the runtime firmware and serves requests until the card
/// fails.
pub fn run(card: &mut Transport, firmware: &Firmware, init: InitResults) -> Result<(), Error> {
    SHARED.lock().mac = init.mac;
    let mut radio = Radio {
        card,
        firmware,
        init,
        phy: RxInfo::default(),
        deferred: VecDeque::new(),
        link: None,
        networks: Vec::new(),
    };
    radio.start()?;
    // Once: after a firmware error the card starts over, with the same
    // interface.
    if !INTERFACE_ADDED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        net::add_interface("wlan0", Arc::new(Wlan { mac: radio.init.mac }));
    }
    set_state("idle");
    radio.serve()
}

impl Radio<'_> {
    /// Loads and configures the runtime image (after the init image, or to
    /// start over).
    fn start(&mut self) -> Result<(), Error> {
        self.link = None;
        self.deferred.clear();
        self.card.stop();
        self.card.prepare()?;
        self.card.power_up()?;
        self.card.initialize(self.firmware.phy_config)?;
        self.card.load_firmware(self.firmware.sections(Image::Runtime))?;
        let alive = self.card.wait_for(1000, |p| p.command == super::ALIVE && p.group == 0, super::log_unexpected)?;
        let word =
            |offset: usize| alive.data.get(offset..offset + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        let status = word(0) & 0xFFFF;
        debug!("wifi: runtime firmware alive: status {status:#x}, error log at {:#x}", word(20));
        if status != 0xCAFE {
            return Err(Error::Firmware("the runtime firmware didn't start properly"));
        }
        super::ERROR_LOG.store(word(20), core::sync::atomic::Ordering::Relaxed);
        self.card.start_scheduler();

        self.command(cmd::SMART_FIFO, &cmd::smart_fifo(SmartFifo::InitOff))?;
        let antennas = u32::from(self.firmware.valid_tx_antennas());
        self.command(super::TX_ANTENNA_CONFIGURATION, &antennas.to_le_bytes())?;
        self.send_phy_database()?;
        let (flow, event) = self.firmware.calibration[Image::Runtime as usize];
        let mut phy = cmd::Writer::default();
        phy.u32(self.firmware.phy_config).u32(flow).u32(event);
        self.command(super::PHY_CONFIGURATION, &phy.0)?;
        let mut bt = cmd::Writer::default();
        bt.u32(1).u32(1 << 2 | 1 << 4);
        self.command(super::BT_CONFIG, &bt.0)?;
        // "Dynamic queue allocation": the driver picks queues per station.
        self.group_command(cmd::DQA_ENABLE, cmd::DATA_PATH_GROUP, &0u32.to_le_bytes())?;
        // The aux station and its queue, which scans send probes from.
        let ssn = self.card.enable_queue(cmd::AUX_QUEUE, 0);
        let config = cmd::queue_config(cmd::AUX_QUEUE, cmd::AUX_STATION, cmd::MANAGEMENT_TID, cmd::FIFO_MULTICAST, ssn);
        self.command(cmd::QUEUE_CONFIG, &config)?;
        let response = self.command(cmd::ADD_STATION, &cmd::aux_station())?;
        debug!("wifi: aux station: status {:#x}", status_word(&response));
        self.command(cmd::THERMAL_BACKOFF, &0u32.to_le_bytes())?;
        // Power saving off ("continuously aware mode").
        self.command(cmd::DEVICE_POWER, &[0, 0, 0, 0])?;
        info!("wifi: runtime firmware ready");
        Ok(())
    }

    /// Sends the init image's calibration results back to the runtime
    /// image, in the order Linux does: configuration, non-channel
    /// calibration, then each channel group's power amplifier and transmit
    /// power results.
    fn send_phy_database(&mut self) -> Result<(), Error> {
        let mut entries: Vec<(u16, u16, Vec<u8>)> = Vec::new();
        for result in &self.init.calibration {
            if result.len() < 4 {
                continue;
            }
            let kind = u16::from_le_bytes([result[0], result[1]]);
            let len = usize::from(u16::from_le_bytes([result[2], result[3]]));
            let Some(data) = result.get(..4 + len) else { continue };
            let group = if kind == 4 || kind == 5 { u16::from_le_bytes([data[4], data[5]]) } else { 0 };
            entries.retain(|(k, g, _)| (*k, *g) != (kind, group));
            entries.push((kind, group, data.to_vec()));
        }
        entries.retain(|(kind, _, _)| matches!(kind, 1 | 2 | 4 | 5));
        entries.sort_by_key(|(kind, group, _)| (*kind, *group));
        debug!(
            "wifi: PHY database: {} entries ({})",
            entries.len(),
            entries
                .iter()
                .map(|(kind, group, data)| alloc::format!("{kind}/{group}:{}", data.len()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (_, _, data) in entries {
            self.command(cmd::PHY_DB, &data)?;
        }
        Ok(())
    }

    /// Sends a command and waits for its response; packets that arrive
    /// meanwhile are kept for later.
    fn command(&mut self, command: u8, payload: &[u8]) -> Result<Packet, Error> {
        self.group_command(command, 0, payload)
    }

    fn group_command(&mut self, command: u8, group: u8, payload: &[u8]) -> Result<Packet, Error> {
        let sequence = self.card.send_command(command, group, 0, payload)?;
        let mut others = Vec::new();
        let result = self.card.wait_for(
            1000,
            |packet| packet.sequence == sequence && !packet.is_notification(),
            |packet| {
                others.push(Packet {
                    command: packet.command,
                    group: packet.group,
                    sequence: packet.sequence,
                    data: packet.data.clone(),
                })
            },
        );
        self.deferred.extend(others);
        if result.is_err() {
            warn!("wifi: no response to command {command:#04x} (group {group})");
        }
        result
    }

    /// The next thing from the card: a received 802.11 frame or another
    /// packet (None if nothing is waiting). Frames' radio details arrive in
    /// a separate packet just before them.
    fn receive(&mut self) -> Result<Option<Received>, Error> {
        loop {
            let packet = match self.deferred.pop_front() {
                Some(packet) => packet,
                None => {
                    if let Some(packet) = self.card.next_packet() {
                        packet
                    } else {
                        self.card.poll_events()?;
                        match self.card.next_packet() {
                            Some(packet) => packet,
                            None => return Ok(None),
                        }
                    }
                }
            };
            if packet.group != 0 {
                return Ok(Some(Received::Other(packet)));
            }
            match packet.command {
                cmd::RX_PHY => self.phy = parse_phy(&packet.data),
                cmd::RX_MPDU => {
                    if let Some(frame) = parse_mpdu(&packet.data) {
                        return Ok(Some(Received::Frame(frame, self.phy)));
                    }
                }
                _ => return Ok(Some(Received::Other(packet))),
            }
        }
    }

    /// The thread's loop: requests, received frames, outgoing traffic.
    fn serve(&mut self) -> Result<(), Error> {
        loop {
            let request = SHARED.lock().requests.pop_front();
            match request {
                Some(Request::Scan) => {
                    if self.link.is_some() {
                        fail(String::from("scanning while connected isn't supported yet; disconnect first"));
                    } else {
                        self.scan(&[])?;
                        set_state("idle");
                    }
                }
                Some(Request::Connect { ssid, password, identity }) => {
                    if self.link.is_some() {
                        self.disconnect("connecting elsewhere")?;
                    }
                    if let Err(error) = self.connect(&ssid, &password, &identity) {
                        let message = match error {
                            JoinError::Card(error) => return Err(error),
                            JoinError::Failed(message) => message,
                        };
                        fail(message);
                        self.start()?;
                        set_state("idle");
                    }
                }
                Some(Request::Disconnect) if self.link.is_some() => self.disconnect("asked to")?,
                Some(Request::Disconnect) | None => {}
            }

            let mut busy = false;
            while let Some(received) = self.receive()? {
                busy = true;
                match received {
                    Received::Frame(frame, info) => self.handle_frame(frame, info)?,
                    Received::Other(packet) => self.handle_packet(&packet)?,
                }
            }
            if self.link.is_some() {
                loop {
                    let frame = SHARED.lock().outgoing.pop_front();
                    let Some(frame) = frame else { break };
                    busy = true;
                    self.send_ethernet(&frame)?;
                }
            }
            if !busy {
                task::sleep_ms(if self.link.is_some() { 1 } else { 10 });
            }
        }
    }

    /// Notifications outside a request.
    fn handle_packet(&mut self, packet: &Packet) -> Result<(), Error> {
        match packet.command {
            cmd::TX => self.transmitted(packet)?,
            cmd::MISSED_BEACONS => {
                let word = |index: usize| read_u32(&packet.data, index * 4);
                let (since_rx, total) = (word(1), word(2));
                debug!("wifi: missed beacons: {since_rx} since the last frame, {total} in a row");
                if self.link.is_some() && since_rx >= MISSED_BEACONS_LIMIT {
                    self.disconnect("the access point stopped answering")?;
                }
            }
            cmd::TIME_EVENT_NOTIFICATION | SCAN_ITERATION_COMPLETE | cmd::SCAN_COMPLETE => {}
            _ => super::log_unexpected(packet),
        }
        Ok(())
    }

    /// A TX response: frees the slot, and adjusts the rates.
    fn transmitted(&mut self, packet: &Packet) -> Result<(), Error> {
        let queue = u32::from(packet.sequence >> 8 & 0x1F);
        let slot = usize::from(packet.sequence & 0xFF);
        self.card.reclaim(queue, slot);
        let data = &packet.data;
        if data.len() < 38 {
            return Ok(());
        }
        let retries = data[3];
        let status = u16::from_le_bytes([data[36], data[37]]) & 0xFF;
        let success = status == 1 || status == 2;
        if !success {
            SHARED.lock().failed += 1;
            debug!(
                "wifi: frame on queue {queue} not sent: status {status:#x}, {retries} retries, rate {:#x}",
                read_u32(data, 4)
            );
        }
        if queue == cmd::DATA_QUEUE {
            self.adapt_rate(success && retries == 0, !success || retries >= 2)?;
        }
        Ok(())
    }

    /// Simple rate control: start lower after failures, try higher after a
    /// run of first-time successes. The firmware falls back down the list
    /// on its own for each frame.
    fn adapt_rate(&mut self, clean: bool, struggled: bool) -> Result<(), Error> {
        let Some(link) = &mut self.link else { return Ok(()) };
        let now = time::uptime_nanos();
        let mut changed = false;
        if struggled {
            link.successes = 0;
            if link.first_rate + 1 < link.rates.len() && now - link.last_rate_change > 50_000_000 {
                link.first_rate += 1;
                changed = true;
            }
        } else if clean {
            link.successes += 1;
            if link.successes >= 32 && link.first_rate > 0 && now - link.last_rate_change > 500_000_000 {
                link.first_rate -= 1;
                link.successes = 0;
                changed = true;
            }
        }
        if changed {
            link.last_rate_change = now;
            let rates = link.rates[link.first_rate..].to_vec();
            let mimo = link.bss.ht.is_some_and(|ht| ht.two_streams);
            debug!("wifi: starting rate now {:#x}", rates[0]);
            self.command(cmd::LINK_QUALITY, &cmd::link_quality(cmd::AP_STATION, &rates, mimo))?;
        }
        Ok(())
    }

    /// Scans every channel (or looks for one network), keeping what it
    /// finds in `self.networks`.
    fn scan(&mut self, ssid: &[u8]) -> Result<(), Error> {
        set_state("scanning");
        let max = self.firmware.scan_channels as usize;
        let channels: Vec<Channel> = self.init.channels.iter().copied().take(max).collect();
        let request = cmd::scan_request(&channels, max, &self.init.mac, ssid);
        let response = self.command(cmd::SCAN_REQUEST, &request)?;
        let status = status_word(&response);
        if status != 0 {
            fail(alloc::format!("the firmware refused to scan (status {status:#x})"));
            return Ok(());
        }
        let mut found: Vec<Bss> = Vec::new();
        let deadline = time::uptime_nanos() + 15_000_000_000;
        loop {
            match self.receive()? {
                Some(Received::Frame(frame, info)) => {
                    let kind = frame[0] & 0xFC;
                    if (kind == wlan::BEACON || kind == wlan::PROBE_RESPONSE)
                        && let Some(bss) = wlan::parse_bss(&frame, info.channel, info.signal)
                    {
                        match found.iter_mut().find(|known| known.bssid == bss.bssid) {
                            Some(known) => known.signal = known.signal.max(bss.signal),
                            None => found.push(bss),
                        }
                    }
                }
                Some(Received::Other(packet)) if packet.command == cmd::SCAN_COMPLETE && packet.group == 0 => {
                    debug!("wifi: scan complete: status {}", packet.data.get(2).copied().unwrap_or(0));
                    break;
                }
                Some(Received::Other(packet)) => self.handle_packet(&packet)?,
                None => {
                    if time::uptime_nanos() > deadline {
                        warn!("wifi: the scan didn't finish; aborting it");
                        let _ = self.command(cmd::SCAN_ABORT, &[]);
                        break;
                    }
                    task::sleep_ms(2);
                }
            }
        }
        found.sort_by_key(|bss| core::cmp::Reverse(bss.signal));
        info!("wifi: scan found {} access points", found.len());
        for bss in &found {
            info!(
                "wifi:   {} {:>4} dBm channel {:>3} {} {:?}{}",
                net::format_mac(&bss.bssid),
                bss.signal,
                bss.channel,
                bss.security.name(),
                bss.ssid_text(),
                if bss.ht.is_some() { " (HT)" } else { "" }
            );
        }
        self.networks = found.clone();
        SHARED.lock().networks = found;
        Ok(())
    }

    /// Joins a network. On failure the caller restarts the firmware.
    fn connect(&mut self, ssid: &[u8], password: &[u8], identity: &[u8]) -> Result<(), JoinError> {
        let name = String::from_utf8_lossy(ssid).into_owned();
        // A fresh scan for this network finds hidden ones too, and the
        // current signal strengths.
        self.scan(ssid)?;
        let bss = self
            .networks
            .iter()
            .filter(|bss| bss.ssid == ssid)
            .max_by_key(|bss| bss.signal)
            .cloned()
            .ok_or_else(|| JoinError::Failed(alloc::format!("network {name:?} not found")))?;
        match bss.security {
            Security::Open if !password.is_empty() => {
                return Err(JoinError::Failed(alloc::format!("{name:?} is an open network; no password needed")));
            }
            Security::Wpa2Personal if password.is_empty() => {
                return Err(JoinError::Failed(alloc::format!("{name:?} needs a password")));
            }
            Security::Wpa2Personal if !identity.is_empty() => {
                return Err(JoinError::Failed(alloc::format!(
                    "{name:?} uses a shared password (WPA2-Personal); no user name needed"
                )));
            }
            Security::Wpa2Enterprise if identity.is_empty() || password.is_empty() => {
                return Err(JoinError::Failed(alloc::format!(
                    "{name:?} is an enterprise network: it needs a user name and password (wifi connect NAME --user USER)"
                )));
            }
            Security::Unsupported(what) => {
                return Err(JoinError::Failed(alloc::format!("{name:?} uses {what}, which eliOS can't join yet")));
            }
            _ => {}
        }
        set_state(&alloc::format!("connecting to {name:?}"));
        info!(
            "wifi: joining {} on channel {} ({} dBm, {}, beacon interval {}, DTIM period {}, rates {:x?}, HT {:?}, WMM {})",
            net::format_mac(&bss.bssid),
            bss.channel,
            bss.signal,
            bss.security.name(),
            bss.beacon_interval,
            bss.dtim_period,
            bss.rates,
            bss.ht,
            bss.wmm.is_some()
        );
        let pmk = (bss.security == Security::Wpa2Personal).then(|| crypto::passphrase_key(password, ssid));
        let enterprise = (bss.security == Security::Wpa2Enterprise).then(|| Enterprise {
            peer: Peer::new(&String::from_utf8_lossy(identity), &String::from_utf8_lossy(password), random::fill),
            started: false,
            certificate_logged: false,
            failure: None,
        });
        SHARED.lock().certificate = None;
        let mac = self.init.mac;
        let channel = Channel { number: bss.channel };

        // 1. Channel, MAC context, binding, quota.
        self.checked(cmd::PHY_CONTEXT, &cmd::phy_context(cmd::ADD, channel), "PHY context")?;
        let context = self.mac_context(cmd::ADD, &bss, 0, None);
        self.checked(cmd::MAC_CONTEXT, &context, "MAC context")?;
        self.checked(cmd::BINDING, &cmd::binding(cmd::ADD), "binding")?;
        self.checked(cmd::TIME_QUOTA, &cmd::time_quota(), "time quota")?;

        // 2. The access point as a station, with a data queue and a
        // management queue.
        for (queue, tid, fifo) in
            [(cmd::DATA_QUEUE, 0, cmd::FIFO_BEST_EFFORT), (cmd::MANAGEMENT_QUEUE, cmd::MANAGEMENT_TID, cmd::FIFO_VOICE)]
        {
            let ssn = self.card.enable_queue(queue, 0);
            let response =
                self.command(cmd::QUEUE_CONFIG, &cmd::queue_config(queue, cmd::AP_STATION, tid, fifo, ssn))?;
            // The answer isn't a status word: it echoes the token, station,
            // TID and queue (SCD_QUEUE_CFG_RSP), as the 7260 showed.
            let echo = response.data.get(..4).unwrap_or_default();
            debug!("wifi: queue {queue}: firmware answered {echo:02x?}");
            if echo.get(1) != Some(&cmd::AP_STATION) || echo.get(3) != Some(&(queue as u8)) {
                return Err(JoinError::Failed(alloc::format!(
                    "the firmware didn't set up queue {queue} ({echo:02x?})"
                )));
            }
        }
        let queues = 1 << cmd::DATA_QUEUE | 1 << cmd::MANAGEMENT_QUEUE;
        let station = Station {
            id: cmd::AP_STATION,
            mac_id: cmd::CONTEXT_ID,
            address: &bss.bssid,
            modify: false,
            queues,
            flags: 0,
            ht: false,
            association_id: 0,
        };
        let response = self.command(cmd::ADD_STATION, &cmd::add_station(&station))?;
        if status_word(&response) != 1 {
            return Err(JoinError::Failed(alloc::format!(
                "the firmware refused the station (status {:#x})",
                status_word(&response)
            )));
        }
        let basic = self.basic_rate(&bss);
        // Linux never reads this command's answer; log it rather than
        // guess what it means.
        let response = self.command(cmd::LINK_QUALITY, &cmd::link_quality(cmd::AP_STATION, &[basic], false))?;
        debug!("wifi: link quality: firmware answered {:02x?}", response.data.get(..8).unwrap_or_default());

        // 3. Stay on the channel, then authenticate and associate.
        let response = self.command(cmd::TIME_EVENT, &cmd::time_event(cmd::ADD, JOIN_TIME, 0))?;
        let event = read_u32(&response.data, 8);
        debug!("wifi: time event {event:#x}: status {:#x}", status_word(&response));
        self.wait_for_time_event()?;

        self.link = Some(Link {
            bss: bss.clone(),
            qos: bss.wmm.is_some(),
            association_id: 0,
            handshake: pmk.map(|pmk| Handshake::new(pmk, mac, bss.bssid, bss.rsn.clone(), wlan::our_rsn(bss.security))),
            enterprise,
            keys: None,
            sequence: 0,
            dtim_known: false,
            rates: alloc::vec![basic],
            first_rate: 0,
            successes: 0,
            last_rate_change: 0,
        });
        let authentication = wlan::authentication(&bss.bssid, &mac);
        let response = self.exchange(&authentication, &bss, wlan::AUTHENTICATION, "authentication")?;
        let (status, _) = wlan::response_status(&response).unwrap_or((0xFFFF, 0));
        if status != 0 {
            return Err(JoinError::Failed(alloc::format!("authentication refused: {}", status_code_text(status))));
        }
        info!("wifi: authenticated");
        let request = wlan::association_request(&bss, &mac, cmd::LISTEN_INTERVAL);
        let response = self.exchange(&request, &bss, wlan::ASSOCIATION_RESPONSE, "association")?;
        let (status, association_id) = wlan::response_status(&response).unwrap_or((0xFFFF, 0));
        if status != 0 {
            return Err(JoinError::Failed(alloc::format!("association refused: {}", status_code_text(status))));
        }
        info!("wifi: associated (association id {association_id})");
        self.associated(&bss, association_id)?;

        // 4. Enterprise sign-in (EAP), then the WPA2 handshake.
        if bss.security != Security::Open {
            self.handshake()?;
        }
        let _ = self.command(cmd::TIME_EVENT, &cmd::time_event(cmd::REMOVE, 0, event));

        // 5. Ready.
        {
            let mut shared = SHARED.lock();
            shared.connected = Some(bss.clone());
            shared.outgoing.clear();
            shared.sent = 0;
            shared.received = 0;
            shared.failed = 0;
        }
        set_state("connected");
        info!("wifi: connected to {name:?}");
        net::activate("wlan0");
        Ok(())
    }

    /// Sends a command whose response is a status word (0 for success).
    fn checked(&mut self, command: u8, payload: &[u8], what: &str) -> Result<(), JoinError> {
        let response = self.command(command, payload)?;
        let status = status_word(&response);
        debug!("wifi: {what}: status {status:#x}");
        if status != 0 {
            return Err(JoinError::Failed(alloc::format!("the firmware refused the {what} (status {status:#x})")));
        }
        Ok(())
    }

    /// Waits for the firmware to say the time event started (it's on the
    /// channel).
    fn wait_for_time_event(&mut self) -> Result<(), Error> {
        let deadline = time::uptime_nanos() + 1_000_000_000;
        while time::uptime_nanos() < deadline {
            match self.receive()? {
                Some(Received::Other(packet)) if packet.command == cmd::TIME_EVENT_NOTIFICATION => {
                    let (action, status) = (read_u32(&packet.data, 16), read_u32(&packet.data, 20));
                    debug!("wifi: time event notification: action {action:#x}, status {status}");
                    if action & TIME_EVENT_START != 0 {
                        return Ok(());
                    }
                }
                Some(Received::Other(packet)) => self.handle_packet(&packet)?,
                Some(Received::Frame(..)) => {}
                None => task::sleep_ms(1),
            }
        }
        debug!("wifi: the time event didn't start; trying anyway");
        Ok(())
    }

    /// Sends a management frame and waits for the access point's answer
    /// (of type `answer`), trying three times.
    fn exchange(&mut self, frame: &[u8], bss: &Bss, answer: u8, what: &str) -> Result<Vec<u8>, JoinError> {
        for attempt in 1..=3 {
            self.send_management(frame, bss)?;
            let deadline = time::uptime_nanos() + 300_000_000;
            while time::uptime_nanos() < deadline {
                match self.receive()? {
                    Some(Received::Frame(reply, _)) => {
                        if reply[0] & 0xFC == answer && reply.get(10..16) == Some(&bss.bssid[..]) {
                            return Ok(reply);
                        }
                        if matches!(reply[0] & 0xFC, wlan::DEAUTHENTICATION | wlan::DISASSOCIATION)
                            && reply.get(10..16) == Some(&bss.bssid[..])
                        {
                            let reason = u16::from_le_bytes([reply[24], reply.get(25).copied().unwrap_or(0)]);
                            return Err(JoinError::Failed(alloc::format!(
                                "the access point refused us during {what} (reason {reason})"
                            )));
                        }
                    }
                    Some(Received::Other(packet)) => self.handle_packet(&packet)?,
                    None => task::sleep_ms(1),
                }
            }
            debug!("wifi: no {what} response (attempt {attempt})");
        }
        Err(JoinError::Failed(alloc::format!("the access point didn't answer the {what}")))
    }

    /// After association: the station's capabilities, its rates, and the
    /// MAC context's association id.
    fn associated(&mut self, bss: &Bss, association_id: u16) -> Result<(), JoinError> {
        let ht = bss.ht.filter(|_| bss.wmm.is_some());
        let two_streams = ht.is_some_and(|ht| ht.two_streams);
        let station = Station {
            id: cmd::AP_STATION,
            mac_id: cmd::CONTEXT_ID,
            address: &bss.bssid,
            modify: true,
            queues: 1 << cmd::DATA_QUEUE | 1 << cmd::MANAGEMENT_QUEUE,
            flags: if two_streams { cmd::STATION_MIMO2 } else { 0 },
            ht: ht.is_some(),
            association_id,
        };
        let response = self.command(cmd::ADD_STATION, &cmd::add_station(&station))?;
        debug!("wifi: station updated: status {:#x}", status_word(&response));
        let rates = rate_ladder(bss, ht.is_some(), two_streams);
        debug!("wifi: rates {:x?}", rates);
        self.command(cmd::LINK_QUALITY, &cmd::link_quality(cmd::AP_STATION, &rates, two_streams))?;
        let context = self.mac_context(cmd::MODIFY, bss, association_id, None);
        self.checked(cmd::MAC_CONTEXT, &context, "MAC context")?;
        let streams = if two_streams { 2 } else { 1 };
        self.command(cmd::SMART_FIFO, &cmd::smart_fifo(SmartFifo::FullOn { streams }))?;
        if let Some(link) = &mut self.link {
            link.association_id = association_id;
            link.rates = rates;
        }
        Ok(())
    }

    /// Runs the 4-way handshake: messages 1 and 3 from the access point,
    /// our 2 and 4. On enterprise networks, EAP comes first: the access
    /// point relays it to a RADIUS server, which can take seconds.
    fn handshake(&mut self) -> Result<(), JoinError> {
        let start = time::uptime_nanos();
        let enterprise = self.link.as_ref().is_some_and(|link| link.enterprise.is_some());
        let deadline = start + if enterprise { 30_000_000_000 } else { 5_000_000_000 };
        let mut asked = false;
        while time::uptime_nanos() < deadline {
            if let Some(failure) =
                self.link.as_mut().and_then(|link| link.enterprise.as_mut()).and_then(|eap| eap.failure.take())
            {
                return Err(JoinError::Failed(failure));
            }
            let waiting = self.link.as_ref().and_then(|link| link.enterprise.as_ref()).is_some_and(|eap| !eap.started);
            if waiting && !asked && time::uptime_nanos() > start + 1_000_000_000 {
                // Access points normally start EAP on their own; some
                // wait for an EAPOL-Start.
                info!("wifi: no EAP request yet; sending EAPOL-Start");
                let bssid = self.link.as_ref().map_or([0; 6], |link| link.bss.bssid);
                self.send_data(&bssid, wlan::ETHERTYPE_EAPOL, &[2, EAPOL_START, 0, 0])?;
                asked = true;
            }
            match self.receive()? {
                Some(Received::Frame(frame, info)) => {
                    self.handle_frame(frame, info)?;
                    if self.link.as_ref().is_some_and(|link| link.keys.is_some()) {
                        info!("wifi: WPA2 handshake done");
                        return Ok(());
                    }
                    if self.link.is_none() {
                        return Err(JoinError::Failed(String::from("the access point disconnected us")));
                    }
                }
                Some(Received::Other(packet)) => self.handle_packet(&packet)?,
                None => task::sleep_ms(1),
            }
        }
        let signed_in = self.link.as_ref().is_some_and(|link| link.handshake.is_some());
        Err(JoinError::Failed(String::from(match (enterprise, signed_in) {
            (true, false) => "signing in didn't finish (the RADIUS server didn't answer)",
            (true, true) => "the WPA2 handshake after signing in didn't finish",
            _ => "the WPA2 handshake didn't finish (wrong password?)",
        })))
    }

    fn mac_context(&self, action: u32, bss: &Bss, association_id: u16, dtim: Option<cmd::Dtim>) -> Vec<u8> {
        let two_ghz = bss.channel <= 14;
        let (cck_rates, ofdm_rates) = acknowledgement_rates(&bss.rates, two_ghz);
        let mut protection = 0;
        if bss.ht.is_some_and(|ht| ht.protection) {
            protection |= 1 << 23 | 1 << 24;
        }
        if bss.use_protection {
            protection |= 1 << 3;
        }
        cmd::mac_context(&MacContext {
            action,
            address: &self.init.mac,
            bssid: &bss.bssid,
            cck_rates,
            ofdm_rates,
            protection,
            short_preamble: bss.short_preamble(),
            short_slot: bss.short_slot() || !two_ghz,
            ht: bss.ht.is_some() && bss.wmm.is_some(),
            edca: bss.wmm.unwrap_or(cmd::DEFAULT_EDCA),
            beacon_interval: bss.beacon_interval,
            dtim_period: bss.dtim_period,
            association_id,
            dtim,
        })
    }

    /// The lowest basic rate: what management frames go at.
    fn basic_rate(&self, bss: &Bss) -> u32 {
        if bss.channel <= 14 { rate::CCK_1M } else { rate::OFDM_6M }
    }

    /// Sends a management frame to the access point.
    fn send_management(&mut self, frame: &[u8], bss: &Bss) -> Result<(), Error> {
        let association_request = frame[0] & 0xFC == wlan::ASSOCIATION_REQUEST;
        let parameters = TxFrame {
            len: 0,
            flags: cmd::TX_ACK | cmd::TX_FIRMWARE_SEQUENCE | cmd::TX_NO_BLUETOOTH,
            rate: self.basic_rate(bss),
            station: cmd::AP_STATION,
            tid: cmd::MANAGEMENT_TID,
            management: true,
            association_request,
            port_control: false,
            scratch: 0,
        };
        match self.card.transmit(cmd::MANAGEMENT_QUEUE, parameters, &frame[..24], &frame[24..]) {
            Err(Error::QueueFull) => {
                debug!("wifi: management queue full");
                Ok(())
            }
            other => other.map(|_| ()),
        }
    }

    /// Sends a packet (Ethernet type and payload) to `destination` through
    /// the access point, encrypted once the keys are known.
    fn send_data(&mut self, destination: &[u8; 6], ethertype: u16, payload: &[u8]) -> Result<(), Error> {
        let mac = self.init.mac;
        let Some(link) = &mut self.link else { return Ok(()) };
        let sequence = link.sequence;
        link.sequence = (link.sequence + 1) & 0xFFF;
        let mut header = wlan::data_header(&link.bss.bssid, &mac, destination, link.qos, sequence);
        let mut body = Vec::with_capacity(payload.len() + 8);
        body.extend_from_slice(&wlan::llc_snap(ethertype));
        body.extend_from_slice(payload);
        let eapol = ethertype == wlan::ETHERTYPE_EAPOL;
        let body = match &mut link.keys {
            Some(keys) => {
                header[1] |= wlan::PROTECTED;
                keys.next_pn += 1;
                wpa::ccmp_encrypt(&keys.pairwise, &header, keys.next_pn, 0, &body)
            }
            None => body,
        };
        let parameters = TxFrame {
            len: 0,
            flags: cmd::TX_ACK | cmd::TX_STATION_RATE | if eapol { cmd::TX_NO_BLUETOOTH } else { 0 },
            rate: 0,
            station: cmd::AP_STATION,
            tid: if link.qos { 0 } else { cmd::MANAGEMENT_TID },
            management: false,
            association_request: false,
            port_control: eapol,
            scratch: 0,
        };
        let queue = if link.qos { cmd::DATA_QUEUE } else { cmd::MANAGEMENT_QUEUE };
        match self.card.transmit(queue, parameters, &header, &body) {
            Ok(_) => {
                SHARED.lock().sent += 1;
                Ok(())
            }
            Err(Error::QueueFull) => {
                SHARED.lock().failed += 1;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// An Ethernet frame from the network stack.
    fn send_ethernet(&mut self, frame: &[u8]) -> Result<(), Error> {
        if frame.len() < 14 {
            return Ok(());
        }
        let destination: [u8; 6] = frame[..6].try_into().unwrap();
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        if self.link.as_ref().is_some_and(|link| link.bss.security != Security::Open && link.keys.is_none()) {
            // Nothing but the handshake until the keys are known.
            return Ok(());
        }
        self.send_data(&destination, ethertype, &frame[14..])
    }

    /// A received 802.11 frame.
    fn handle_frame(&mut self, mut frame: Vec<u8>, info: RxInfo) -> Result<(), Error> {
        let Some(link) = &self.link else { return Ok(()) };
        let bssid = link.bss.bssid;
        if frame.get(10..16) != Some(&bssid[..]) {
            return Ok(());
        }
        match frame[0] & 0xFC {
            wlan::BEACON => return self.beacon(&frame, info),
            wlan::DEAUTHENTICATION | wlan::DISASSOCIATION if frame.get(4..10) == Some(&self.init.mac[..]) => {
                let reason = frame.get(24..26).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
                warn!("wifi: the access point disconnected us (reason {reason})");
                return self.disconnect("the access point disconnected us");
            }
            wlan::ACTION => {
                // Block Ack requests (category 3, action 0) are declined:
                // without aggregation everything is acknowledged one by one.
                debug!("wifi: action frame, category {}", frame.get(24).copied().unwrap_or(0));
                return Ok(());
            }
            _ => {}
        }
        let Some(data) = wlan::parse_data(&frame) else { return Ok(()) };
        let link = self.link.as_mut().expect("checked above");
        let body: Vec<u8> = if data.protected {
            let Some(keys) = &mut link.keys else { return Ok(()) };
            let Some((pn, key_index)) = wpa::ccmp_header(&frame, data.header_len) else { return Ok(()) };
            let group = data.destination[0] & 1 != 0;
            let slot = if group { 16 } else { usize::from(data.tid) };
            if pn <= keys.received_pn[slot] {
                debug!("wifi: replayed frame dropped (packet number {pn})");
                return Ok(());
            }
            let aes = if group { keys.group[usize::from(key_index)].as_ref() } else { Some(&keys.pairwise) };
            let Some(aes) = aes else { return Ok(()) };
            match wpa::ccmp_decrypt(aes, &mut frame, data.header_len, pn) {
                Some(body) => {
                    keys.received_pn[slot] = pn;
                    body.to_vec()
                }
                None => {
                    debug!("wifi: frame failed decryption");
                    return Ok(());
                }
            }
        } else {
            frame[data.header_len..].to_vec()
        };
        let encrypted_link = link.bss.security != Security::Open;
        let parts: Vec<([u8; 6], [u8; 6], &[u8])> =
            if data.amsdu { wlan::amsdu_parts(&body) } else { alloc::vec![(data.destination, data.source, &body[..])] };
        for (destination, source, payload) in parts {
            let Some((ethertype, packet)) = wlan::strip_llc(payload) else { continue };
            if ethertype == wlan::ETHERTYPE_EAPOL {
                self.eapol(packet)?;
                continue;
            }
            if encrypted_link && !data.protected {
                // Unencrypted traffic on an encrypted network is forged.
                continue;
            }
            let mut ethernet = Vec::with_capacity(packet.len() + 14);
            ethernet.extend_from_slice(&destination);
            ethernet.extend_from_slice(&source);
            ethernet.extend_from_slice(&ethertype.to_be_bytes());
            ethernet.extend_from_slice(packet);
            SHARED.lock().received += 1;
            net::receive(self.init.mac, ethernet);
        }
        Ok(())
    }

    /// An EAPOL frame: a handshake message, or EAP on enterprise networks.
    fn eapol(&mut self, packet: &[u8]) -> Result<(), Error> {
        if packet.get(1) == Some(&EAPOL_EAP) {
            return self.eap(packet);
        }
        let Some(link) = &mut self.link else { return Ok(()) };
        let Some(handshake) = &mut link.handshake else { return Ok(()) };
        let step = handshake.handle(packet);
        let bssid = link.bss.bssid;
        match step {
            Step::Reply(reply) => {
                debug!("wifi: handshake message 1; sending message 2");
                self.send_data(&bssid, wlan::ETHERTYPE_EAPOL, &reply)?;
            }
            Step::Done(reply, tk, group) => {
                debug!("wifi: handshake message 3; sending message 4 (group key {})", group.index);
                self.send_data(&bssid, wlan::ETHERTYPE_EAPOL, &reply)?;
                let link = self.link.as_mut().expect("checked above");
                let mut keys = Keys {
                    pairwise: Aes128::new(&tk),
                    group: [None, None, None, None],
                    next_pn: 0,
                    received_pn: [0; 17],
                };
                install_group_key(&mut keys, &group);
                link.keys = Some(keys);
            }
            Step::NewGroupKey(reply, group) => {
                debug!("wifi: new group key {}", group.index);
                self.send_data(&bssid, wlan::ETHERTYPE_EAPOL, &reply)?;
                if let Some(keys) = self.link.as_mut().and_then(|link| link.keys.as_mut()) {
                    install_group_key(keys, &group);
                }
            }
            Step::Ignore(why) => debug!("wifi: EAPOL frame ignored: {why}"),
        }
        Ok(())
    }

    /// An EAP packet from the access point (relaying the RADIUS server).
    fn eap(&mut self, packet: &[u8]) -> Result<(), Error> {
        let mac = self.init.mac;
        let Some(link) = &mut self.link else { return Ok(()) };
        let Some(eap) = &mut link.enterprise else {
            debug!("wifi: EAP packet on a network that doesn't use it; ignored");
            return Ok(());
        };
        let length = packet.get(2..4).map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
        let Some(body) = packet.get(4..4 + length) else { return Ok(()) };
        debug!("wifi: EAP code {} id {} length {}", body.first().unwrap_or(&0), body.get(1).unwrap_or(&0), length);
        eap.started = true;
        let reply = eap.peer.receive(body);
        for note in eap.peer.take_notes() {
            info!("wifi: {note}");
        }
        if !eap.certificate_logged
            && let Some(tls) = eap.peer.tls()
            && let Some(der) = tls.certificates().first()
        {
            eap.certificate_logged = true;
            let description = describe_certificate(der);
            info!("wifi: server certificate: {description} ({} in the chain; not verified)", tls.certificates().len());
            SHARED.lock().certificate = Some(description);
        }
        let bssid = link.bss.bssid;
        match reply {
            Reply::Send(answer) => {
                let mut frame = alloc::vec![2, EAPOL_EAP];
                frame.extend_from_slice(&(answer.len() as u16).to_be_bytes());
                frame.extend_from_slice(&answer);
                self.send_data(&bssid, wlan::ETHERTYPE_EAPOL, &frame)?;
            }
            Reply::Nothing => {}
            Reply::Success(pmk) => {
                info!("wifi: signed in; starting the WPA2 handshake");
                link.handshake =
                    Some(Handshake::new(pmk, mac, bssid, link.bss.rsn.clone(), wlan::our_rsn(link.bss.security)));
            }
            Reply::Failure(why) => {
                warn!("wifi: signing in failed: {why}");
                eap.failure = Some(why);
            }
        }
        Ok(())
    }

    /// A beacon from our access point: the first after associating tells
    /// the firmware when DTIM beacons come (so it knows when to expect
    /// broadcasts).
    fn beacon(&mut self, frame: &[u8], info: RxInfo) -> Result<(), Error> {
        let Some(link) = &self.link else { return Ok(()) };
        if link.dtim_known || link.association_id == 0 {
            return Ok(());
        }
        let Some(dtim_count) = frame.get(36..).and_then(|elements| {
            wlan::elements(elements).find(|(id, value)| *id == 5 && value.len() >= 2).map(|(_, value)| value[0])
        }) else {
            return Ok(());
        };
        let bss = link.bss.clone();
        let association_id = link.association_id;
        let offset = u32::from(dtim_count) * u32::from(bss.beacon_interval) * 1024;
        let dtim = cmd::Dtim {
            tsf: info.tsf + u64::from(offset),
            time: info.device_time.wrapping_add(offset),
            beacon_time: info.device_time,
        };
        debug!("wifi: DTIM count {dtim_count}, TSF {:#x}, device time {:#x}", info.tsf, info.device_time);
        let context = self.mac_context(cmd::MODIFY, &bss, association_id, Some(dtim));
        let response = self.command(cmd::MAC_CONTEXT, &context)?;
        debug!("wifi: MAC context with DTIM: status {:#x}", status_word(&response));
        if let Some(link) = &mut self.link {
            link.dtim_known = true;
        }
        if let Some(connected) = &mut SHARED.lock().connected {
            connected.signal = info.signal;
        }
        Ok(())
    }

    /// Leaves the network: tells the access point, then restarts the
    /// firmware.
    fn disconnect(&mut self, why: &str) -> Result<(), Error> {
        info!("wifi: disconnecting ({why})");
        if let Some(link) = &self.link {
            let bss = link.bss.clone();
            // Reason 3: "leaving".
            let frame = wlan::deauthentication(&bss.bssid, &self.init.mac, 3);
            self.send_management(&frame, &bss)?;
            task::sleep_ms(20);
        }
        {
            let mut shared = SHARED.lock();
            shared.connected = None;
            shared.outgoing.clear();
        }
        net::deactivate("wlan0");
        self.start()?;
        set_state("idle");
        Ok(())
    }
}

enum JoinError {
    /// The card failed: the thread restarts it.
    Card(Error),
    /// Joining failed; why.
    Failed(String),
}

impl From<Error> for JoinError {
    fn from(error: Error) -> Self {
        JoinError::Card(error)
    }
}

fn install_group_key(keys: &mut Keys, group: &GroupKey) {
    keys.group[usize::from(group.index & 3)] = Some(Aes128::new(&group.key));
    keys.received_pn[16] = group.rsc;
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// The status word most responses start with.
fn status_word(packet: &Packet) -> u32 {
    read_u32(&packet.data, 0)
}

/// RX_PHY: channel, signal and timestamps of the frame that follows.
fn parse_phy(data: &[u8]) -> RxInfo {
    let energy = read_u32(data, 28);
    let strongest = [energy & 0xFF, energy >> 8 & 0xFF].into_iter().filter(|&e| e != 0).min();
    RxInfo {
        channel: data.get(22).copied().unwrap_or(0),
        signal: strongest.map_or(-127, |energy| -(energy.min(127) as i8)),
        tsf: data.get(8..16).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap())),
        device_time: read_u32(data, 4),
    }
}

/// RX_MPDU: the frame, if it arrived intact.
fn parse_mpdu(data: &[u8]) -> Option<Vec<u8>> {
    let len = usize::from(u16::from_le_bytes([*data.first()?, *data.get(1)?]));
    let frame = data.get(4..4 + len)?;
    let status = read_u32(data, 4 + len);
    // CRC correct, and no overrun.
    if status & 0x3 != 0x3 || frame.len() < 24 {
        return None;
    }
    Some(frame.to_vec())
}

/// Which rates the card may use for acknowledgements and control frames
/// (Linux's `iwl_mvm_ack_rates`): the network's basic rates plus the
/// mandatory ones below them.
fn acknowledgement_rates(rates: &[u8], two_ghz: bool) -> (u32, u32) {
    const CCK: [u8; 4] = [2, 4, 11, 22];
    const OFDM: [u8; 8] = [12, 18, 24, 36, 48, 72, 96, 108];
    let (mut cck, mut ofdm) = (0u32, 0u32);
    let (mut lowest_cck, mut lowest_ofdm) = (usize::MAX, usize::MAX);
    for &rate in rates.iter().filter(|&&rate| rate & 0x80 != 0) {
        let rate = rate & 0x7F;
        if let Some(index) = CCK.iter().position(|&r| r == rate) {
            cck |= 1 << index;
            lowest_cck = lowest_cck.min(index);
        } else if let Some(index) = OFDM.iter().position(|&r| r == rate) {
            ofdm |= 1 << index;
            lowest_ofdm = lowest_ofdm.min(index);
        }
    }
    // 6, 12 and 24 Mbit/s are mandatory OFDM rates.
    ofdm |= 1;
    for index in [4, 2] {
        if index < lowest_ofdm {
            ofdm |= 1 << index;
        }
    }
    if two_ghz {
        cck |= 1;
        for index in 1..4 {
            if index < lowest_cck {
                cck |= 1 << index;
            }
        }
    } else {
        cck = 0;
    }
    (cck, ofdm)
}

/// The rates to try, fastest first: HT (802.11n) rates if both sides
/// support them, then the network's legacy rates.
fn rate_ladder(bss: &Bss, ht: bool, two_streams: bool) -> Vec<u32> {
    let mut rates = Vec::new();
    if ht {
        if two_streams {
            for mcs in [15, 14, 13, 12] {
                rates.push(rate::HT | rate::ANTENNA_AB | mcs);
            }
        }
        for mcs in (0..8).rev() {
            rates.push(rate::HT | rate::ANTENNA_A | mcs);
        }
    }
    let mut legacy: Vec<u8> = bss.rates.iter().map(|rate| rate & 0x7F).collect();
    legacy.sort_unstable_by(|a, b| b.cmp(a));
    legacy.dedup();
    for value in legacy {
        // On 5 GHz there are no CCK rates; with HT, only the slowest legacy
        // rates are useful as a last resort.
        let Some(code) = rate::legacy(value) else { continue };
        if bss.channel > 14 && code & rate::CCK != 0 {
            continue;
        }
        if ht && value > 24 {
            continue;
        }
        rates.push(code | rate::ANTENNA_A);
    }
    if rates.is_empty() {
        rates.push(if bss.channel <= 14 { rate::CCK_1M } else { rate::OFDM_6M });
    }
    rates
}

/// 802.11 status codes, in words.
fn status_code_text(code: u16) -> String {
    let text = match code {
        1 => "unspecified failure",
        10 => "capabilities not supported",
        12 => "denied for another reason",
        13 => "authentication algorithm not supported",
        17 => "the access point is full",
        18 => "rates not supported",
        40 => "invalid element",
        43 => "invalid AKMP",
        _ => "",
    };
    alloc::format!("status {code}{}{text}", if text.is_empty() { "" } else { ", " })
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn acknowledgement_rates_follow_linux() {
            // 802.11g with basic 1, 2, 5.5, 11: all CCK, and 6, 12, 24.
            assert_eq!(acknowledgement_rates(&[0x82, 0x84, 0x8B, 0x96, 12, 18, 24, 36], true), (0xF, 0x15));
            // 5 GHz with basic 6, 12, 24.
            assert_eq!(acknowledgement_rates(&[0x8C, 18, 0x98, 36, 0xB0, 72, 96, 108], false), (0, 0x15));
            // Basic 24 only: 6 and 12 below it are added.
            assert_eq!(acknowledgement_rates(&[0xB0], false), (0, 0x15));
        }

        fn rate_ladders_go_down() {
            let mut frame = alloc::vec![wlan::BEACON, 0, 0, 0];
            frame.resize(36, 0);
            frame.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8B, 0x96, 12, 18, 24, 36]);
            frame.extend_from_slice(&[3, 1, 6]);
            let bss = wlan::parse_bss(&frame, 6, -50).unwrap();
            let legacy = rate_ladder(&bss, false, false);
            assert_eq!(legacy[0], 7 | rate::ANTENNA_A);
            assert_eq!(*legacy.last().unwrap(), rate::CCK_1M);
            let ht = rate_ladder(&bss, true, true);
            assert_eq!(ht[0], rate::HT | rate::ANTENNA_AB | 15);
            assert_eq!(ht.len(), 4 + 8 + 7);
        }
    }
}
