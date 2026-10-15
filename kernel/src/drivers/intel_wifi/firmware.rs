//! Intel's Wi-Fi firmware files (`iwlwifi-<model>-<version>.ucode`).
//!
//! After a short header, the file is a list of TLVs (type, length, value,
//! padded to 4 bytes). The ones that matter here are the code sections of
//! each firmware image, each starting with the address in the card's
//! memory it's loaded at, and a few values describing the card and what
//! the firmware supports. There are separate images for initialization
//! (calibrating the radio and reading the card's settings) and for normal
//! operation ("runtime").

use alloc::string::String;
use alloc::vec::Vec;

const MAGIC: u32 = 0x0A4C_5749; // "IWL\n"
const HEADER_LEN: usize = 88;

// TLV types.
const TLV_SECTION_RUNTIME: u32 = 19;
const TLV_SECTION_INIT: u32 = 20;
const TLV_DEFAULT_CALIBRATION: u32 = 22;
const TLV_PHY_SKU: u32 = 23;
const TLV_CAPABILITIES: u32 = 30;
const TLV_SCAN_CHANNELS: u32 = 31;
const TLV_FIRMWARE_VERSION: u32 = 36;

/// Section addresses that aren't addresses: markers between the code for
/// the card's two processors, and before "paged" code.
pub const SEPARATOR_CPU1_CPU2: u32 = 0xFFFF_CCCC;
pub const SEPARATOR_PAGING: u32 = 0xAAAA_BBBB;

/// The images in a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Image {
    Runtime = 0,
    Init = 1,
}

pub struct Section {
    /// Where in the card's memory it goes.
    pub address: u32,
    pub data: Vec<u8>,
}

pub struct Firmware {
    /// The version, as Linux shows it ("major.minor.local").
    pub version: String,
    pub runtime: Vec<Section>,
    pub init: Vec<Section>,
    /// The radio's configuration: its type, step and dash, and which
    /// antennas transmit and receive.
    pub phy_config: u32,
    /// For each image: (flow trigger, event trigger) for calibration.
    pub calibration: [(u32, u32); 2],
    /// Capability bits, 32 per index.
    capabilities: Vec<u32>,
    pub scan_channels: u32,
}

impl Firmware {
    pub fn parse(file: &[u8]) -> Result<Self, &'static str> {
        let word =
            |offset: usize| -> Option<u32> { Some(u32::from_le_bytes(file.get(offset..offset + 4)?.try_into().ok()?)) };
        if word(0) != Some(0) || word(4) != Some(MAGIC) {
            return Err("not an Intel firmware file");
        }
        let version = word(72).ok_or("truncated header")?;
        let mut firmware = Firmware {
            version: alloc::format!("{version}"),
            runtime: Vec::new(),
            init: Vec::new(),
            phy_config: 0,
            calibration: [(0, 0); 2],
            capabilities: Vec::new(),
            scan_channels: 0,
        };
        let mut offset = HEADER_LEN;
        while offset + 8 <= file.len() {
            let kind = word(offset).ok_or("truncated TLV")?;
            let len = word(offset + 4).ok_or("truncated TLV")? as usize;
            let value = file.get(offset + 8..offset + 8 + len).ok_or("TLV past the end of the file")?;
            let value_word = |index: usize| u32::from_le_bytes(value[index * 4..index * 4 + 4].try_into().unwrap());
            match kind {
                TLV_SECTION_RUNTIME | TLV_SECTION_INIT if len >= 4 => {
                    let section = Section { address: value_word(0), data: Vec::from(&value[4..]) };
                    if kind == TLV_SECTION_RUNTIME {
                        firmware.runtime.push(section);
                    } else {
                        firmware.init.push(section);
                    }
                }
                TLV_DEFAULT_CALIBRATION if len >= 12 => {
                    // (image, flow trigger, event trigger), images numbered
                    // as `Image` is.
                    let image = value_word(0) as usize;
                    if image < 2 {
                        firmware.calibration[image] = (value_word(1), value_word(2));
                    }
                }
                TLV_PHY_SKU if len >= 4 => firmware.phy_config = value_word(0),
                TLV_FIRMWARE_VERSION if len >= 12 => {
                    firmware.version = alloc::format!("{}.{}.{}", value_word(0), value_word(1), value_word(2));
                }
                TLV_SCAN_CHANNELS if len >= 4 => firmware.scan_channels = value_word(0),
                TLV_CAPABILITIES if len >= 8 => {
                    let (index, bits) = (value_word(0) as usize, value_word(1));
                    if firmware.capabilities.len() <= index {
                        firmware.capabilities.resize(index + 1, 0);
                    }
                    firmware.capabilities[index] |= bits;
                }
                _ => {}
            }
            offset += 8 + len.next_multiple_of(4);
        }
        if firmware.init.is_empty() || firmware.runtime.is_empty() {
            return Err("the firmware file has no code");
        }
        Ok(firmware)
    }

    pub fn sections(&self, image: Image) -> &[Section] {
        match image {
            Image::Runtime => &self.runtime,
            Image::Init => &self.init,
        }
    }

    /// Whether the firmware has capability `bit` (`IWL_UCODE_TLV_CAPA_*`).
    pub fn has_capability(&self, bit: usize) -> bool {
        self.capabilities.get(bit / 32).is_some_and(|word| word & 1 << (bit % 32) != 0)
    }

    /// Which antennas transmit (bits 16-19 of the PHY configuration).
    pub fn valid_tx_antennas(&self) -> u8 {
        (self.phy_config >> 16 & 0xF) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn the_7260_firmware_parses() {
            let file = crate::fs::lookup("/", "/lib/firmware/iwlwifi-7260-17.ucode", true).expect("no 7260 firmware");
            let mut data = alloc::vec![0; file.metadata().size as usize];
            let mut done = 0;
            while done < data.len() {
                done += file.read_at(done as u64, &mut data[done..]).unwrap();
            }
            let firmware = Firmware::parse(&data).unwrap();
            assert_eq!(firmware.version, "17.3216344376.0");
            // Two sections for the one processor, then the separator.
            let addresses: Vec<u32> = firmware.sections(Image::Init).iter().map(|s| s.address).collect();
            assert_eq!(addresses, [0x80_0000, 0, SEPARATOR_CPU1_CPU2]);
            assert_eq!(firmware.sections(Image::Init)[1].data.len(), 180_224);
            assert_eq!(firmware.phy_config, 0x0033_0018);
            assert_eq!(firmware.valid_tx_antennas(), 3);
            assert!(firmware.has_capability(12));
            assert_eq!(firmware.calibration[Image::Init as usize], (0x8C_15FB, 0));
            assert_eq!(firmware.calibration[Image::Runtime as usize], (0x8C_15FB, 0x1133));
        }
    }
}
