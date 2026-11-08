//! Intel's older Bluetooth controllers (7260, 3160, 7265, 8260 ROM
//! parts) and their firmware patches.
//!
//! These controllers run firmware from ROM that has bugs, so each time
//! they power up the host loads a patch for that ROM. The patch file
//! (`intel/ibt-hw-*.bseq`, named from the controller's "read version"
//! answer) is a script: HCI commands, each followed by the event(s) the
//! controller answers with. The host sends each command in manufacturer
//! mode, checks the answer matches the file byte for byte, and on
//! leaving manufacturer mode asks the controller to reset and switch the
//! patch on. This follows Linux (`btintel_legacy_rom_setup`), which is
//! the only description of the procedure there is.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::Hci;

const READ_VERSION: u16 = 0xFC05;
const MANUFACTURER_MODE: u16 = 0xFC11;
const SET_EVENT_MASK: u16 = 0xFC52;
/// The command that writes patch data; a file without it has nothing to
/// switch on.
const WRITE_PATCH: u16 = 0xFC8E;

/// A controller's version, from Intel's "read version" command.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Version {
    hardware: [u8; 3],
    firmware: [u8; 5],
    /// Which patch is running: 0 means none.
    patch: u8,
}

impl Version {
    fn read(hci: &mut Hci) -> Result<Self, String> {
        let reply = hci.command(READ_VERSION, &[])?;
        if reply.len() < 10 {
            return Err(format!("short Intel version {reply:02x?}"));
        }
        Ok(Self {
            hardware: reply[1..4].try_into().unwrap(),
            firmware: reply[4..9].try_into().unwrap(),
            patch: reply[9],
        })
    }

    /// The patch file made for this ROM.
    fn patch_file(&self) -> String {
        let [platform, variant, revision] = self.hardware;
        let [fw_variant, fw_revision, build, week, year] = self.firmware;
        format!(
            "intel/ibt-hw-{platform:x}.{variant:x}.{revision:x}-fw-{fw_variant:x}.{fw_revision:x}.{build:x}.{week:x}.\
             {year:x}.bseq"
        )
    }

    /// The patch file for the hardware, used when there's none for the
    /// exact ROM.
    fn default_patch_file(&self) -> String {
        format!("intel/ibt-hw-{:x}.{:x}.bseq", self.hardware[0], self.hardware[1])
    }

    fn describe(&self) -> String {
        let [platform, variant, revision] = self.hardware;
        let [fw_variant, fw_revision, build, week, year] = self.firmware;
        format!(
            "Intel hardware {platform:x}.{variant:x}.{revision:x}, firmware \
             {fw_variant:x}.{fw_revision:x}.{build:x}.{week:x}.{year:x}"
        )
    }
}

/// One step of a patch file: a command and the event to expect after it
/// (the last one the file lists, as some commands are answered with a
/// Command Status and then a vendor event).
#[derive(Debug, PartialEq)]
struct Step<'a> {
    opcode: u16,
    parameters: &'a [u8],
    event: u8,
    event_parameters: &'a [u8],
}

/// Reads a patch file into steps.
fn parse(mut file: &[u8]) -> Result<Vec<Step<'_>>, String> {
    let mut steps = Vec::new();
    while !file.is_empty() {
        let offset = |rest: &[u8]| file.len() - rest.len();
        let [0x01, low, high, length, rest @ ..] = file else {
            return Err(format!("patch file: no command at byte {}", offset(file)));
        };
        let length = usize::from(*length);
        if rest.len() < length {
            return Err("patch file: command cut short".into());
        }
        let (parameters, mut rest) = rest.split_at(length);
        let mut event = None;
        while let [0x02, code, length, after @ ..] = rest {
            let length = usize::from(*length);
            if after.len() < length {
                return Err("patch file: event cut short".into());
            }
            event = Some((*code, &after[..length]));
            rest = &after[length..];
        }
        let (event, event_parameters) = event.ok_or("patch file: a command without an event")?;
        steps.push(Step { opcode: u16::from_le_bytes([*low, *high]), parameters, event, event_parameters });
        file = rest;
    }
    Ok(steps)
}

/// Patches the controller if it isn't already, and switches on Intel's
/// extra events. Returns a description of the firmware for
/// `/proc/bluetooth`.
pub(super) fn setup(hci: &mut Hci) -> Result<String, String> {
    let version = Version::read(hci)?;
    info!("bluetooth: {}, patch {:#04x}", version.describe(), version.patch);
    let result = if version.patch != 0 {
        // A warm reboot keeps the patch.
        info!("bluetooth: already patched");
        Ok(format!("{}, patch {:#04x} (already loaded)", version.describe(), version.patch))
    } else {
        patch(hci, &version)
    };
    // Intel's vendor events, useful in normal operation (Linux turns on
    // the same ones).
    enter_manufacturer_mode(hci)?;
    hci.command(SET_EVENT_MASK, &[0x87, 0x0C, 0, 0, 0, 0, 0, 0])?;
    hci.command(MANUFACTURER_MODE, &[0, 0])?;
    result
}

fn enter_manufacturer_mode(hci: &mut Hci) -> Result<(), String> {
    hci.command(MANUFACTURER_MODE, &[1, 0]).map(drop)
}

/// Loads the patch file for `version`. Without one, the controller runs
/// its ROM unpatched (mostly works); a patch that fails is switched off.
fn patch(hci: &mut Hci, version: &Version) -> Result<String, String> {
    let (name, file) = match crate::drivers::read_firmware(&version.patch_file()) {
        Ok(file) => (version.patch_file(), file),
        Err(error) => {
            warn!("bluetooth: {error}; trying the default patch");
            match crate::drivers::read_firmware(&version.default_patch_file()) {
                Ok(file) => (version.default_patch_file(), file),
                Err(error) => {
                    warn!("bluetooth: {error}; running unpatched");
                    return Ok(format!("{}, not patched (no file)", version.describe()));
                }
            }
        }
    };
    let steps = parse(&file).map_err(|error| format!("{name}: {error}"))?;
    let writes_patch = steps.iter().any(|step| step.opcode == WRITE_PATCH);
    info!("bluetooth: loading {name}: {} bytes, {} commands", file.len(), steps.len());
    enter_manufacturer_mode(hci)?;
    let applied = steps.iter().enumerate().try_for_each(|(index, step)| {
        let event = hci.command_event(step.opcode, step.parameters, step.event)?;
        let parameters = &event[2..];
        if parameters != step.event_parameters {
            return Err(format!(
                "{name}: step {index} (command {:#06x}): expected {:02x?}, got {:02x?}",
                step.opcode,
                step.event_parameters,
                &parameters[..parameters.len().min(16)]
            ));
        }
        Ok(())
    });
    // Leaving manufacturer mode: 0x02 resets and switches the patch on,
    // 0x01 resets and switches it off, 0x00 doesn't reset (nothing was
    // patched).
    let (exit, outcome) = match (&applied, writes_patch) {
        (Err(_), _) => (1, "switched off"),
        (Ok(()), true) => (2, "loaded"),
        (Ok(()), false) => (0, "had nothing to load"),
    };
    // The controller answers before it resets; the next command waits
    // for it to come back.
    let exited = hci.command(MANUFACTURER_MODE, &[0, exit]);
    if let Err(error) = applied {
        warn!("bluetooth: patch failed: {error}");
        exited?;
        return Err(error);
    }
    exited?;
    let now = Version::read(hci)?;
    info!("bluetooth: patch {outcome}, now patch {:#04x}", now.patch);
    Ok(format!("{}, patch {:#04x} ({name})", now.describe(), now.patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn intel_patch_files_are_parsed() {
            // A write with its Command Complete, then a command answered
            // with Command Status and a vendor event (only the last
            // counts).
            let file = [
                1, 0x8E, 0xFC, 2, 0xAA, 0xBB, 2, 0x0E, 4, 1, 0x8E, 0xFC, 0,
                1, 0x2F, 0xFC, 0, 2, 0x0F, 4, 0, 1, 0x2F, 0xFC, 2, 0xFF, 2, 0x19, 0,
            ];
            let steps = parse(&file).unwrap();
            assert_eq!(steps.len(), 2);
            assert_eq!(steps[0], Step { opcode: 0xFC8E, parameters: &[0xAA, 0xBB], event: 0x0E, event_parameters: &[1, 0x8E, 0xFC, 0] });
            assert_eq!(steps[1], Step { opcode: 0xFC2F, parameters: &[], event: 0xFF, event_parameters: &[0x19, 0] });
            assert!(parse(&file[..file.len() - 1]).is_err());
            assert!(parse(&[1, 0x8E, 0xFC, 0]).is_err());
            let version = Version { hardware: [0x37, 7, 0x10], firmware: [1, 0x80, 2, 3, 0xD], patch: 0 };
            assert_eq!(version.patch_file(), "intel/ibt-hw-37.7.10-fw-1.80.2.3.d.bseq");
            assert_eq!(version.default_patch_file(), "intel/ibt-hw-37.7.bseq");
        }
    }
}
