//! Disks: listing them, mounting, formatting, and the space on them.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::{self, File};
use eli::{eprintln, println, time};

/// A disk or partition opened through its device file, usable by the
/// FAT and ext2 code (which work in 512-byte sectors).
pub struct Device {
    file: File,
    /// Where the partition starts on the device, in sectors (0 when the
    /// device file is the partition itself).
    start: u64,
    sectors: u64,
}

impl Device {
    pub fn open(path: &str) -> eli::Result<Self> {
        let file = File::open_read_write(path)?;
        let sectors = file.device_size()? / 512;
        Ok(Self { file, start: 0, sectors })
    }

    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    pub fn read(&self, sector: u64, buffer: &mut [u8]) -> eli::Result<()> {
        if sector + (buffer.len() / 512) as u64 > self.sectors {
            return Err(eli::Error::INVALID);
        }
        self.file.read_exact_at((self.start + sector) * 512, buffer)
    }

    pub fn write(&self, sector: u64, data: &[u8]) -> eli::Result<()> {
        if sector + (data.len() / 512) as u64 > self.sectors {
            return Err(eli::Error::INVALID);
        }
        self.file.write_all_at((self.start + sector) * 512, data)
    }
}

impl fat::BlockDevice for Device {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> fat::Result<()> {
        Device::read(self, sector, buffer).map_err(|_| fat::Error::Io)
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> fat::Result<()> {
        Device::write(self, sector, data).map_err(|_| fat::Error::Io)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }
}

impl ext2::BlockDevice for Device {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> ext2::Result<()> {
        Device::read(self, sector, buffer).map_err(|_| ext2::Error::Io)
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> ext2::Result<()> {
        Device::write(self, sector, data).map_err(|_| ext2::Error::Io)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }
}

/// A disk, from /sys/block.
pub struct DiskInfo {
    pub name: String,
    pub model: String,
    pub sectors: u64,
    pub removable: bool,
    pub partitions: Vec<PartitionInfo>,
}

pub struct PartitionInfo {
    pub name: String,
    pub number: u32,
    pub sectors: u64,
}

fn sys_value(path: &str) -> String {
    fs::read_to_string(path).map(|text| text.trim().to_string()).unwrap_or_default()
}

/// Every disk the kernel knows, with its partitions.
pub fn disks() -> Vec<DiskInfo> {
    let mut disks: Vec<DiskInfo> = fs::read_dir("/sys/block")
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let base = format!("/sys/block/{}", entry.name);
            let mut partitions: Vec<PartitionInfo> = fs::read_dir(&base)
                .unwrap_or_default()
                .into_iter()
                .filter(|child| fs::exists(&format!("{base}/{}/partition", child.name)))
                .map(|child| {
                    let at = |field: &str| sys_value(&format!("{base}/{}/{field}", child.name));
                    PartitionInfo {
                        name: child.name.clone(),
                        number: at("partition").parse().unwrap_or(0),
                        sectors: at("size").parse().unwrap_or(0),
                    }
                })
                .collect();
            partitions.sort_by_key(|partition| partition.number);
            DiskInfo {
                model: sys_value(&format!("{base}/device/model")),
                sectors: sys_value(&format!("{base}/size")).parse().unwrap_or(0),
                removable: sys_value(&format!("{base}/removable")) == "1",
                name: entry.name,
                partitions,
            }
        })
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

/// What's on a partition, from its first sectors: `ext2`, `vfat`, or
/// `None` (unknown, or not readable by this user).
pub fn detect_filesystem(device: &str) -> Option<&'static str> {
    let file = File::open(device).ok()?;
    let mut start = [0u8; 2048];
    file.read_exact_at(0, &mut start).ok()?;
    if u16::from_le_bytes([start[1080], start[1081]]) == 0xEF53 {
        return Some("ext2");
    }
    if &start[82..90] == b"FAT32   " || &start[54..59] == b"FAT16" || &start[54..59] == b"FAT12" {
        return Some("vfat");
    }
    None
}

/// Sizes like 1.5G, 512M.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 && unit > 0 { format!("{value:.1}{}", UNITS[unit]) } else { format!("{value:.0}{}", UNITS[unit]) }
}

/// `lsblk`: disks and partitions, their sizes, file systems and where
/// they're mounted.
pub fn lsblk(_: &[String]) -> i32 {
    let mounts = fs::mounts();
    let mounted_at = |name: &str| {
        let device = format!("/dev/{name}");
        mounts.iter().find(|m| m.source == device).map(|m| m.target.clone()).unwrap_or_default()
    };
    println!("\x1b[1m{:<14} {:>7}  {:<4}  {:<6}  {:<12} MODEL\x1b[0m", "NAME", "SIZE", "TYPE", "FS", "MOUNTED ON");
    for disk in disks() {
        let kind = if disk.removable { "usb" } else { "disk" };
        let fs_type = detect_filesystem(&format!("/dev/{}", disk.name)).unwrap_or("");
        println!(
            "{:<14} {:>7}  {kind:<4}  {fs_type:<6}  {:<12} {}",
            disk.name,
            human_size(disk.sectors * 512),
            mounted_at(&disk.name),
            disk.model
        );
        let count = disk.partitions.len();
        for (index, partition) in disk.partitions.iter().enumerate() {
            let branch = if index + 1 == count { "└─" } else { "├─" };
            let fs_type = detect_filesystem(&format!("/dev/{}", partition.name)).unwrap_or("");
            println!(
                "{branch}{:<12} {:>7}  part  {fs_type:<6}  {}",
                partition.name,
                human_size(partition.sectors * 512),
                mounted_at(&partition.name)
            );
        }
    }
    0
}

/// `mount`: lists mounts; `mount [-t TYPE] [-o ro|rw|remount|bind] SOURCE
/// DIR` mounts.
pub fn mount(arguments: &[String]) -> i32 {
    let mut kind = String::new();
    let mut options = String::new();
    let mut positional = Vec::new();
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-t" => kind = rest.next().cloned().unwrap_or_default(),
            "-o" => options = rest.next().cloned().unwrap_or_default(),
            "-r" => options.push_str(",ro"),
            _ => positional.push(argument.clone()),
        }
    }
    if positional.is_empty() {
        for entry in fs::mounts() {
            println!("{} on {} type {} ({})", entry.source, entry.target, entry.kind, entry.options);
        }
        return 0;
    }
    let mut flags = 0;
    for option in options.split(',') {
        match option {
            "ro" => flags |= fs::MOUNT_READ_ONLY,
            "remount" => flags |= fs::MOUNT_REMOUNT,
            "bind" => flags |= fs::MOUNT_BIND,
            "rw" | "" | "defaults" => {}
            other => {
                eprintln!("mount: unknown option {other}");
                return 2;
            }
        }
    }
    let (source, target) = match positional.as_slice() {
        [target] if flags & fs::MOUNT_REMOUNT != 0 => (String::new(), target.clone()),
        [source, target] => (source.clone(), target.clone()),
        _ => {
            eprintln!("usage: mount [-t TYPE] [-o OPTIONS] DEVICE DIRECTORY");
            return 2;
        }
    };
    match fs::mount(&source, &target, &kind, flags) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("mount: {target}: {error}");
            1
        }
    }
}

/// `umount DIR` (or the device mounted there).
pub fn umount(arguments: &[String]) -> i32 {
    let Some(target) = arguments.get(1) else {
        eprintln!("usage: umount DIRECTORY");
        return 2;
    };
    // A device name means wherever it's mounted.
    let target = fs::mounts().into_iter().find(|m| m.source == *target).map_or(target.clone(), |m| m.target);
    match fs::unmount(&target) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("umount: {target}: {error}");
            1
        }
    }
}

/// `df`: space on each mounted disk.
pub fn df(_: &[String]) -> i32 {
    println!("\x1b[1m{:<14} {:>7} {:>7} {:>7} {:>4}  MOUNTED ON\x1b[0m", "FILE SYSTEM", "SIZE", "USED", "FREE", "USE");
    for entry in fs::mounts() {
        let Ok((block, total, free)) = fs::space(&entry.target) else { continue };
        if total == 0 || ["proc", "sysfs", "devtmpfs"].contains(&entry.kind.as_str()) {
            continue;
        }
        let used = total - free;
        println!(
            "{:<14} {:>7} {:>7} {:>7} {:>3}%  {}",
            entry.source,
            human_size(total * block),
            human_size(used * block),
            human_size(free * block),
            used * 100 / total,
            entry.target
        );
    }
    0
}

/// A fresh volume id, from the kernel's random numbers.
pub fn random_uuid() -> [u8; 16] {
    let mut uuid = [0u8; 16];
    eli::system::random_bytes(&mut uuid);
    // Version 4 (random), variant 1, as RFC 4122 says.
    uuid[6] = (uuid[6] & 0x0F) | 0x40;
    uuid[8] = (uuid[8] & 0x3F) | 0x80;
    uuid
}

/// Formats a device: `ext2` or `fat` (FAT32).
pub fn format(device: &mut Device, kind: &str, label: &str) -> Result<(), String> {
    match kind {
        "ext2" => {
            let options =
                ext2::FormatOptions { label, uuid: random_uuid(), time: time::unix_time() as u32, block_size: None };
            ext2::FileSystem::format(device, &options).map_err(|error| format!("{error:?}"))
        }
        "fat" | "vfat" | "fat32" => fat::FileSystem::format(device, label).map_err(|error| format!("{error:?}")),
        _ => Err(format!("unknown file system type {kind} (try ext2 or fat)")),
    }
}

/// `mkfs [-t ext2|fat] [-L LABEL] DEVICE`: makes an empty file system.
pub fn mkfs(arguments: &[String]) -> i32 {
    let mut kind = String::from("ext2");
    let mut label = String::new();
    let mut device = None;
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-t" => kind = rest.next().cloned().unwrap_or_default(),
            "-L" | "-n" => label = rest.next().cloned().unwrap_or_default(),
            _ if device.is_none() => device = Some(argument.clone()),
            _ => device = None,
        }
    }
    let Some(path) = device else {
        eprintln!("usage: mkfs [-t ext2|fat] [-L LABEL] DEVICE");
        return 2;
    };
    if fs::mounts().iter().any(|m| m.source == path) {
        eprintln!("mkfs: {path} is mounted");
        return 1;
    }
    let mut device = match Device::open(&path) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("mkfs: {path}: {error}");
            return 1;
        }
    };
    let label = if label.is_empty() { "eliOS".to_string() } else { label };
    match format(&mut device, &kind, &label) {
        Ok(()) => {
            println!("{path}: {kind} file system of {}, labelled {label}", human_size(device.sectors() * 512));
            0
        }
        Err(error) => {
            eprintln!("mkfs: {path}: {error}");
            1
        }
    }
}
