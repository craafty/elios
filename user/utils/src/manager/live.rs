//! A data partition for the live USB stick, so what is installed and set
//! up while running from it (packages, Wi-Fi networks, the Firefox profile
//! with its sign-ins) is still there at the next boot.
//!
//! Writing the image to a stick leaves the rest of the stick unused. At
//! boot, a live system (one started without `root=` or `data=`) looks
//! on its own disk for a partition of eliOS's data type; if there is none
//! and there is room, it adds one, formats it ext2, and mounts it at /data
//! in place of the memory-backed folder there. The partition starts at a
//! fixed place, 1 GiB in, well past the end of the image, so writing a
//! newer image over the stick leaves its contents alone: the new image's
//! table no longer lists it, but the file system is still there at the
//! same place, and the next boot finds it and lists it again instead of
//! formatting.

use alloc::format;
use alloc::string::{String, ToString};

use eli::fs;
use eli::println;

use crate::gpt::{self, Entry, Table};
use crate::storage::{self, Device};

/// Where the data partition starts, in sectors (1 GiB), whatever the size
/// of the image: images must stay smaller than this.
const START: u64 = 1 << 21;
/// Less room than this (1 GiB) after START and there's no partition.
const SMALLEST: u64 = 1 << 21;
/// No bigger than 16 GiB: plenty for packages, and formatting (which
/// writes every inode table) stays quick on a slow stick.
const LARGEST: u64 = 1 << 25;
const LABEL: &str = "eliOS data";

/// Finds or makes the stick's data partition and mounts it at /data;
/// nothing happens on installed systems and on disks without room.
pub fn mount_data() {
    let command_line = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if command_line.split_whitespace().any(|word| word.starts_with("root=") || word.starts_with("data="))
        || fs::mounts().iter().any(|mount| mount.target == "/data")
    {
        return;
    }
    if let Err(message) = find_or_make() {
        println!("eli: live data: {message}");
    }
}

fn find_or_make() -> Result<(), String> {
    let Some(disk) = boot_disk() else { return Err("no boot disk".into()) };
    let path = format!("/dev/{disk}");
    let device = Device::open(&path).map_err(|e| format!("{path}: {e}"))?;
    let mut table = Table::read(&device).ok_or_else(|| format!("{disk} has no partition table"))?;
    let data_type = gpt::guid(gpt::ELIOS_DATA_TYPE);
    let number = match table.partitions().into_iter().find(|(_, entry)| entry.type_guid == data_type) {
        Some((number, _)) => number,
        None => {
            // The table as an image writes it ends where the image does.
            table.last_usable = table.sectors - 2 - gpt::ENTRY_SECTORS;
            // No room (a disk image just the image's size): quietly
            // nothing, /data stays in memory.
            let Some((_, last)) = table.free_space().into_iter().find(|&(first, last)| first <= START && last >= START)
            else {
                return Ok(());
            };
            let room = last - START + 1;
            if room < SMALLEST {
                return Ok(());
            }
            let adopted = existing_volume(&device);
            let sectors = adopted.unwrap_or(room.min(LARGEST)).min(room);
            let entry = Entry {
                type_guid: data_type,
                guid: gpt::random_guid(),
                first: START,
                last: START + sectors - 1,
                attributes: 0,
                name: LABEL.into(),
            };
            let number = table.add(entry).ok_or("the partition table is full")?;
            table.write(&device, true).map_err(|e| format!("writing the partition table: {e}"))?;
            if let Ok(file) = fs::File::open(&path) {
                let _ = file.reread_partitions();
            }
            if adopted.is_some() {
                println!("eli: live data: found the data from before ({})", storage::human_size(sectors * 512));
            } else {
                println!("eli: live data: making a {} data partition", storage::human_size(sectors * 512));
                let mut volume = Device::slice(&path, START, sectors).map_err(|e| e.to_string())?;
                storage::format(&mut volume, "ext2", LABEL).map_err(|e| format!("formatting: {e}"))?;
            }
            number
        }
    };
    let partition = storage::disks()
        .into_iter()
        .find(|d| d.name == disk)
        .and_then(|d| d.partitions.into_iter().find(|p| p.number == number))
        .ok_or_else(|| format!("{disk} partition {number} has no device file"))?;
    let source = format!("/dev/{}", partition.name);
    fs::mount(&source, "/data", "ext2", 0).map_err(|e| format!("mounting {source}: {e}"))?;
    println!("eli: live data: {source} is /data");
    Ok(())
}

/// The disk the system started from (the one holding /disk).
fn boot_disk() -> Option<String> {
    let source = fs::mounts().into_iter().find(|mount| mount.target == "/disk")?.source;
    let partition = source.strip_prefix("/dev/")?.to_string();
    storage::disks().into_iter().find(|disk| disk.partitions.iter().any(|p| p.name == partition)).map(|disk| disk.name)
}

/// The size in sectors of an eliOS data volume left at START by an
/// earlier boot, if there is one.
fn existing_volume(device: &Device) -> Option<u64> {
    let mut superblock = [0u8; 1024];
    device.read(START + 2, &mut superblock).ok()?;
    let word = |at: usize| u32::from_le_bytes(superblock[at..at + 4].try_into().unwrap());
    if u16::from_le_bytes([superblock[56], superblock[57]]) != 0xEF53
        || !superblock[120..136].starts_with(LABEL.as_bytes())
    {
        return None;
    }
    let block_size = 1024u64.checked_shl(word(24)).filter(|&size| size <= 65536)?;
    Some(u64::from(word(4)) * block_size / 512)
}
