//! Disks and partitions.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// Storage made of 512-byte sectors.
pub trait Disk: Send + Sync {
    fn name(&self) -> &str;
    #[allow(dead_code, reason = "for tools that show disk sizes")]
    fn sectors(&self) -> u64;
    /// Reads whole sectors starting at `lba`.
    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
    /// An EFI System Partition (FAT).
    EfiSystem,
    /// Another partition an MBR marks as FAT32.
    Fat32,
    Other,
}

/// A slice of a disk.
#[derive(Clone)]
pub struct Partition {
    pub disk: Arc<dyn Disk>,
    pub start: u64,
    pub sectors: u64,
    pub kind: PartitionKind,
}

/// GPT's type GUID for EFI System Partitions, in its on-disk byte order.
const ESP_TYPE: [u8; 16] =
    [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];

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
        if size < 128 {
            return Vec::new();
        }
        let mut table = vec![0u8; (count * size).next_multiple_of(512)];
        if disk.read(entries_lba, &mut table).is_err() {
            return Vec::new();
        }
        return table
            .chunks(size)
            .take(count)
            .filter(|entry| entry[..16].iter().any(|&b| b != 0))
            .map(|entry| {
                let (first, last) = (le64(entry, 32), le64(entry, 40));
                let kind = if entry[..16] == ESP_TYPE { PartitionKind::EfiSystem } else { PartitionKind::Other };
                Partition { disk: disk.clone(), start: first, sectors: last + 1 - first, kind }
            })
            .collect();
    }

    if mbr[510..512] != [0x55, 0xAA] {
        return Vec::new();
    }
    (0..4)
        .map(|index| &mbr[446 + index * 16..462 + index * 16])
        .filter(|entry| entry[4] != 0)
        .map(|entry| {
            let kind = match entry[4] {
                0xEF => PartitionKind::EfiSystem,
                0x0B | 0x0C => PartitionKind::Fat32,
                _ => PartitionKind::Other,
            };
            Partition {
                disk: disk.clone(),
                start: u64::from(le32(entry, 8)),
                sectors: u64::from(le32(entry, 12)),
                kind,
            }
        })
        .collect()
}

impl fat::BlockDevice for Partition {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> fat::Result<()> {
        if sector + (buffer.len() / 512) as u64 > self.sectors {
            return Err(fat::Error::Io);
        }
        self.disk.read(self.start + sector, buffer).map_err(|_| fat::Error::Io)
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> fat::Result<()> {
        if sector + (data.len() / 512) as u64 > self.sectors {
            return Err(fat::Error::Io);
        }
        self.disk.write(self.start + sector, data).map_err(|_| fat::Error::Io)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }
}
