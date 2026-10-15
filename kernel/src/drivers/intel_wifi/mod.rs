//! Intel Wireless 7260 and 3160 Wi-Fi cards (the ThinkPad T440p's, and many
//! other 2013-2015 laptops').
//!
//! The card runs Intel's firmware, which does the radio work and most of
//! 802.11 itself; the driver loads it and talks to it with commands. Its
//! code is closed (it runs on the card, not here) and comes from the
//! linux-firmware collection, in /lib/firmware.
//!
//! Bringing a card up takes two firmware images: "init", which calibrates
//! the radio and lets the driver read the card's settings (its NVM, which
//! holds the MAC address), then "runtime", which does the networking.
//! This first stage runs the init image: loading it, reading the NVM and
//! calibrating. Every step is logged, since this hardware can only be
//! tested on a real machine.

mod firmware;
mod transport;

use alloc::string::String;
use alloc::vec::Vec;

use self::firmware::{Firmware, Image};
use self::transport::{Error, Packet, Transport};
use super::pci;
use crate::{fs, task, time};

/// A card model: its PCI device IDs, name and firmware file.
struct Model {
    ids: &'static [u16],
    name: &'static str,
    firmware: &'static str,
}

const MODELS: [Model; 2] = [
    Model { ids: &[0x08B1, 0x08B2], name: "Intel Wireless 7260", firmware: "iwlwifi-7260-17.ucode" },
    Model { ids: &[0x08B3, 0x08B4], name: "Intel Wireless 3160", firmware: "iwlwifi-3160-17.ucode" },
];

// Firmware commands and notifications (group 0).
const ALIVE: u8 = 0x01;
const INIT_COMPLETE: u8 = 0x04;
const PHY_CONFIGURATION: u8 = 0x6A;
const CALIBRATION_RESULT: u8 = 0x6B;
const NVM_ACCESS: u8 = 0x88;
const TX_ANTENNA_CONFIGURATION: u8 = 0x98;
const BT_CONFIG: u8 = 0x9B;

/// NVM sections: the hardware section (MAC address) and the software one
/// (version, which bands the card supports).
const NVM_SECTION_HARDWARE: u16 = 0;
const NVM_SECTION_SOFTWARE: u16 = 1;
const NVM_CHUNK: u16 = 2048;

/// How long to wait for the root file system (and the firmware file).
const FIRMWARE_WAIT_MS: u64 = 60_000;

/// Starts a thread for each supported card.
pub fn probe(devices: &[pci::Device]) {
    for device in devices.iter().filter(|d| d.vendor == 0x8086) {
        let Some(model) = MODELS.iter().find(|model| model.ids.contains(&device.device)) else { continue };
        let Some(bar) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        // No legacy interrupts while the driver polls.
        device.write(0x04, device.read(0x04) | 1 << 10);
        // Linux turns off the PCI retry timeout (byte 0x41), which can
        // otherwise interfere with the processor's sleep states.
        device.write(0x40, device.read(0x40) & !0xFF00);
        info!("wifi: {} ({:04x}:{:04x})", model.name, device.vendor, device.device);
        task::spawn("wifi", move || {
            if let Err(error) = start(bar, model) {
                warn!("wifi: {}: {error}", model.name);
            }
        });
    }
}

fn start(bar: u64, model: &Model) -> Result<(), String> {
    let file = read_firmware(model.firmware)?;
    let firmware = Firmware::parse(&file).map_err(String::from)?;
    drop(file);
    info!(
        "wifi: firmware {} version {}, radio configuration {:#x}, {} scan channels",
        model.firmware, firmware.version, firmware.phy_config, firmware.scan_channels
    );
    // Capability 12 is "DQA", the queue layout this driver uses.
    debug!(
        "wifi: {} init and {} runtime sections, DQA {}",
        firmware.sections(Image::Init).len(),
        firmware.sections(Image::Runtime).len(),
        if firmware.has_capability(12) { "supported" } else { "NOT supported" }
    );

    let mut card = Transport::new(bar).map_err(describe)?;
    debug!(
        "wifi: hardware revision {:#x}, radio {}",
        card.hw_revision,
        if card.radio_allowed() { "allowed" } else { "off (kill switch)" }
    );
    let result = run_init_image(&mut card, &firmware);
    if let Err(error) = &result {
        dump_state(&card);
        return Err(describe_ref(error));
    }
    Ok(())
}

/// Reads /lib/firmware/<name>, waiting for the root file system if it
/// isn't mounted yet (drivers start before it is).
fn read_firmware(name: &str) -> Result<Vec<u8>, String> {
    let path = alloc::format!("/lib/firmware/{name}");
    let deadline = time::uptime_nanos() + FIRMWARE_WAIT_MS * 1_000_000;
    let inode = loop {
        match fs::lookup("/", &path, true) {
            Ok(inode) => break inode,
            Err(_) if time::uptime_nanos() < deadline => task::sleep_ms(250),
            Err(error) => return Err(alloc::format!("can't open {path}: {error:?}")),
        }
    };
    let size = inode.metadata().size as usize;
    let mut data = alloc::vec![0; size];
    let mut done = 0;
    while done < size {
        match inode.read_at(done as u64, &mut data[done..]) {
            Ok(0) => break,
            Ok(count) => done += count,
            Err(error) => return Err(alloc::format!("reading {path}: {error:?}")),
        }
    }
    data.truncate(done);
    Ok(data)
}

/// Runs the init image: reads the card's settings and calibrates the
/// radio.
fn run_init_image(card: &mut Transport, firmware: &Firmware) -> Result<(), Error> {
    card.prepare()?;
    card.power_up()?;
    card.initialize(firmware.phy_config)?;
    card.load_firmware(firmware.sections(Image::Init))?;
    let alive = card.wait_for(1000, |packet| packet.command == ALIVE && packet.group == 0, log_unexpected)?;
    let status = u16::from_le_bytes([alive.data[0], alive.data[1]]);
    let word =
        |offset: usize| alive.data.get(offset..offset + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    debug!(
        "wifi: init firmware alive: status {status:#x}, {} bytes, version {}.{}, error log at {:#x}",
        alive.data.len(),
        word(4),
        word(8),
        word(20)
    );
    if status != 0xCAFE {
        return Err(Error::Firmware("the init firmware didn't start properly"));
    }
    ERROR_LOG.store(word(20), core::sync::atomic::Ordering::Relaxed);
    card.start_scheduler();

    // Bluetooth coexistence ("BT_COEX_NW", with SCO sync and high band
    // retention), as the radio is shared with the card's Bluetooth.
    let mut bt = Vec::new();
    bt.extend_from_slice(&1u32.to_le_bytes());
    bt.extend_from_slice(&(1u32 << 2 | 1 << 4).to_le_bytes());
    command(card, BT_CONFIG, &bt)?;

    let hardware = read_nvm_section(card, NVM_SECTION_HARDWARE)?;
    let software = read_nvm_section(card, NVM_SECTION_SOFTWARE)?;
    // The MAC address is at word 0x15 of the hardware section, each 16-bit
    // word's bytes swapped.
    let raw = hardware.get(0x2A..0x30).ok_or(Error::Firmware("the NVM's hardware section is too short"))?;
    let mac = [raw[1], raw[0], raw[3], raw[2], raw[5], raw[4]];
    let nvm_word =
        |data: &[u8], index: usize| data.get(index * 2..index * 2 + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    let nvm_version = nvm_word(&software, 0);
    info!(
        "wifi: MAC address {}, NVM version {:#x}, radio config {:#x}, SKU {:#x}",
        crate::net::format_mac(&mac),
        nvm_version,
        nvm_word(&software, 1),
        nvm_word(&software, 2)
    );

    if !card.radio_allowed() {
        warn!("wifi: the radio is switched off (hardware kill switch); not calibrating");
        return Ok(());
    }

    // Calibrate: the antennas that transmit, then the PHY configuration
    // with the init image's calibration triggers. The firmware reports
    // results, then "init complete".
    command(card, TX_ANTENNA_CONFIGURATION, &u32::from(firmware.valid_tx_antennas()).to_le_bytes())?;
    let (flow, event) = firmware.calibration[Image::Init as usize];
    let mut phy = Vec::new();
    phy.extend_from_slice(&firmware.phy_config.to_le_bytes());
    phy.extend_from_slice(&flow.to_le_bytes());
    phy.extend_from_slice(&event.to_le_bytes());
    let sequence = card.send_command(PHY_CONFIGURATION, 0, 0, &phy)?;
    let mut calibration = Vec::new();
    card.wait_for(
        2000,
        |packet| packet.command == INIT_COMPLETE && packet.group == 0,
        |packet| {
            if packet.command == CALIBRATION_RESULT {
                calibration.push(packet.data.clone());
            } else if packet.sequence != sequence {
                log_unexpected(packet);
            }
        },
    )?;
    // The results are what the runtime image will need (the next stage).
    info!("wifi: radio calibrated ({} results)", calibration.len());
    Ok(())
}

/// The address of the firmware's error log in the card's memory, from the
/// alive notification.
static ERROR_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Sends a command and waits for its response.
fn command(card: &mut Transport, command: u8, payload: &[u8]) -> Result<Packet, Error> {
    let sequence = card.send_command(command, 0, 0, payload)?;
    card.wait_for(1000, |packet| packet.sequence == sequence && !packet.is_notification(), log_unexpected)
}

/// Reads a whole NVM section, 2 KiB at a time.
fn read_nvm_section(card: &mut Transport, section: u16) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    loop {
        let offset = data.len() as u16;
        let mut request = Vec::new();
        // Read (opcode 0), target 0, section, offset, length.
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(&section.to_le_bytes());
        request.extend_from_slice(&offset.to_le_bytes());
        request.extend_from_slice(&NVM_CHUNK.to_le_bytes());
        let response = command(card, NVM_ACCESS, &request)?;
        let half =
            |index: usize| response.data.get(index * 2..index * 2 + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        let (length, status) = (half(1), half(3));
        if status != 0 {
            // Reading past the end of a section that's a multiple of 2 KiB
            // fails; that's the end.
            if offset != 0 {
                break;
            }
            debug!("wifi: NVM section {section}: status {status:#x}");
            return Err(Error::Firmware("reading the NVM failed"));
        }
        let bytes = response.data.get(8..8 + usize::from(length)).ok_or(Error::Firmware("short NVM response"))?;
        data.extend_from_slice(bytes);
        if length < NVM_CHUNK {
            break;
        }
    }
    debug!("wifi: NVM section {section}: {} bytes", data.len());
    Ok(data)
}

fn log_unexpected(packet: &Packet) {
    debug!(
        "wifi: packet {:#04x} (group {}, sequence {:#06x}, {} bytes)",
        packet.command,
        packet.group,
        packet.sequence,
        packet.data.len()
    );
}

/// Logs the card's state and the firmware's error log, for reports from
/// real hardware.
fn dump_state(card: &Transport) {
    let registers: Vec<String> = [0x000, 0x008, 0x00C, 0x010, 0x020, 0x024, 0x028, 0x054, 0x088]
        .iter()
        .map(|&register| alloc::format!("{register:#05x}={:#010x}", card.read(register)))
        .collect();
    debug!("wifi: registers {}", registers.join(" "));
    let log = ERROR_LOG.load(core::sync::atomic::Ordering::Relaxed);
    if log != 0 {
        let words: Vec<String> = card.read_memory(log, 16).iter().map(|word| alloc::format!("{word:08x}")).collect();
        debug!("wifi: firmware error log at {log:#x}: {}", words.join(" "));
    }
}

fn describe(error: Error) -> String {
    describe_ref(&error)
}

fn describe_ref(error: &Error) -> String {
    match error {
        Error::Timeout(what) => alloc::format!("timed out waiting for {what}"),
        Error::Firmware(what) => String::from(*what),
        Error::NoMemory => String::from("out of memory"),
    }
}
