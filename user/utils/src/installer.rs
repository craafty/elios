//! `installer`: puts eliOS on a disk, so the computer starts it without the
//! USB stick (or live disk) it's running from.
//!
//! An installed system is two partitions: an EFI System Partition (FAT32)
//! with eliboot, the kernel and the initrd, and an ext2 root partition
//! holding the system's files, which the kernel finds through
//! `root=PARTUUID=...` in `\EFI\eliOS\cmdline.txt`. Two ways to get there:
//!
//! - **Erase the disk**: a new partition table with both partitions.
//!   eliboot also goes in the firmware's fallback place
//!   (`\EFI\BOOT\BOOTX64.EFI`), so the disk starts even without a boot
//!   entry.
//! - **Alongside** another system: the root partition goes in free space,
//!   and the boot files into the disk's existing EFI System Partition
//!   (`\EFI\eliOS`), leaving the other system's alone.
//!
//! Either way, a firmware boot entry named "eliOS" is added (through
//! /sys/firmware/efi/efivars) and put first in the boot order.
//!
//! The system files come from the initrd on the boot partition (/disk): a
//! pristine copy, not the running system with whatever was changed in it.
//! The first account is set up as an administrator (it can use sudo); root
//! gets no password, so it can't be logged into directly.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eli::io::{self, Lines};
use eli::{fs, print, println, time, users};

use crate::gpt::{self, Entry, Table};
use crate::storage::{self, Device, DiskInfo};

const BOOT_FILES: &str = "/disk/EFI";
const ESP_LABEL: &str = "EFI";
const ROOT_LABEL: &str = "eliOS";
/// The smallest root partition worth installing to.
const MIN_ROOT_SECTORS: u64 = 512 * 2048;
/// UEFI's global variables (boot entries).
const GLOBAL_VARIABLES: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFIVARS: &str = "/sys/firmware/efi/efivars";
/// Non-volatile, and visible to boot and runtime services.
const VARIABLE_ATTRIBUTES: u32 = 7;
/// Where eliboot goes in an EFI System Partition (besides the fallback).
const BOOTLOADER_PATH: &str = "\\EFI\\eliOS\\eliboot.efi";

/// What the installer was asked to do.
struct Plan {
    disk: DiskInfo,
    erase: bool,
    full_name: String,
    user: String,
    password: String,
    hostname: String,
}

/// The files being installed.
struct BootFiles {
    bootloader: Vec<u8>,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
}

fn fail(message: &str) -> i32 {
    eli::eprintln!("installer: {message}");
    1
}

/// Asks a question; returns the answer, or `default` for an empty one.
/// `None` at end of input.
fn ask(lines: &mut Lines, question: &str, default: &str) -> Option<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    let answer = lines.next_line().ok()??;
    let answer = answer.trim();
    Some(if answer.is_empty() { default.into() } else { answer.into() })
}

/// The disk the running system was started from (it can't be installed to).
fn boot_disk() -> Option<String> {
    let source = fs::mounts().into_iter().find(|mount| mount.target == "/disk")?.source;
    let partition = source.strip_prefix("/dev/")?;
    storage::disks().into_iter().find(|disk| disk.partitions.iter().any(|p| p.name == partition)).map(|disk| disk.name)
}

fn describe(disk: &DiskInfo) -> String {
    let contents = match disk.partitions.len() {
        0 => String::from("empty"),
        1 => String::from("1 partition"),
        count => format!("{count} partitions"),
    };
    format!("{:<9} {:>7}  {:<24} ({contents})", disk.name, storage::human_size(disk.sectors * 512), disk.model)
}

/// The largest free stretch on a disk with a GPT and an EFI System
/// Partition, if any: (first, last) sectors.
fn space_alongside(disk: &DiskInfo) -> Option<(u64, u64)> {
    let device = Device::open(&format!("/dev/{}", disk.name)).ok()?;
    let table = Table::read(&device)?;
    table.partitions().iter().find(|(_, entry)| entry.type_guid == gpt::guid(gpt::ESP_TYPE))?;
    table.free_space().into_iter().max_by_key(|(first, last)| last - first)
}

pub fn main(arguments: &[String]) -> i32 {
    if users::euid() != 0 {
        return fail("run the installer as root (sudo installer)");
    }
    println!("\x1b[1meliOS installer\x1b[0m");
    println!("This copies eliOS onto a disk in this computer, so it starts without the");
    println!("disk it's running from now. Press Ctrl+C at any question to stop.\n");

    let boot_files = match read_boot_files() {
        Ok(files) => files,
        Err(message) => return fail(&message),
    };
    let plan = match make_plan(arguments) {
        Ok(Some(plan)) => plan,
        Ok(None) => return 1,
        Err(message) => return fail(&message),
    };
    match install(&plan, &boot_files) {
        Ok(()) => {
            println!("\n\x1b[1;32meliOS is installed on {}.\x1b[0m", plan.disk.name);
            println!("Take out the USB stick (or other installation disk) and type \x1b[1mreboot\x1b[0m.");
            println!("Then log in as \x1b[1m{}\x1b[0m; use sudo for administration.", plan.user);
            0
        }
        Err(message) => fail(&message),
    }
}

fn read_boot_files() -> Result<BootFiles, String> {
    let read = |path: &str| fs::read(&format!("{BOOT_FILES}/{path}"));
    match (read("BOOT/BOOTX64.EFI"), read("eliOS/kernel.elf"), read("eliOS/initrd.cpio")) {
        (Ok(bootloader), Ok(kernel), Ok(initrd)) => Ok(BootFiles { bootloader, kernel, initrd }),
        _ => Err(format!(
            "can't find eliOS's boot files in {BOOT_FILES}: is the disk eliOS started from mounted at /disk?"
        )),
    }
}

/// Works out what to do, from the options given (`--disk NAME`, `--erase`
/// or `--alongside`, `--user`, `--password`, `--full-name`, `--hostname`,
/// `--yes`) or by asking.
fn make_plan(arguments: &[String]) -> Result<Option<Plan>, String> {
    let mut options: BTreeMap<&str, String> = BTreeMap::new();
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--erase" | "--alongside" | "--yes" => {
                options.insert(argument.as_str(), String::new());
            }
            "--disk" | "--user" | "--password" | "--full-name" | "--hostname" => {
                let value = rest.next().ok_or(format!("{argument} needs a value"))?;
                options.insert(argument.as_str(), value.clone());
            }
            _ => return Err(format!("unknown option {argument}")),
        }
    }
    let mut lines = Lines::new(io::STDIN);
    let running_from = boot_disk();
    let candidates: Vec<DiskInfo> =
        storage::disks().into_iter().filter(|disk| Some(&disk.name) != running_from.as_ref()).collect();
    if candidates.is_empty() {
        return Err("there's no other disk to install to".into());
    }

    let disk = match options.get("--disk") {
        Some(name) => candidates.into_iter().find(|disk| disk.name == *name).ok_or(format!("no disk {name}"))?,
        None => {
            println!("Disks:");
            for (index, disk) in candidates.iter().enumerate() {
                println!("  {}) {}", index + 1, describe(disk));
            }
            if let Some(name) = &running_from {
                println!("  (eliOS is running from {name}, so it isn't listed)");
            }
            let Some(answer) = ask(&mut lines, "Install to which disk?", "1") else { return Ok(None) };
            let index = answer.parse::<usize>().ok().filter(|&i| (1..=candidates.len()).contains(&i));
            let index = index.ok_or("that's not one of the disks")?;
            candidates.into_iter().nth(index - 1).unwrap()
        }
    };
    if fs::mounts().iter().any(|mount| mount.source.starts_with(&format!("/dev/{}", disk.name))) {
        return Err(format!("{} is in use (mounted); unmount it first", disk.name));
    }
    if disk.sectors < MIN_ROOT_SECTORS + 300 * 2048 {
        return Err(format!("{} is too small for eliOS (it needs at least 800 MiB)", disk.name));
    }

    let alongside = space_alongside(&disk).filter(|(first, last)| last - first + 1 >= MIN_ROOT_SECTORS);
    let erase = if options.contains_key("--erase") {
        true
    } else if options.contains_key("--alongside") {
        if alongside.is_none() {
            return Err(format!(
                "{} has no EFI System Partition with at least 512 MiB of free space after it",
                disk.name
            ));
        }
        false
    } else {
        println!("\nHow should eliOS use {}?", disk.name);
        println!("  1) Erase the whole disk and use all of it for eliOS");
        match alongside {
            Some((first, last)) => println!(
                "  2) Install alongside what's on it now, in its {} of free space",
                storage::human_size((last - first + 1) * 512)
            ),
            None if !disk.partitions.is_empty() => {
                println!("     (Installing alongside needs an EFI System Partition and 512 MiB of free");
                println!("     space; shrink another system's partition first to make room.)");
            }
            None => {}
        }
        let Some(answer) = ask(&mut lines, "Choice", "1") else { return Ok(None) };
        match answer.as_str() {
            "1" => true,
            "2" if alongside.is_some() => false,
            _ => return Err("that's not one of the choices".into()),
        }
    };

    let full_name = match options.get("--full-name") {
        Some(name) => name.clone(),
        None if options.contains_key("--user") => String::new(),
        None => {
            println!("\nYour account (it can run administrative commands with sudo):");
            let Some(name) = ask(&mut lines, "  Your full name (optional)", "") else { return Ok(None) };
            name
        }
    };
    let user = match options.get("--user") {
        Some(user) => user.clone(),
        None => {
            let suggestion = full_name.split_whitespace().next().unwrap_or("").to_lowercase();
            let Some(user) = ask(&mut lines, "  User name", &suggestion) else { return Ok(None) };
            user
        }
    };
    if !users::valid_name(&user) || user == "root" {
        return Err(format!("{user:?} can't be a user name: use lowercase letters, digits, - and _"));
    }
    let password = match options.get("--password") {
        Some(password) => password.clone(),
        None => match crate::accounts::ask_new_password("installer") {
            Some(password) if !password.is_empty() => password,
            Some(_) => return Err("an administrator needs a password".into()),
            None => return Ok(None),
        },
    };
    let hostname = match options.get("--hostname") {
        Some(hostname) => hostname.clone(),
        None => {
            let current = fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
            let Some(hostname) = ask(&mut lines, "  Computer name", &current) else { return Ok(None) };
            hostname
        }
    };
    if hostname.is_empty() || hostname.len() > 63 || !hostname.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("{hostname:?} can't be a computer name: use letters, digits and -"));
    }

    println!("\n\x1b[1mReady to install:\x1b[0m");
    if erase {
        println!("  \x1b[1;31m{}: everything on it will be ERASED.\x1b[0m", describe(&disk));
    } else {
        println!("  {}: eliOS goes in its free space; what's there is kept.", describe(&disk));
    }
    println!("  Account {user}, computer name {hostname}.");
    if !options.contains_key("--yes") {
        let Some(answer) = ask(&mut lines, &format!("Type {} to go ahead", disk.name), "") else { return Ok(None) };
        if answer != disk.name {
            println!("Nothing was changed.");
            return Ok(None);
        }
    }
    Ok(Some(Plan { disk, erase, full_name, user, password, hostname }))
}

/// Shows one step's progress.
fn step(number: u32, text: &str) {
    println!("[{number}/6] {text}");
}

/// Where the new system goes: (EFI System Partition, root partition), each
/// as (number, entry).
struct Layout {
    esp: (u32, Entry),
    root: (u32, Entry),
}

fn install(plan: &Plan, files: &BootFiles) -> Result<(), String> {
    let disk_path = format!("/dev/{}", plan.disk.name);
    let disk = Device::open(&disk_path).map_err(|e| format!("{disk_path}: {e}"))?;

    step(1, "Partitioning");
    let layout = partition(plan, &disk).map_err(|e| format!("partitioning: {e}"))?;
    // So the new partitions get device files (and nothing stale lingers).
    if let Ok(file) = fs::File::open(&disk_path) {
        let _ = file.reread_partitions();
    }
    let slice = |entry: &Entry| Device::slice(&disk_path, entry.first, entry.sectors());
    let mut esp = slice(&layout.esp.1).map_err(|e| e.to_string())?;
    let mut root = slice(&layout.root.1).map_err(|e| e.to_string())?;

    step(2, "Formatting");
    if plan.erase {
        storage::format(&mut esp, "fat", ESP_LABEL).map_err(|e| format!("formatting the boot partition: {e}"))?;
    }
    storage::format(&mut root, "ext2", ROOT_LABEL).map_err(|e| format!("formatting the root partition: {e}"))?;

    step(3, "Copying the system");
    let mut volume =
        ext2::FileSystem::mount(root, time::unix_time, false).map_err(|e| format!("the new root: {e:?}"))?;
    let count = unpack(&mut volume, &files.initrd).map_err(|e| format!("copying the system: {e}"))?;
    println!("      {count} files");

    step(4, "Setting up your account");
    set_up_accounts(&mut volume, plan).map_err(|e| format!("setting up the account: {e}"))?;
    volume.unmount().map_err(|e| format!("finishing the root partition: {e:?}"))?;
    drop(volume);

    step(5, "Installing the boot loader");
    let root_guid = gpt::guid_text(&layout.root.1.guid);
    write_boot_partition(&mut esp, files, &root_guid, plan.erase).map_err(|e| format!("the boot partition: {e}"))?;
    fs::sync();

    step(6, "Adding a boot entry to the firmware");
    match add_boot_entry(layout.esp.0, &layout.esp.1) {
        Ok(number) => println!("      Boot{number:04X} \"eliOS\", first in the boot order"),
        Err(problem) if plan.erase => {
            println!("      skipped ({problem}); the disk still starts through the fallback boot path")
        }
        Err(problem) => {
            return Err(format!(
                "the firmware's boot entry couldn't be added ({problem}); add one for {BOOTLOADER_PATH} in the \
                 firmware's settings"
            ));
        }
    }
    Ok(())
}

/// Makes or extends the partition table.
fn partition(plan: &Plan, disk: &Device) -> eli::Result<Layout> {
    let sectors = disk.sectors();
    let root_entry = |first: u64, last: u64| Entry {
        type_guid: gpt::guid(gpt::ELIOS_ROOT_TYPE),
        guid: gpt::random_guid(),
        first,
        last,
        attributes: 0,
        name: "eliOS".into(),
    };
    if plan.erase {
        // Clear what was at both ends (old tables, old file systems'
        // signatures) first.
        let zeros = vec![0u8; 2048 * 512];
        disk.write(0, &zeros)?;
        disk.write(sectors - 2048, &zeros)?;
        let mut table = Table::new(sectors);
        let esp_sectors = if sectors >= 4 << 21 { 256 * 2048 } else { 128 * 2048 };
        let esp = Entry {
            type_guid: gpt::guid(gpt::ESP_TYPE),
            guid: gpt::random_guid(),
            first: gpt::ALIGNMENT,
            last: gpt::ALIGNMENT + esp_sectors - 1,
            attributes: 0,
            name: "EFI System".into(),
        };
        let root = root_entry(esp.last + 1, table.last_usable);
        let esp_number = table.add(esp.clone()).ok_or(eli::Error::INVALID)?;
        let root_number = table.add(root.clone()).ok_or(eli::Error::INVALID)?;
        table.write(disk, true)?;
        return Ok(Layout { esp: (esp_number, esp), root: (root_number, root) });
    }
    let mut table = Table::read(disk).ok_or(eli::Error::INVALID)?;
    let esp = table
        .partitions()
        .into_iter()
        .find(|(_, entry)| entry.type_guid == gpt::guid(gpt::ESP_TYPE))
        .map(|(number, entry)| (number, entry.clone()))
        .ok_or(eli::Error::NOT_FOUND)?;
    let (first, last) =
        table.free_space().into_iter().max_by_key(|(first, last)| last - first).ok_or(eli::Error::INVALID)?;
    let root = root_entry(first, last);
    let root_number = table.add(root.clone()).ok_or(eli::Error::INVALID)?;
    table.write(disk, false)?;
    Ok(Layout { esp, root: (root_number, root) })
}

/// One file of a cpio ("newc") archive.
struct ArchiveEntry<'a> {
    path: &'a str,
    mode: u32,
    uid: u32,
    gid: u32,
    modified: u32,
    data: &'a [u8],
}

/// The entries of a cpio archive (the initrd), in order.
fn archive_entries(archive: &[u8]) -> Result<Vec<ArchiveEntry<'_>>, String> {
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let header = archive.get(offset..offset + 110).ok_or("the initrd is cut short")?;
        if &header[..6] != b"070701" {
            return Err("the initrd is damaged".into());
        }
        let field = |index: usize| -> Result<usize, String> {
            let text = core::str::from_utf8(&header[6 + index * 8..14 + index * 8]).map_err(|e| e.to_string())?;
            usize::from_str_radix(text, 16).map_err(|e| e.to_string())
        };
        let (mode, uid, gid, modified, size, name_size) =
            (field(1)?, field(2)?, field(3)?, field(5)?, field(6)?, field(11)?);
        let name_at = offset + 110;
        let name = archive.get(name_at..name_at + name_size - 1).ok_or("the initrd is cut short")?;
        let name = core::str::from_utf8(name).map_err(|_| "a file name in the initrd isn't UTF-8")?;
        let data_at = (name_at + name_size).next_multiple_of(4);
        let data = archive.get(data_at..data_at + size).ok_or("the initrd is cut short")?;
        offset = (data_at + size).next_multiple_of(4);
        if name == "TRAILER!!!" {
            return Ok(entries);
        }
        entries.push(ArchiveEntry {
            path: name.trim_start_matches("./"),
            mode: mode as u32,
            uid: uid as u32,
            gid: gid as u32,
            modified: modified as u32,
            data,
        });
    }
}

/// Copies the initrd's files into the new root. Returns how many.
fn unpack(volume: &mut ext2::FileSystem<Device>, initrd: &[u8]) -> Result<usize, String> {
    let mut directories: BTreeMap<String, u32> = BTreeMap::new();
    directories.insert(String::new(), ext2::ROOT);
    let mut count = 0;
    for entry in archive_entries(initrd)? {
        if entry.path.is_empty() || entry.path == "." || entry.path == "etc/autologin" {
            continue;
        }
        let (parent, name) = entry.path.rsplit_once('/').unwrap_or(("", entry.path));
        let parent = *directories.get(parent).ok_or(format!("{}: its directory comes later", entry.path))?;
        let owner = ext2::Owner { uid: entry.uid, gid: entry.gid };
        let permissions = (entry.mode & 0o7777) as u16;
        let problem = |e: ext2::Error| format!("{}: {e:?}", entry.path);
        let number = match entry.mode & 0o170000 {
            0o040000 => {
                let number =
                    volume.create(parent, name, ext2::FileType::Directory, permissions, owner).map_err(problem)?;
                directories.insert(entry.path.to_string(), number);
                number
            }
            0o120000 => {
                let target = core::str::from_utf8(entry.data).map_err(|_| format!("{}: bad link", entry.path))?;
                volume.symlink(parent, name, target, owner).map_err(problem)?
            }
            0o100000 => {
                let number =
                    volume.create(parent, name, ext2::FileType::Regular, permissions, owner).map_err(problem)?;
                volume.write(number, 0, entry.data).map_err(problem)?;
                number
            }
            _ => continue,
        };
        volume.set_attributes(number, None, None, Some(entry.modified)).map_err(problem)?;
        count += 1;
    }
    // Where /dev, /proc, /sys and the rest get mounted.
    let root_owner = ext2::Owner { uid: 0, gid: 0 };
    for (name, permissions) in [
        ("dev", 0o755),
        ("proc", 0o555),
        ("sys", 0o555),
        ("tmp", 0o1777),
        ("run", 0o755),
        ("disk", 0o755),
        ("mnt", 0o755),
        ("home", 0o755),
        ("root", 0o700),
    ] {
        if volume.lookup(ext2::ROOT, name).is_err() {
            volume
                .create(ext2::ROOT, name, ext2::FileType::Directory, permissions, root_owner)
                .map_err(|e| format!("/{name}: {e:?}"))?;
        }
    }
    Ok(count)
}

/// Finds the inode at a path in the new root.
fn find(volume: &mut ext2::FileSystem<Device>, path: &str) -> Result<u32, String> {
    let mut number = ext2::ROOT;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        number = volume.lookup(number, name).map_err(|e| format!("{path}: {e:?}"))?;
    }
    Ok(number)
}

fn read_text(volume: &mut ext2::FileSystem<Device>, path: &str) -> Result<String, String> {
    let number = find(volume, path)?;
    let size = volume.inode(number).map_err(|e| format!("{path}: {e:?}"))?.size as usize;
    let mut data = vec![0; size];
    volume.read(number, 0, &mut data).map_err(|e| format!("{path}: {e:?}"))?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Replaces a file's contents in the new root (making it if needed).
fn write_text(volume: &mut ext2::FileSystem<Device>, path: &str, text: &str, permissions: u16) -> Result<(), String> {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = find(volume, directory)?;
    let number = match volume.lookup(parent, name) {
        Ok(number) => number,
        Err(_) => volume
            .create(parent, name, ext2::FileType::Regular, permissions, ext2::Owner { uid: 0, gid: 0 })
            .map_err(|e| format!("{path}: {e:?}"))?,
    };
    volume.truncate(number, 0).map_err(|e| format!("{path}: {e:?}"))?;
    volume.write(number, 0, text.as_bytes()).map_err(|e| format!("{path}: {e:?}"))?;
    volume.set_attributes(number, Some(permissions), None, None).map_err(|e| format!("{path}: {e:?}"))?;
    Ok(())
}

/// Copies a directory tree within the new root, giving it to `owner`.
fn copy_tree(volume: &mut ext2::FileSystem<Device>, from: u32, to: u32, owner: ext2::Owner) -> Result<(), String> {
    for entry in volume.list(from).map_err(|e| format!("{e:?}"))? {
        let inode = volume.inode(entry.inode).map_err(|e| format!("{e:?}"))?;
        let problem = |e: ext2::Error| format!("{}: {e:?}", entry.name);
        match entry.kind {
            ext2::FileType::Directory => {
                let made = volume.create(to, &entry.name, entry.kind, inode.permissions(), owner).map_err(problem)?;
                copy_tree(volume, entry.inode, made, owner)?;
            }
            ext2::FileType::Symlink => {
                let target = volume.read_link(entry.inode).map_err(problem)?;
                volume.symlink(to, &entry.name, &target, owner).map_err(problem)?;
            }
            _ => {
                let mut data = vec![0; inode.size as usize];
                volume.read(entry.inode, 0, &mut data).map_err(problem)?;
                let made = volume.create(to, &entry.name, entry.kind, inode.permissions(), owner).map_err(problem)?;
                volume.write(made, 0, &data).map_err(problem)?;
            }
        }
    }
    Ok(())
}

/// The new system's accounts: the user (an administrator, with a home copied
/// from /etc/skel), root without a password, and the computer's name.
fn set_up_accounts(volume: &mut ext2::FileSystem<Device>, plan: &Plan) -> Result<(), String> {
    let uid = users::FIRST_USER_ID;
    let owner = ext2::Owner { uid, gid: uid };
    let mut passwd = read_text(volume, "/etc/passwd")?;
    passwd.push_str(&format!("{}:x:{uid}:{uid}:{}:/home/{}:/bin/sh\n", plan.user, plan.full_name, plan.user));
    write_text(volume, "/etc/passwd", &passwd, 0o644)?;

    let mut group = String::new();
    for line in read_text(volume, "/etc/group")?.lines() {
        let line = if line.starts_with(&format!("{}:", users::ADMIN_GROUP)) {
            if line.ends_with(':') { format!("{line}{}", plan.user) } else { format!("{line},{}", plan.user) }
        } else {
            line.to_string()
        };
        group.push_str(&line);
        group.push('\n');
    }
    group.push_str(&format!("{}:x:{uid}:\n", plan.user));
    write_text(volume, "/etc/group", &group, 0o644)?;

    let days = time::unix_time() / 86_400;
    let mut shadow = String::new();
    for line in read_text(volume, "/etc/shadow")?.lines() {
        // Root can't be logged into: administrators use sudo.
        match line.strip_prefix("root:") {
            Some(rest) => shadow.push_str(&format!("root:!:{}\n", rest.split_once(':').map_or("", |(_, rest)| rest))),
            None => {
                shadow.push_str(line);
                shadow.push('\n');
            }
        }
    }
    let hash = eli::crypt::hash_password(&plan.password);
    shadow.push_str(&format!("{}:{hash}:{days}:0:99999:7:::\n", plan.user));
    write_text(volume, "/etc/shadow", &shadow, 0o600)?;
    write_text(volume, "/etc/hostname", &format!("{}\n", plan.hostname), 0o644)?;

    let home = find(volume, "/home")?;
    let user_home = volume
        .create(home, &plan.user, ext2::FileType::Directory, 0o750, owner)
        .map_err(|e| format!("/home/{}: {e:?}", plan.user))?;
    let skeleton = find(volume, "/etc/skel")?;
    copy_tree(volume, skeleton, user_home, owner)
}

/// Writes a file into the boot partition, making its directories.
fn put_file(volume: &mut fat::FileSystem<&mut Device>, path: &str, data: &[u8]) -> Result<(), String> {
    let mut directory = volume.root();
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let (name, parents) = components.split_last().ok_or("empty path")?;
    for parent in parents {
        directory = match volume.lookup(&directory, parent) {
            Ok(entry) => entry,
            Err(fat::Error::NotFound) => {
                volume.create(&directory, parent, true).map_err(|e| format!("{parent}: {e:?}"))?
            }
            Err(e) => return Err(format!("{parent}: {e:?}")),
        };
    }
    if let Ok(old) = volume.lookup(&directory, name) {
        volume.remove(&old).map_err(|e| format!("{path}: {e:?}"))?;
    }
    let mut file = volume.create(&directory, name, false).map_err(|e| format!("{path}: {e:?}"))?;
    volume.write(&mut file, 0, data).map_err(|e| format!("{path}: {e:?}"))?;
    Ok(())
}

/// Puts eliboot, the kernel, the initrd and the command line naming the
/// root into the EFI System Partition.
fn write_boot_partition(esp: &mut Device, files: &BootFiles, root_guid: &str, fallback: bool) -> Result<(), String> {
    let mut volume = fat::FileSystem::mount(esp, time::unix_time).map_err(|e| format!("{e:?}"))?;
    let needed = (files.bootloader.len() * 2 + files.kernel.len() + files.initrd.len()) as u64;
    if volume.free_bytes().map_err(|e| format!("{e:?}"))? < needed {
        return Err(format!("it needs {} free", storage::human_size(needed)));
    }
    put_file(&mut volume, "EFI/eliOS/eliboot.efi", &files.bootloader)?;
    put_file(&mut volume, "EFI/eliOS/kernel.elf", &files.kernel)?;
    put_file(&mut volume, "EFI/eliOS/initrd.cpio", &files.initrd)?;
    put_file(&mut volume, "EFI/eliOS/cmdline.txt", format!("root=PARTUUID={root_guid}\n").as_bytes())?;
    if fallback {
        put_file(&mut volume, "EFI/BOOT/BOOTX64.EFI", &files.bootloader)?;
    }
    Ok(())
}

/// A firmware variable's file in efivarfs.
fn variable_path(name: &str) -> String {
    format!("{EFIVARS}/{name}-{GLOBAL_VARIABLES}")
}

/// UCS-2, NUL-terminated.
fn ucs2(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(core::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

/// An `EFI_LOAD_OPTION` that starts eliboot from the EFI System Partition:
/// active, described as "eliOS", with a device path of the partition (by
/// its GPT GUID and position) and the file.
fn load_option(number: u32, esp: &Entry) -> Vec<u8> {
    let mut path = Vec::new();
    // Media device path, hard drive: partition number, start, size, GPT
    // signature (the partition's GUID), GPT format, GUID signature type.
    path.extend_from_slice(&[4, 1, 42, 0]);
    path.extend_from_slice(&number.to_le_bytes());
    path.extend_from_slice(&esp.first.to_le_bytes());
    path.extend_from_slice(&esp.sectors().to_le_bytes());
    path.extend_from_slice(&esp.guid);
    path.extend_from_slice(&[2, 2]);
    // Media device path, file path.
    let file = ucs2(BOOTLOADER_PATH);
    path.extend_from_slice(&[4, 4]);
    path.extend_from_slice(&((4 + file.len()) as u16).to_le_bytes());
    path.extend_from_slice(&file);
    // End of the device path.
    path.extend_from_slice(&[0x7F, 0xFF, 4, 0]);

    let mut option = Vec::new();
    option.extend_from_slice(&1u32.to_le_bytes());
    option.extend_from_slice(&(path.len() as u16).to_le_bytes());
    option.extend_from_slice(&ucs2("eliOS"));
    option.extend_from_slice(&path);
    option
}

/// If a load option is one of eliOS's ("eliOS", starting eliboot), the GPT
/// GUID of the partition it starts from.
fn elios_entry_partition(option: &[u8]) -> Option<[u8; 16]> {
    let path_length = usize::from(u16::from_le_bytes(option.get(4..6)?.try_into().ok()?));
    let description = ucs2("eliOS");
    if option.get(6..6 + description.len())? != description.as_slice() {
        return None;
    }
    let path = option.get(6 + description.len()..6 + description.len() + path_length)?;
    // The first node is the partition (media, hard drive, GUID signature).
    if path.len() < 42 || path[0] != 4 || path[1] != 1 || path[41] != 2 {
        return None;
    }
    path[24..40].try_into().ok()
}

/// Adds (or replaces) the "eliOS" boot entry and puts it first in the boot
/// order. Returns its number.
fn add_boot_entry(esp_number: u32, esp: &Entry) -> Result<u16, String> {
    if !fs::exists(EFIVARS) {
        return Err("the firmware's variables aren't available".into());
    }
    let option = load_option(esp_number, esp);
    let entries: Vec<u16> = fs::read_dir(EFIVARS)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|entry| {
            let name = entry.name.strip_suffix(&format!("-{GLOBAL_VARIABLES}"))?;
            let hex = name.strip_prefix("Boot")?;
            (hex.len() == 4).then(|| u16::from_str_radix(hex, 16).ok()).flatten()
        })
        .collect();
    // Entries an earlier installation left, pointing at a partition that's
    // gone (erased, say), are removed.
    let partitions: Vec<String> = storage::disks()
        .iter()
        .flat_map(|disk| disk.partitions.iter().map(move |p| format!("/sys/block/{}/{}/uuid", disk.name, p.name)))
        .filter_map(|path| fs::read_to_string(&path).ok())
        .map(|text| text.trim().to_string())
        .collect();
    for &number in &entries {
        let path = variable_path(&format!("Boot{number:04X}"));
        let Ok(contents) = fs::read(&path) else { continue };
        if let Some(partition) = elios_entry_partition(contents.get(4..).unwrap_or(&[]))
            && !partitions.contains(&gpt::guid_text(&partition))
        {
            let _ = fs::remove_file(&path);
        }
    }
    let entries: Vec<u16> =
        entries.into_iter().filter(|&n| fs::exists(&variable_path(&format!("Boot{n:04X}")))).collect();
    // Reuse an entry for this same partition and file (from an earlier
    // installation), or take the first free number.
    let existing = entries.iter().copied().find(|&number| {
        fs::read(&variable_path(&format!("Boot{number:04X}"))).is_ok_and(|contents| contents.get(4..) == Some(&option))
    });
    let number = existing.or_else(|| (0..=0xFFFF).find(|number| !entries.contains(number))).ok_or("no free entry")?;
    let mut contents = VARIABLE_ATTRIBUTES.to_le_bytes().to_vec();
    contents.extend_from_slice(&option);
    fs::write(&variable_path(&format!("Boot{number:04X}")), &contents).map_err(|e| format!("Boot{number:04X}: {e}"))?;

    let mut order: Vec<u16> = fs::read(&variable_path("BootOrder"))
        .ok()
        .map(|contents| {
            contents.get(4..).unwrap_or(&[]).chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect()
        })
        .unwrap_or_default();
    order.retain(|&entry| entry != number);
    order.insert(0, number);
    let mut contents = VARIABLE_ATTRIBUTES.to_le_bytes().to_vec();
    contents.extend(order.iter().flat_map(|entry| entry.to_le_bytes()));
    fs::write(&variable_path("BootOrder"), &contents).map_err(|e| format!("BootOrder: {e}"))?;
    Ok(number)
}
