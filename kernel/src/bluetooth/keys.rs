//! Link keys: what pairing leaves behind, so a paired device can come
//! back without pairing again.
//!
//! When two devices pair, both controllers compute a 128-bit link key
//! and hand it to their hosts to keep (the controller forgets it). When
//! the device connects again, the controller asks the host for the key
//! and the two prove to each other they still have the same one.
//!
//! Keys are kept in `bluetooth/keys` on the data partition (an installed
//! system) or else on the boot partition (the live USB stick), one line a
//! device: address, key, key type, class of device, name. The file is
//! read once one of those is mounted (the Bluetooth thread starts before
//! the file systems are).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use super::{format_address, parse_address};
use crate::fs::{self, Attributes, FileKind, Inode};
use crate::time;

/// A device paired with.
#[derive(Clone)]
pub(super) struct Paired {
    pub address: [u8; 6],
    pub key: [u8; 16],
    /// How the key was made (4: Secure Simple Pairing without protection
    /// against eavesdroppers in the middle, 5: with it; 0: PIN pairing).
    pub key_type: u8,
    pub class: u32,
    pub name: String,
}

pub(super) struct Keys {
    pub list: Vec<Paired>,
    /// Whether the saved keys have been read (or there's nowhere to save
    /// them), and when that was last tried.
    loaded: bool,
    tried_at: u64,
}

/// How often to look for somewhere to keep keys until there is one.
pub(super) const RETRY_MS: u64 = 2000;

impl Keys {
    pub fn new() -> Self {
        Self { list: Vec::new(), loaded: false, tried_at: 0 }
    }

    pub fn find(&self, address: &[u8; 6]) -> Option<&Paired> {
        self.list.iter().find(|paired| paired.address == *address)
    }

    /// When to try loading again, if the saved keys haven't been read.
    pub fn next_try(&self) -> Option<u64> {
        (!self.loaded).then_some(self.tried_at + RETRY_MS)
    }

    /// Reads the saved keys, once there's a file system to read them from.
    pub fn load_if_ready(&mut self) {
        let now = time::ticks();
        if self.loaded || now < self.tried_at + RETRY_MS {
            return;
        }
        self.tried_at = now;
        let Some(base) = base_directory() else { return };
        self.loaded = true;
        let Ok(file) = fs::lookup("/", &format!("{base}/bluetooth/keys"), true) else {
            debug!("bluetooth: no saved keys in {base}/bluetooth");
            return;
        };
        let text = read_all(&file);
        for line in text.lines() {
            match parse_line(line) {
                // Learned before the file was read: newer.
                Some(paired) if self.find(&paired.address).is_some() => {}
                Some(paired) => self.list.push(paired),
                None if line.trim().is_empty() => {}
                None => warn!("bluetooth: can't read {base}/bluetooth/keys line {line:?}"),
            }
        }
        info!("bluetooth: {} paired devices in {base}/bluetooth/keys", self.list.len());
    }

    /// Keeps a new key (replacing any old one for the device) and saves.
    pub fn store(&mut self, paired: Paired) {
        self.list.retain(|other| other.address != paired.address);
        self.list.push(paired);
        self.save();
    }

    /// Forgets a device; returns whether it was paired.
    pub fn remove(&mut self, address: &[u8; 6]) -> bool {
        let before = self.list.len();
        self.list.retain(|other| other.address != *address);
        let removed = self.list.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Writes the file afresh (it's a few lines).
    fn save(&self) {
        let Some(base) = base_directory() else {
            warn!("bluetooth: nowhere to save keys (no /data or /disk); pairing lasts until shutdown");
            return;
        };
        let mut text = String::new();
        for paired in &self.list {
            let _ = write!(text, "{} ", format_address(&paired.address));
            for byte in paired.key {
                let _ = write!(text, "{byte:02x}");
            }
            let _ = writeln!(text, " {:02x} {:06x} {}", paired.key_type, paired.class, paired.name);
        }
        let result = (|| {
            let base = fs::lookup("/", base, true)?;
            let directory = match base.lookup("bluetooth") {
                Ok(directory) => directory,
                Err(_) => base.create("bluetooth", FileKind::Directory, Attributes::root(0o700))?,
            };
            let file = match directory.lookup("keys") {
                Ok(file) => file,
                Err(_) => directory.create("keys", FileKind::Regular, Attributes::root(0o600))?,
            };
            file.truncate(0)?;
            file.write_at(0, text.as_bytes())?;
            Ok::<(), crate::errno::Errno>(())
        })();
        match result {
            Ok(()) => debug!("bluetooth: saved {} keys in {base}/bluetooth/keys", self.list.len()),
            Err(error) => warn!("bluetooth: can't save keys in {base}/bluetooth: {error:?}"),
        }
    }
}

/// Where keys are kept: the data partition if there is one, else the boot
/// partition.
fn base_directory() -> Option<&'static str> {
    ["/data", "/disk"].into_iter().find(|path| fs::mount::is_mounted(path))
}

fn read_all(file: &Arc<dyn Inode>) -> String {
    let mut data = Vec::new();
    let mut buffer = [0u8; 512];
    while let Ok(count @ 1..) = file.read_at(data.len() as u64, &mut buffer) {
        data.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// One line of the file: `ADDRESS KEY TYPE CLASS NAME`.
fn parse_line(line: &str) -> Option<Paired> {
    let mut fields = line.splitn(5, ' ');
    let address = parse_address(fields.next()?)?;
    let hex = fields.next()?;
    if hex.len() != 32 {
        return None;
    }
    let mut key = [0u8; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    let key_type = u8::from_str_radix(fields.next()?, 16).ok()?;
    let class = u32::from_str_radix(fields.next()?, 16).ok()?;
    let name = String::from(fields.next().unwrap_or(""));
    Some(Paired { address, key, key_type, class, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn bluetooth_key_lines_are_read() {
            let paired = parse_line("11:22:33:44:55:01 00112233445566778899aabbccddeeff 05 002540 Test keyboard").unwrap();
            assert_eq!(paired.address, [0x01, 0x55, 0x44, 0x33, 0x22, 0x11]);
            assert_eq!(paired.key[0], 0x00);
            assert_eq!(paired.key[15], 0xFF);
            assert_eq!(paired.key_type, 5);
            assert_eq!(paired.class, 0x002540);
            assert_eq!(paired.name, "Test keyboard");
            assert!(parse_line("11:22:33:44:55:01 0011 05 002540 x").is_none());
            assert!(parse_line("nonsense").is_none());
        }
    }
}
