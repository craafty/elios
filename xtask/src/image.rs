//! Bootable disk images: a GPT partition table with one EFI System
//! Partition, formatted FAT32 by the same code the kernel uses.
//!
//! The image boots in QEMU and other virtual machines, and written to a USB
//! stick (`dd if=target/eliOS.img of=/dev/sdX`), on real UEFI PCs.

use std::fs;
use std::path::Path;

use fat::{BlockDevice, FileSystem, SECTOR_SIZE};

const SECTOR: u64 = SECTOR_SIZE as u64;
/// The partition starts 1 MiB in, as is customary.
const PARTITION_START: u64 = 2048;
/// Room for the boot files and plenty of user files.
const PARTITION_SECTORS: u64 = 128 * 2048;
/// GPT keeps a backup of its table at the end of the disk.
const BACKUP_SECTORS: u64 = 33;
const DISK_SECTORS: u64 = PARTITION_START + PARTITION_SECTORS + 2048;
// A live system puts its data partition on the stick from 1 GiB in
// (user/utils/src/manager/live.rs), so writing a newer image leaves it be.
const _: () = assert!(DISK_SECTORS <= 1 << 21);

const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
const DISK_GUID: &str = "E1105E1E-0000-4E11-A050-E1105E1E0001";
const PARTITION_GUID: &str = "E1105E1E-0000-4E11-A050-E1105E1E0002";

/// The partition inside the disk image, as a block device.
struct Partition<'a> {
    disk: &'a mut [u8],
}

impl BlockDevice for Partition<'_> {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> fat::Result<()> {
        let start = ((PARTITION_START + sector) * SECTOR) as usize;
        buffer.copy_from_slice(self.disk.get(start..start + buffer.len()).ok_or(fat::Error::Io)?);
        Ok(())
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> fat::Result<()> {
        let start = ((PARTITION_START + sector) * SECTOR) as usize;
        self.disk.get_mut(start..start + data.len()).ok_or(fat::Error::Io)?.copy_from_slice(data);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        PARTITION_SECTORS
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Builds (or updates) the disk image at `path` so its partition holds
/// `files` (path within the partition, contents). An existing image is
/// updated in place, keeping whatever else is on it, unless `fresh`.
pub fn build(path: &Path, files: &[(&str, Vec<u8>)], fresh: bool) -> Result<(), String> {
    let existing = (!fresh).then(|| fs::read(path).ok()).flatten();
    let mut disk = match existing {
        Some(disk) if disk.len() as u64 == DISK_SECTORS * SECTOR && &disk[512..520] == b"EFI PART" => disk,
        _ => {
            let mut disk = vec![0u8; (DISK_SECTORS * SECTOR) as usize];
            write_partition_table(&mut disk);
            FileSystem::format(&mut Partition { disk: &mut disk }, "ELIOS")
                .map_err(|e| format!("formatting: {e:?}"))?;
            disk
        }
    };

    let mut volume =
        FileSystem::mount(Partition { disk: &mut disk }, now).map_err(|e| format!("mounting the image: {e:?}"))?;
    for (file_path, contents) in files {
        let mut directory = volume.root();
        let components: Vec<&str> = file_path.split('/').filter(|c| !c.is_empty()).collect();
        let (name, parents) = components.split_last().ok_or("empty path")?;
        for parent in parents {
            directory = match volume.lookup(&directory, parent) {
                Ok(entry) => entry,
                Err(fat::Error::NotFound) => {
                    volume.create(&directory, parent, true).map_err(|e| format!("creating {parent}: {e:?}"))?
                }
                Err(e) => return Err(format!("looking up {parent}: {e:?}")),
            };
        }
        if let Ok(old) = volume.lookup(&directory, name) {
            volume.remove(&old).map_err(|e| format!("replacing {file_path}: {e:?}"))?;
        }
        let mut file = volume.create(&directory, name, false).map_err(|e| format!("creating {file_path}: {e:?}"))?;
        volume.write(&mut file, 0, contents).map_err(|e| format!("writing {file_path}: {e:?}"))?;
    }
    fs::write(path, &disk).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// A GUID in the mixed-endian byte order GPT uses.
fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
    }
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    bytes
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn write_partition_table(disk: &mut [u8]) {
    let last = DISK_SECTORS - 1;

    // A protective MBR: one partition of type 0xEE covering the disk, so
    // tools that only know MBR leave it alone.
    let mbr = &mut disk[..512];
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[446 + 4] = 0xEE;
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&((DISK_SECTORS - 1).min(0xFFFF_FFFF) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut entries = vec![0u8; 128 * 128];
    entries[0..16].copy_from_slice(&guid_bytes(ESP_TYPE));
    entries[16..32].copy_from_slice(&guid_bytes(PARTITION_GUID));
    entries[32..40].copy_from_slice(&PARTITION_START.to_le_bytes());
    entries[40..48].copy_from_slice(&(PARTITION_START + PARTITION_SECTORS - 1).to_le_bytes());
    for (index, unit) in "eliOS".encode_utf16().enumerate() {
        entries[56 + index * 2..58 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    let entries_crc = crc32(&entries);

    let header = |current: u64, backup: u64, entries_at: u64| {
        let mut header = [0u8; 92];
        header[0..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&current.to_le_bytes());
        header[32..40].copy_from_slice(&backup.to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&(last - BACKUP_SECTORS).to_le_bytes());
        header[56..72].copy_from_slice(&guid_bytes(DISK_GUID));
        header[72..80].copy_from_slice(&entries_at.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&header);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        header
    };
    let at = |lba: u64| (lba * SECTOR) as usize;
    disk[at(1)..at(1) + 92].copy_from_slice(&header(1, last, 2));
    disk[at(2)..at(2) + entries.len()].copy_from_slice(&entries);
    let backup_entries = last - 32;
    disk[at(backup_entries)..at(backup_entries) + entries.len()].copy_from_slice(&entries);
    disk[at(last)..at(last) + 92].copy_from_slice(&header(last, 1, backup_entries));
}
