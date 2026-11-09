//! Disks and partitions.
//!
//! Drivers register each disk they find; the disk then appears in /dev
//! (`sda`, `nvme0n1`, ...) with a node per partition (`sda1`, `nvme0n1p1`),
//! named as on Linux so tools and habits carry over.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::SpinLock;

/// Storage made of 512-byte sectors.
pub trait Disk: Send + Sync {
    /// The name in /dev (`sda`).
    fn name(&self) -> &str;
    /// The manufacturer's model name, for people.
    fn model(&self) -> &str;
    fn sectors(&self) -> u64;
    /// Whether it can be unplugged (a USB stick).
    fn removable(&self) -> bool {
        false
    }
    /// Reads whole sectors starting at `lba`.
    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str>;
}

static DISKS: SpinLock<Vec<Arc<dyn Disk>>> = SpinLock::new(Vec::new());

/// Requests and sectors read and written, by disk name: Linux's
/// `/sys/block/NAME/stat`, which shows whether I/O comes in few big
/// requests or many small ones (what a USB stick is slowest at).
static STATISTICS: SpinLock<BTreeMap<String, [u64; 4]>> = SpinLock::new(BTreeMap::new());

fn count(disk: &str, write: bool, bytes: usize) {
    let mut statistics = STATISTICS.lock();
    let counts = match statistics.get_mut(disk) {
        Some(counts) => counts,
        None => statistics.entry(disk.into()).or_default(),
    };
    let at = if write { 2 } else { 0 };
    counts[at] += 1;
    counts[at + 1] += (bytes / 512) as u64;
}

/// `/sys/block/NAME/stat`'s line: reads, merged, sectors, time, writes,
/// merged, sectors, time, then the queue fields (only the counts are kept).
pub fn statistics_text(disk: &str) -> String {
    let [reads, read_sectors, writes, written_sectors] = STATISTICS.lock().get(disk).copied().unwrap_or_default();
    format!("{reads} 0 {read_sectors} 0 {writes} 0 {written_sectors} 0 0 0 0 0 0 0 0 0 0\n")
}
/// The next letter for an `sd` disk (SATA or USB, as on Linux).
static NEXT_SD: AtomicUsize = AtomicUsize::new(0);

/// The next free `sdX` name.
pub fn next_sd_name() -> String {
    let index = NEXT_SD.fetch_add(1, Ordering::Relaxed);
    let letter = (b'a' + (index % 26) as u8) as char;
    if index < 26 { format!("sd{letter}") } else { format!("sd{}{letter}", (b'a' + (index / 26 - 1) as u8) as char) }
}

/// Makes a disk known: it gets device files, and its partitions can be
/// mounted.
pub fn register(disk: Arc<dyn Disk>) {
    DISKS.lock().push(disk.clone());
    crate::fs::devfs::add_disk(&disk);
}

/// Forgets a disk that went away (a USB stick pulled out).
#[allow(dead_code, reason = "for hot-unpluggable disks")]
pub fn unregister(name: &str) {
    DISKS.lock().retain(|disk| disk.name() != name);
    crate::fs::devfs::remove_disk(name);
}

pub fn disks() -> Vec<Arc<dyn Disk>> {
    DISKS.lock().clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
    /// An EFI System Partition (FAT).
    EfiSystem,
    /// eliOS's own: a data partition (or, from the first installer, a root).
    Elios,
    /// Another partition an MBR marks as FAT32.
    Fat32,
    Other,
}

/// A slice of a disk (or a whole disk).
#[derive(Clone)]
pub struct Partition {
    pub disk: Arc<dyn Disk>,
    /// Its number in the partition table (from 1), or 0 for the whole disk.
    pub number: u32,
    pub start: u64,
    pub sectors: u64,
    pub kind: PartitionKind,
    /// GPT's unique partition GUID (zero for MBR partitions), in its
    /// on-disk byte order.
    pub guid: [u8; 16],
}

impl Partition {
    /// The whole disk, as one partition.
    pub fn whole(disk: &Arc<dyn Disk>) -> Self {
        Self {
            disk: disk.clone(),
            number: 0,
            start: 0,
            sectors: disk.sectors(),
            kind: PartitionKind::Other,
            guid: [0; 16],
        }
    }

    /// The name in /dev: `sda1`, or `nvme0n1p1` when the disk's name ends in
    /// a digit (as on Linux).
    pub fn name(&self) -> String {
        let disk = self.disk.name();
        match self.number {
            0 => disk.into(),
            number if disk.ends_with(|c: char| c.is_ascii_digit()) => format!("{disk}p{number}"),
            number => format!("{disk}{number}"),
        }
    }

    /// Reads sectors `sector..` of the partition, refusing to go past its end.
    pub fn read(&self, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if sector + (buffer.len() / 512) as u64 > self.sectors {
            return Err("past the end of the partition");
        }
        count(self.disk.name(), false, buffer.len());
        self.disk.read(self.start + sector, buffer)
    }

    pub fn write(&self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        if sector + (data.len() / 512) as u64 > self.sectors {
            return Err("past the end of the partition");
        }
        count(self.disk.name(), true, data.len());
        self.disk.write(self.start + sector, data)
    }
}

/// GPT's type GUID for EFI System Partitions, in its on-disk byte order.
const ESP_TYPE: [u8; 16] =
    [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
/// eliOS's own type GUIDs for its data partition,
/// E1105A00-7E1D-4F0C-8C5B-30E1105A0003, and the first installer's root
/// partition (…0002), in on-disk byte order.
pub const ELIOS_DATA_TYPE: [u8; 16] =
    [0x00, 0x5A, 0x10, 0xE1, 0x1D, 0x7E, 0x0C, 0x4F, 0x8C, 0x5B, 0x30, 0xE1, 0x10, 0x5A, 0x00, 0x03];
pub const ELIOS_ROOT_TYPE: [u8; 16] =
    [0x00, 0x5A, 0x10, 0xE1, 0x1D, 0x7E, 0x0C, 0x4F, 0x8C, 0x5B, 0x30, 0xE1, 0x10, 0x5A, 0x00, 0x02];

/// The partitions on a disk, from its GPT or (failing that) MBR.
pub fn partitions(disk: &Arc<dyn Disk>) -> Vec<Partition> {
    let mut sectors = vec![0u8; 512 * 2];
    if disk.read(0, &mut sectors).is_err() {
        return Vec::new();
    }
    let (mbr, gpt) = sectors.split_at(512);
    let le64 = |data: &[u8], at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    let le32 = |data: &[u8], at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());

    if &gpt[..8] == b"EFI PART" {
        let entries_lba = le64(gpt, 72);
        let count = le32(gpt, 80).min(256) as usize;
        let size = le32(gpt, 84) as usize;
        if !(128..=4096).contains(&size) {
            return Vec::new();
        }
        let mut table = vec![0u8; (count * size).next_multiple_of(512)];
        if disk.read(entries_lba, &mut table).is_err() {
            return Vec::new();
        }
        return table
            .chunks(size)
            .take(count)
            .enumerate()
            .filter(|(_, entry)| entry[..16].iter().any(|&b| b != 0))
            .filter_map(|(index, entry)| {
                let (first, last) = (le64(entry, 32), le64(entry, 40));
                if last < first || last >= disk.sectors() {
                    return None;
                }
                let kind = match entry[..16].try_into().unwrap() {
                    ESP_TYPE => PartitionKind::EfiSystem,
                    ELIOS_DATA_TYPE | ELIOS_ROOT_TYPE => PartitionKind::Elios,
                    _ => PartitionKind::Other,
                };
                Some(Partition {
                    disk: disk.clone(),
                    number: index as u32 + 1,
                    start: first,
                    sectors: last + 1 - first,
                    kind,
                    guid: entry[16..32].try_into().unwrap(),
                })
            })
            .collect();
    }

    if mbr[510..512] != [0x55, 0xAA] {
        return Vec::new();
    }
    (0..4)
        .map(|index| (index, &mbr[446 + index * 16..462 + index * 16]))
        .filter(|(_, entry)| entry[4] != 0 && le32(entry, 12) != 0)
        .map(|(index, entry)| {
            let kind = match entry[4] {
                0xEF => PartitionKind::EfiSystem,
                0x0B | 0x0C => PartitionKind::Fat32,
                _ => PartitionKind::Other,
            };
            Partition {
                disk: disk.clone(),
                number: index as u32 + 1,
                start: u64::from(le32(entry, 8)),
                sectors: u64::from(le32(entry, 12)),
                kind,
                guid: [0; 16],
            }
        })
        .collect()
}

/// Formats a GUID in its usual text form, from GPT's mixed-endian bytes.
pub fn guid_text(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[3],
        bytes[2],
        bytes[1],
        bytes[0],
        bytes[5],
        bytes[4],
        bytes[7],
        bytes[6],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

/// The major and minor device numbers Linux would give a disk or partition:
/// SCSI-style disks (`sd`) are major 8 with 16 minors per disk, NVMe major
/// 259. Programs show these (`ls -l /dev`) and /proc/partitions lists them.
pub fn device_numbers(partition: &Partition) -> (u32, u32) {
    let name = partition.disk.name();
    let disk_index = if let Some(letters) = name.strip_prefix("sd") {
        letters.bytes().fold(0u32, |index, letter| index * 26 + u32::from(letter - b'a'))
    } else {
        name.bytes().find(u8::is_ascii_digit).map_or(0, |digit| u32::from(digit - b'0'))
    };
    let major = if name.starts_with("sd") { 8 } else { 259 };
    (major, disk_index * 16 + partition.number)
}

/// The device number (`st_rdev`), encoded as Linux's `makedev` does.
pub fn device_number(partition: &Partition) -> u64 {
    let (major, minor) = device_numbers(partition);
    let (major, minor) = (u64::from(major), u64::from(minor));
    (minor & 0xFF) | ((major & 0xFFF) << 8) | ((minor & !0xFF) << 12) | ((major & !0xFFF) << 32)
}

/// Parses a GUID's text form into GPT's byte order.
pub fn parse_guid(text: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = text.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 || text.len() != 36 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let pair = core::str::from_utf8(&hex[index * 2..index * 2 + 2]).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    Some(bytes)
}

/// Finds a partition by its GPT GUID, on any disk.
pub fn find_partition(guid: &[u8; 16]) -> Option<Partition> {
    disks().iter().flat_map(partitions).find(|partition| partition.guid == *guid)
}

impl fat::BlockDevice for Partition {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> fat::Result<()> {
        Partition::read(self, sector, buffer).map_err(|_| fat::Error::Io)
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> fat::Result<()> {
        Partition::write(self, sector, data).map_err(|_| fat::Error::Io)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }
}

impl ext2::BlockDevice for Partition {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> ext2::Result<()> {
        Partition::read(self, sector, buffer).map_err(|_| ext2::Error::Io)
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> ext2::Result<()> {
        Partition::write(self, sector, data).map_err(|_| ext2::Error::Io)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn guids_convert_between_text_and_gpt_bytes() {
            let text = "e1105a00-7e1d-4f0c-8c5b-30e1105a0002";
            assert_eq!(parse_guid(text), Some(ELIOS_ROOT_TYPE));
            assert_eq!(guid_text(&ELIOS_ROOT_TYPE), text);
            assert_eq!(parse_guid("not a guid"), None);
        }

        fn disks_have_linux_names() {
            let disks = disks();
            assert!(disks.iter().any(|disk| disk.name() == "sda"), "no sda");
            let partition = partitions(&disks[0]).into_iter().next().expect("the boot disk has a partition");
            assert_eq!(partition.name(), "sda1");
            assert_eq!(partition.kind, PartitionKind::EfiSystem);
        }
    }
}
