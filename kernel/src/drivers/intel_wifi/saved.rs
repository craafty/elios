//! Networks joined before, so the computer joins them again by itself.
//!
//! After a network is joined at someone's request, its name and password
//! (and the user name, for an enterprise network) go into
//! `/data/wifi/networks`, readable by root alone, one network a line with
//! each field in hexadecimal (names and passwords may hold any byte),
//! the most recently joined first. Only the data partition keeps them:
//! the boot partition of a USB stick is FAT, which has no permissions,
//! and an enterprise password is someone's own account password. A live
//! stick without a data partition remembers nothing past shutdown.
//!
//! The data partition may be mounted after the Wi-Fi thread starts (a
//! live stick's is mounted by `eli boot`), so the file is looked for every
//! few seconds for the first two minutes.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::fs::{self, Attributes, FileKind};
use crate::time;

const DIRECTORY: &str = "/data";
/// How often to look for the file system, and for how long after boot.
const RETRY_MS: u64 = 3000;
const GIVE_UP_MS: u64 = 120_000;
/// More than this many are forgotten, the longest unused first.
const MOST: usize = 16;

#[derive(Clone)]
pub struct Network {
    pub ssid: Vec<u8>,
    pub password: Vec<u8>,
    /// The user name on an enterprise network (empty otherwise).
    pub identity: Vec<u8>,
}

pub struct Saved {
    pub list: Vec<Network>,
    loaded: bool,
    tried_at: u64,
}

impl Saved {
    pub const fn new() -> Self {
        Self { list: Vec::new(), loaded: false, tried_at: 0 }
    }

    /// Reads the file once its file system is there. Returns true when the
    /// list was just read (so joining one can start).
    pub fn load_if_ready(&mut self) -> bool {
        let now = time::ticks();
        if self.loaded || now < self.tried_at + RETRY_MS {
            return false;
        }
        self.tried_at = now;
        if !fs::mount::is_mounted(DIRECTORY) {
            self.loaded = now > GIVE_UP_MS;
            return false;
        }
        self.loaded = true;
        let Ok(file) = fs::lookup("/", &alloc::format!("{DIRECTORY}/wifi/networks"), true) else {
            debug!("wifi: no saved networks");
            return true;
        };
        let mut data = Vec::new();
        let mut buffer = [0u8; 512];
        while let Ok(count @ 1..) = file.read_at(data.len() as u64, &mut buffer) {
            data.extend_from_slice(&buffer[..count]);
        }
        let text = String::from_utf8_lossy(&data);
        for line in text.lines() {
            match parse_line(line) {
                Some(network) if !self.list.iter().any(|known| known.ssid == network.ssid) => self.list.push(network),
                Some(_) => {}
                None if line.trim().is_empty() => {}
                None => warn!("wifi: can't read a line of {DIRECTORY}/wifi/networks"),
            }
        }
        info!("wifi: {} saved networks", self.list.len());
        true
    }

    /// Keeps a network just joined (first, replacing what was saved for
    /// that name) and saves.
    pub fn remember(&mut self, network: Network) {
        self.list.retain(|known| known.ssid != network.ssid);
        self.list.insert(0, network);
        self.list.truncate(MOST);
        self.save();
    }

    /// Forgets a network; returns whether it was saved.
    pub fn forget(&mut self, ssid: &[u8]) -> bool {
        let before = self.list.len();
        self.list.retain(|known| known.ssid != ssid);
        let removed = self.list.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    fn save(&self) {
        if !fs::mount::is_mounted(DIRECTORY) {
            debug!("wifi: no data partition: the network is remembered until shutdown");
            return;
        }
        let mut text = String::new();
        for network in &self.list {
            for (index, field) in [&network.ssid, &network.password, &network.identity].into_iter().enumerate() {
                if index > 0 {
                    text.push(' ');
                }
                // An empty field is "-", so lines always have three.
                if field.is_empty() {
                    text.push('-');
                }
                for byte in field {
                    let _ = write!(text, "{byte:02x}");
                }
            }
            text.push('\n');
        }
        let result = (|| {
            let base = fs::lookup("/", DIRECTORY, true)?;
            let directory = match base.lookup("wifi") {
                Ok(directory) => directory,
                Err(_) => base.create("wifi", FileKind::Directory, Attributes::root(0o700))?,
            };
            let file = match directory.lookup("networks") {
                Ok(file) => file,
                Err(_) => directory.create("networks", FileKind::Regular, Attributes::root(0o600))?,
            };
            file.truncate(0)?;
            file.write_at(0, text.as_bytes())?;
            Ok::<(), crate::errno::Errno>(())
        })();
        match result {
            Ok(()) => debug!("wifi: saved {} networks", self.list.len()),
            Err(error) => warn!("wifi: can't save networks in {DIRECTORY}/wifi: {error:?}"),
        }
    }
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if text == "-" {
        return Some(Vec::new());
    }
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok()).collect()
}

/// One line: `SSID PASSWORD IDENTITY`, each in hexadecimal.
fn parse_line(line: &str) -> Option<Network> {
    let mut fields = line.split(' ');
    let ssid = parse_hex(fields.next()?)?;
    let password = parse_hex(fields.next()?)?;
    let identity = parse_hex(fields.next()?)?;
    (!ssid.is_empty() && fields.next().is_none()).then_some(Network { ssid, password, identity })
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn saved_network_lines_read_back() {
            assert!(parse_line("50414c332e30 7365637265742070617373 75736572").is_some_and(|network| network.ssid
                == b"PAL3.0"
                && network.password == b"secret pass"
                && network.identity == b"user"));
            let open = parse_line("507572647565204775657374 - -").unwrap();
            assert_eq!(open.ssid, b"Purdue Guest");
            assert!(open.password.is_empty() && open.identity.is_empty());
            assert!(parse_line("- - -").is_none());
            assert!(parse_line("abc - -").is_none());
            assert!(parse_line("41 - - extra").is_none());
        }
    }
}
