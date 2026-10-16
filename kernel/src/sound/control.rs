//! The ALSA control device (`/dev/snd/controlC0`): what a sound card is,
//! which PCM devices it has, and its mixer controls (playback and capture
//! volume, and switches to mute them), with Linux's ioctls.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{AUDIO_GROUP, Card, Direction};
use crate::errno::{Errno, Result};
use crate::fs::{FileKind, Inode, Metadata, PollSource, ramfs};
use crate::process::user;

const VERSION: u32 = 0x0002_0009;

// Requests ('U').
const PVERSION: u64 = 0x8004_5500;
const CARD_INFO: u64 = 0x8178_5501;
const ELEM_LIST: u64 = 0xC050_5510;
const ELEM_INFO: u64 = 0xC110_5511;
const ELEM_READ: u64 = 0xC4C8_5512;
const ELEM_WRITE: u64 = 0xC4C8_5513;
const ELEM_LOCK: u64 = 0x4040_5514;
const ELEM_UNLOCK: u64 = 0x4040_5515;
const SUBSCRIBE_EVENTS: u64 = 0xC004_5516;
const TLV_READ: u64 = 0xC008_551A;
const HWDEP_NEXT_DEVICE: u64 = 0xC004_5520;
const PCM_NEXT_DEVICE: u64 = 0x8004_5530;
const PCM_INFO: u64 = 0xC120_5531;
const PCM_PREFER_SUBDEVICE: u64 = 0x4004_5532;
const RAWMIDI_NEXT_DEVICE: u64 = 0xC004_5540;
const POWER_STATE: u64 = 0x8004_55D1;

const IFACE_MIXER: u32 = 2;
const TYPE_BOOLEAN: u32 = 1;
const TYPE_INTEGER: u32 = 2;
const ACCESS_READ_WRITE: u32 = 3;
const ID_SIZE: usize = 64;

/// A mixer control: the playback or capture volume, or its switch.
#[derive(Clone, Copy)]
struct Element {
    numid: u32,
    name: &'static str,
    output: bool,
    volume: bool,
}

fn elements(card: &Card) -> Vec<Element> {
    let mut list = Vec::new();
    let mut add = |name, output, volume| {
        list.push(Element { numid: list.len() as u32 + 1, name, output, volume });
    };
    if card.mixer.volume_steps(true).is_some() {
        add("Master Playback Volume", true, true);
    }
    add("Master Playback Switch", true, false);
    if card.capture.is_some() {
        if card.mixer.volume_steps(false).is_some() {
            add("Capture Volume", false, true);
        }
        add("Capture Switch", false, false);
    }
    list
}

fn write_id(bytes: &mut [u8], element: &Element) {
    bytes[..ID_SIZE].fill(0);
    bytes[0..4].copy_from_slice(&element.numid.to_le_bytes());
    bytes[4..8].copy_from_slice(&IFACE_MIXER.to_le_bytes());
    bytes[16..16 + element.name.len()].copy_from_slice(element.name.as_bytes());
}

/// Finds the element an id names: by number, or else by name.
fn find(card: &Card, id: &[u8]) -> Result<Element> {
    let numid = u32::from_le_bytes(id[0..4].try_into().unwrap());
    let name_bytes = &id[16..60];
    let name = &name_bytes[..name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len())];
    elements(card)
        .into_iter()
        .find(|element| if numid != 0 { element.numid == numid } else { element.name.as_bytes() == name })
        .ok_or(Errno::ENOENT)
}

fn copy_text(bytes: &mut [u8], text: &str) {
    let length = text.len().min(bytes.len() - 1);
    bytes[..length].copy_from_slice(&text.as_bytes()[..length]);
}

pub struct ControlNode {
    inode: u64,
    card: Arc<Card>,
}

impl ControlNode {
    pub fn new(card: Arc<Card>) -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number(), card })
    }

    fn card_info(&self, address: u64) -> Result<u64> {
        let card = &self.card;
        let mut info = [0u8; 376];
        info[0..4].copy_from_slice(&(card.index as u32).to_le_bytes());
        copy_text(&mut info[8..24], card.id());
        // ALSA's library picks its configuration for the card by driver.
        copy_text(&mut info[24..40], "HDA-Intel");
        copy_text(&mut info[40..72], card.name);
        copy_text(&mut info[72..152], card.name);
        copy_text(&mut info[168..248], card.name);
        copy_text(&mut info[248..376], "HDA:eliOS");
        user::write_bytes(address, &info)?;
        Ok(0)
    }

    fn list(&self, address: u64) -> Result<u64> {
        let mut header = user::read_bytes(address, 80)?;
        let word = |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let (offset, space) = (word(&header, 0) as usize, word(&header, 4) as usize);
        let pids = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let all = elements(&self.card);
        let listed: Vec<&Element> = all.iter().skip(offset).take(space).collect();
        for (index, element) in listed.iter().enumerate() {
            let mut id = [0u8; ID_SIZE];
            write_id(&mut id, element);
            user::write_bytes(pids + (index * ID_SIZE) as u64, &id)?;
        }
        header[8..12].copy_from_slice(&(listed.len() as u32).to_le_bytes());
        header[12..16].copy_from_slice(&(all.len() as u32).to_le_bytes());
        user::write_bytes(address, &header)?;
        Ok(0)
    }

    fn info(&self, address: u64) -> Result<u64> {
        let mut info = user::read_bytes(address, 272)?;
        let element = find(&self.card, &info[..ID_SIZE])?;
        write_id(&mut info, &element);
        let kind = if element.volume { TYPE_INTEGER } else { TYPE_BOOLEAN };
        info[64..68].copy_from_slice(&kind.to_le_bytes());
        info[68..72].copy_from_slice(&ACCESS_READ_WRITE.to_le_bytes());
        // Two channels (left and right), set together.
        info[72..76].copy_from_slice(&2u32.to_le_bytes());
        info[76..80].fill(0);
        info[80..272].fill(0);
        if element.volume {
            let steps = self.card.mixer.volume_steps(element.output).unwrap_or(0);
            info[80..88].copy_from_slice(&0i64.to_le_bytes());
            info[88..96].copy_from_slice(&i64::from(steps).to_le_bytes());
            info[96..104].copy_from_slice(&1i64.to_le_bytes());
        }
        user::write_bytes(address, &info)?;
        Ok(0)
    }

    fn read(&self, address: u64) -> Result<u64> {
        let mut value = user::read_bytes(address, 1224)?;
        let element = find(&self.card, &value[..ID_SIZE])?;
        write_id(&mut value, &element);
        let (step, on) = self.card.volumes.lock()[usize::from(!element.output)];
        let number = if element.volume { i64::from(step) } else { i64::from(on) };
        value[72..1224].fill(0);
        for channel in 0..2 {
            value[72 + channel * 8..80 + channel * 8].copy_from_slice(&number.to_le_bytes());
        }
        user::write_bytes(address, &value)?;
        Ok(0)
    }

    fn write(&self, address: u64) -> Result<u64> {
        let value = user::read_bytes(address, 1224)?;
        let element = find(&self.card, &value[..ID_SIZE])?;
        // The louder channel sets both.
        let channel = |index: usize| i64::from_le_bytes(value[72 + index * 8..80 + index * 8].try_into().unwrap());
        let number = channel(0).max(channel(1));
        let (step, on) = {
            let mut volumes = self.card.volumes.lock();
            let setting = &mut volumes[usize::from(!element.output)];
            if element.volume {
                let steps = self.card.mixer.volume_steps(element.output).unwrap_or(0);
                setting.0 = number.clamp(0, i64::from(steps)) as u32;
            } else {
                setting.1 = number != 0;
            }
            *setting
        };
        self.card.mixer.set_volume(element.output, step, on);
        self.card.changed.notify();
        Ok(0)
    }

    fn pcm_info(&self, address: u64) -> Result<u64> {
        let request = user::read_bytes(address, 12)?;
        let word = |at: usize| u32::from_le_bytes(request[at..at + 4].try_into().unwrap());
        let direction = if word(8) == 0 { Direction::Playback } else { Direction::Capture };
        if word(0) != 0 || word(4) != 0 {
            return Err(Errno::ENXIO);
        }
        let pcm = self.card.pcm(direction).ok_or(Errno::ENXIO)?;
        pcm.write_info(address)
    }
}

impl Inode for ControlNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            rdev: 116 << 8 | (self.card.index as u64 * 32),
            gid: AUDIO_GROUP,
            ..Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o660)
        }
    }

    fn seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            PVERSION => user::write_value(argument, &VERSION).map(|()| 0),
            CARD_INFO => self.card_info(argument),
            ELEM_LIST => self.list(argument),
            ELEM_INFO => self.info(argument),
            ELEM_READ => self.read(argument),
            ELEM_WRITE => self.write(argument),
            ELEM_LOCK | ELEM_UNLOCK | PCM_PREFER_SUBDEVICE => Ok(0),
            // Events about controls changing aren't delivered (nothing
            // ever reads as ready), but asking for them is fine.
            SUBSCRIBE_EVENTS => Ok(0),
            // No dB scales: mixers show plain steps.
            TLV_READ => Err(Errno::ENXIO),
            PCM_NEXT_DEVICE => {
                let device: i32 = user::read_value(argument)?;
                let next: i32 = if device < 0 { 0 } else { -1 };
                user::write_value(argument, &next).map(|()| 0)
            }
            PCM_INFO => self.pcm_info(argument),
            HWDEP_NEXT_DEVICE | RAWMIDI_NEXT_DEVICE => user::write_value(argument, &-1i32).map(|()| 0),
            // D0: fully on.
            POWER_STATE => user::write_value(argument, &0u32).map(|()| 0),
            _ => Err(Errno::ENOTTY),
        }
    }

    fn read_stream(&self, _buf: &mut [u8], _nonblocking: bool) -> Result<usize> {
        Err(Errno::EAGAIN)
    }

    fn poll(&self, _flags: u32) -> u16 {
        0
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.card.changed.clone());
        true
    }
}
