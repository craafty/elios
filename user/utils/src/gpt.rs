//! GUID partition tables: reading, making and changing them.
//!
//! A GPT disk starts with a "protective" MBR (one partition of type 0xEE
//! covering the disk, so old tools leave it alone), then a header at
//! sector 1 and 128 entries of 128 bytes from sector 2. A copy of the
//! entries and header sits at the end of the disk. Each header carries a
//! CRC32 of itself and of the entries.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::storage::Device;

const ENTRY_COUNT: usize = 128;
const ENTRY_SIZE: usize = 128;
/// Sectors the entries take (128 entries of 128 bytes).
pub const ENTRY_SECTORS: u64 = (ENTRY_COUNT * ENTRY_SIZE / 512) as u64;
/// Partitions start on 1 MiB boundaries, as is customary (and best for
/// flash memory).
pub const ALIGNMENT: u64 = 2048;

pub type Guid = [u8; 16];

/// The type of an EFI System Partition.
pub const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
/// The type of an eliOS data partition (eliOS's own).
pub const ELIOS_DATA_TYPE: &str = "E1105A00-7E1D-4F0C-8C5B-30E1105A0003";

/// A GUID in GPT's (mixed-endian) byte order, from its text form.
pub fn guid(text: &str) -> Guid {
    let hex: Vec<u8> = text.bytes().filter(|&b| b != b'-').collect();
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let pair = core::str::from_utf8(&hex[index * 2..index * 2 + 2]).unwrap_or("00");
        *byte = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    bytes
}

/// A GUID's text form (lowercase, as Linux shows them).
pub fn guid_text(bytes: &Guid) -> String {
    let mut bytes = *bytes;
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    let hex: String = bytes.iter().map(|byte| alloc::format!("{byte:02x}")).collect();
    alloc::format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// A new random GUID.
pub fn random_guid() -> Guid {
    crate::storage::random_uuid()
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub type_guid: Guid,
    pub guid: Guid,
    pub first: u64,
    pub last: u64,
    pub attributes: u64,
    pub name: String,
}

impl Entry {
    pub fn sectors(&self) -> u64 {
        self.last + 1 - self.first
    }

    fn parse(data: &[u8]) -> Option<Self> {
        let type_guid: Guid = data[0..16].try_into().unwrap();
        if type_guid == [0; 16] {
            return None;
        }
        let units: Vec<u16> = data[56..128].chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        let len = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
        Some(Self {
            type_guid,
            guid: data[16..32].try_into().unwrap(),
            first: u64::from_le_bytes(data[32..40].try_into().unwrap()),
            last: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            attributes: u64::from_le_bytes(data[48..56].try_into().unwrap()),
            name: String::from_utf16_lossy(&units[..len]),
        })
    }

    fn write(&self, data: &mut [u8]) {
        data[0..16].copy_from_slice(&self.type_guid);
        data[16..32].copy_from_slice(&self.guid);
        data[32..40].copy_from_slice(&self.first.to_le_bytes());
        data[40..48].copy_from_slice(&self.last.to_le_bytes());
        data[48..56].copy_from_slice(&self.attributes.to_le_bytes());
        for (index, unit) in self.name.encode_utf16().take(36).enumerate() {
            data[56 + index * 2..58 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
}

/// A partition table. Slots are kept in place, so partition numbers (and
/// names like `sda2`) don't change when one is added.
pub struct Table {
    pub disk_guid: Guid,
    pub sectors: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub slots: Vec<Option<Entry>>,
}

impl Table {
    /// An empty table for a disk of `sectors`.
    pub fn new(sectors: u64) -> Self {
        Self {
            disk_guid: random_guid(),
            sectors,
            first_usable: 2 + ENTRY_SECTORS,
            last_usable: sectors - 2 - ENTRY_SECTORS,
            slots: vec![None; ENTRY_COUNT],
        }
    }

    /// The disk's table, if it has a (valid) GPT.
    pub fn read(device: &Device) -> Option<Self> {
        let mut header = [0u8; 512];
        device.read(1, &mut header).ok()?;
        if &header[0..8] != b"EFI PART" {
            return None;
        }
        let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        if !(92..=512).contains(&size) {
            return None;
        }
        let mut check = header[..size].to_vec();
        let stored = u32::from_le_bytes(check[16..20].try_into().unwrap());
        check[16..20].fill(0);
        if crc32(&check) != stored {
            return None;
        }
        let entries_at = u64::from_le_bytes(header[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
        if count == 0 || count > ENTRY_COUNT || entry_size != ENTRY_SIZE {
            return None;
        }
        let mut entries = vec![0u8; (count * entry_size).next_multiple_of(512)];
        device.read(entries_at, &mut entries).ok()?;
        let mut slots: Vec<Option<Entry>> = entries.chunks(ENTRY_SIZE).take(count).map(Entry::parse).collect();
        slots.resize(ENTRY_COUNT, None);
        Some(Self {
            disk_guid: header[56..72].try_into().unwrap(),
            sectors: device.sectors(),
            first_usable: u64::from_le_bytes(header[40..48].try_into().unwrap()),
            last_usable: u64::from_le_bytes(header[48..56].try_into().unwrap()),
            slots,
        })
    }

    /// The partitions, with their numbers (from 1).
    pub fn partitions(&self) -> Vec<(u32, &Entry)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| Some((index as u32 + 1, slot.as_ref()?))).collect()
    }

    /// The unused stretches of the disk, aligned: (first, last) sectors.
    pub fn free_space(&self) -> Vec<(u64, u64)> {
        let mut used: Vec<(u64, u64)> = self.partitions().iter().map(|(_, entry)| (entry.first, entry.last)).collect();
        used.sort_unstable();
        let mut free = Vec::new();
        let mut next = self.first_usable;
        for (first, last) in used.into_iter().chain(core::iter::once((self.last_usable + 1, self.last_usable + 1))) {
            let start = next.next_multiple_of(ALIGNMENT);
            if first > start {
                // End on an alignment boundary too, unless it's the disk's end.
                let end = if first > self.last_usable { first - 1 } else { (first / ALIGNMENT) * ALIGNMENT - 1 };
                if end >= start {
                    free.push((start, end));
                }
            }
            next = next.max(last + 1);
        }
        free
    }

    /// Adds a partition in the first empty slot. Returns its number.
    pub fn add(&mut self, entry: Entry) -> Option<u32> {
        let index = self.slots.iter().position(Option::is_none)?;
        self.slots[index] = Some(entry);
        Some(index as u32 + 1)
    }

    fn entries_bytes(&self) -> Vec<u8> {
        let mut entries = vec![0u8; ENTRY_COUNT * ENTRY_SIZE];
        for (slot, data) in self.slots.iter().zip(entries.chunks_mut(ENTRY_SIZE)) {
            if let Some(entry) = slot {
                entry.write(data);
            }
        }
        entries
    }

    fn header(&self, current: u64, backup: u64, entries_at: u64, entries_crc: u32) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[0..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&current.to_le_bytes());
        header[32..40].copy_from_slice(&backup.to_le_bytes());
        header[40..48].copy_from_slice(&self.first_usable.to_le_bytes());
        header[48..56].copy_from_slice(&self.last_usable.to_le_bytes());
        header[56..72].copy_from_slice(&self.disk_guid);
        header[72..80].copy_from_slice(&entries_at.to_le_bytes());
        header[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
        header[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        header
    }

    /// Writes the table: both copies, and with `protective_mbr`, a fresh
    /// protective MBR (for a disk that's being wiped; an existing disk's MBR
    /// is left alone).
    pub fn write(&self, device: &Device, protective_mbr: bool) -> eli::Result<()> {
        let last = self.sectors - 1;
        if protective_mbr {
            let mut mbr = [0u8; 512];
            mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
            mbr[446 + 4] = 0xEE;
            mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
            mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
            mbr[446 + 12..446 + 16].copy_from_slice(&(last.min(0xFFFF_FFFF) as u32).to_le_bytes());
            mbr[510] = 0x55;
            mbr[511] = 0xAA;
            device.write(0, &mbr)?;
        }
        let entries = self.entries_bytes();
        let entries_crc = crc32(&entries);
        let backup_entries = last - ENTRY_SECTORS;
        device.write(2, &entries)?;
        device.write(backup_entries, &entries)?;
        device.write(last, &self.header(last, 1, backup_entries, entries_crc))?;
        // The primary header last: until it's written, the old table stands.
        device.write(1, &self.header(1, last, 2, entries_crc))
    }
}
