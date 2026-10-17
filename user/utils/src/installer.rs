//! `installer`: puts eliOS on a disk, so the computer starts it without the
//! USB stick (or live disk) it's running from.
//!
//! An installed system is two partitions: an EFI System Partition (FAT32)
//! with eliboot, the kernel and the system image (the initrd), and an ext2
//! *data* partition with everything that's yours: /etc/system.conf, the
//! passwords, homes, installed packages and the system's generations. The
//! kernel finds it through `data=PARTUUID=...` in `\EFI\eliOS\cmdline.txt`.
//! The system itself isn't copied anywhere: it starts fresh from the image
//! at every boot, exactly as the live system does, so it can't drift or
//! break. Two ways to get there:
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
//! The first account is set up as an administrator (it can use sudo); root
//! gets no password, so it can't be logged into directly. Packages added to
//! the live system come along.

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
const DATA_LABEL: &str = "eliOS data";
/// The smallest data partition worth installing to.
const MIN_DATA_SECTORS: u64 = 512 * 2048;
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
    if disk.sectors < MIN_DATA_SECTORS + 300 * 2048 {
        return Err(format!("{} is too small for eliOS (it needs at least 800 MiB)", disk.name));
    }

    let alongside = space_alongside(&disk).filter(|(first, last)| last - first + 1 >= MIN_DATA_SECTORS);
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

/// Where the new system goes: (EFI System Partition, data partition), each
/// as (number, entry).
struct Layout {
    esp: (u32, Entry),
    data: (u32, Entry),
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
    let mut data = slice(&layout.data.1).map_err(|e| e.to_string())?;

    step(2, "Formatting");
    if plan.erase {
        storage::format(&mut esp, "fat", ESP_LABEL).map_err(|e| format!("formatting the boot partition: {e}"))?;
    }
    storage::format(&mut data, "ext2", DATA_LABEL).map_err(|e| format!("formatting the data partition: {e}"))?;

    step(3, "Setting up your account and settings");
    let mut volume =
        ext2::FileSystem::mount(data, time::unix_time, false).map_err(|e| format!("the data partition: {e:?}"))?;
    let packages = set_up_data(&mut volume, plan).map_err(|e| format!("setting up: {e}"))?;

    step(4, "Copying packages added to the live system");
    for id in &packages {
        let pkg = find(&mut volume, "/pkg")?;
        copy_into(&mut volume, &format!("/data/pkg/{id}"), pkg, id).map_err(|e| format!("copying {id}: {e}"))?;
    }
    println!("      {}", if packages.is_empty() { "none".to_string() } else { packages.join(", ") });
    volume.unmount().map_err(|e| format!("finishing the data partition: {e:?}"))?;
    drop(volume);

    step(5, "Installing the boot loader");
    let data_guid = gpt::guid_text(&layout.data.1.guid);
    write_boot_partition(&mut esp, files, &data_guid, plan.erase).map_err(|e| format!("the boot partition: {e}"))?;
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
    let data_entry = |first: u64, last: u64| Entry {
        type_guid: gpt::guid(gpt::ELIOS_DATA_TYPE),
        guid: gpt::random_guid(),
        first,
        last,
        attributes: 0,
        name: "eliOS data".into(),
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
        let data = data_entry(esp.last + 1, table.last_usable);
        let esp_number = table.add(esp.clone()).ok_or(eli::Error::INVALID)?;
        let data_number = table.add(data.clone()).ok_or(eli::Error::INVALID)?;
        table.write(disk, true)?;
        return Ok(Layout { esp: (esp_number, esp), data: (data_number, data) });
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
    let data = data_entry(first, last);
    let data_number = table.add(data.clone()).ok_or(eli::Error::INVALID)?;
    table.write(disk, false)?;
    Ok(Layout { esp, data: (data_number, data) })
}

/// Finds the inode at a path in the new volume.
fn find(volume: &mut ext2::FileSystem<Device>, path: &str) -> Result<u32, String> {
    let mut number = ext2::ROOT;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        number = volume.lookup(number, name).map_err(|e| format!("{path}: {e:?}"))?;
    }
    Ok(number)
}

/// A new file in the new volume, owned by `owner`.
fn write_file(
    volume: &mut ext2::FileSystem<Device>,
    path: &str,
    text: &[u8],
    permissions: u16,
    owner: ext2::Owner,
) -> Result<(), String> {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = find(volume, directory)?;
    let number = volume
        .create(parent, name, ext2::FileType::Regular, permissions, owner)
        .map_err(|e| format!("{path}: {e:?}"))?;
    volume.write(number, 0, text).map_err(|e| format!("{path}: {e:?}"))?;
    Ok(())
}

/// Copies a file or directory tree of the running system into the new
/// volume (as `name` in directory `parent`), keeping permissions, owners,
/// times and symbolic links.
fn copy_into(volume: &mut ext2::FileSystem<Device>, from: &str, parent: u32, name: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(from).map_err(|e| format!("{from}: {e}"))?;
    let owner = ext2::Owner { uid: metadata.uid, gid: metadata.gid };
    let permissions = metadata.permissions() as u16;
    let problem = |e: ext2::Error| format!("{from}: {e:?}");
    let number = match metadata.kind {
        fs::FileType::Directory => {
            let number = volume.create(parent, name, ext2::FileType::Directory, permissions, owner).map_err(problem)?;
            for entry in fs::read_dir(from).map_err(|e| format!("{from}: {e}"))? {
                copy_into(volume, &format!("{from}/{}", entry.name), number, &entry.name)?;
            }
            number
        }
        fs::FileType::Symlink => {
            let target = fs::read_link(from).map_err(|e| format!("{from}: {e}"))?;
            volume.symlink(parent, name, &target, owner).map_err(problem)?
        }
        fs::FileType::File => {
            let data = fs::read(from).map_err(|e| format!("{from}: {e}"))?;
            let number = volume.create(parent, name, ext2::FileType::Regular, permissions, owner).map_err(problem)?;
            volume.write(number, 0, &data).map_err(problem)?;
            number
        }
        _ => return Ok(()),
    };
    volume.set_attributes(number, None, None, Some(metadata.modified.max(0) as u32)).map_err(problem)?;
    Ok(())
}

/// Fills the new data partition: the configuration (this computer's name,
/// the account, and the packages added to the live system), passwords,
/// the account's home, and the first generation. Returns the packages
/// (`name-version`) to copy.
fn set_up_data(volume: &mut ext2::FileSystem<Device>, plan: &Plan) -> Result<Vec<String>, String> {
    let root = ext2::Owner { uid: 0, gid: 0 };
    let user = ext2::Owner { uid: users::FIRST_USER_ID, gid: users::FIRST_USER_ID };
    for (name, permissions) in
        [("etc", 0o755), ("home", 0o755), ("pkg", 0o755), ("generations", 0o755), ("cache", 0o755)]
    {
        volume
            .create(ext2::ROOT, name, ext2::FileType::Directory, permissions, root)
            .map_err(|e| format!("/{name}: {e:?}"))?;
    }

    // The live system's configuration, as a starting point: its packages
    // come along, its accounts don't.
    let live = fs::read_to_string("/etc/system.conf")
        .or_else(|_| fs::read_to_string("/etc/eli/system.conf"))
        .unwrap_or_default();
    let live = crate::manager::without_users(&live);
    let mut config = crate::manager::set_setting(&live, "hostname", &plan.hostname);
    let account = crate::manager::ConfiguredUser {
        name: plan.user.clone(),
        full_name: plan.full_name.clone(),
        admin: true,
        shell: "/bin/sh".into(),
    };
    config = crate::manager::add_user_block(&config, &account);
    write_file(volume, "/etc/system.conf", config.as_bytes(), 0o644, root)?;

    // Root's password is locked: administration goes through sudo.
    let days = time::unix_time() / 86_400;
    let hash = eli::crypt::hash_password(&plan.password);
    let shadow =
        format!("root:!:{days}:0:99999:7:::\nnobody:*:{days}:0:99999:7:::\n{}:{hash}:{days}:0:99999:7:::\n", plan.user);
    write_file(volume, "/etc/shadow", shadow.as_bytes(), 0o600, root)?;
    write_file(volume, "/etc/ids", format!("{} {}\n", plan.user, users::FIRST_USER_ID).as_bytes(), 0o644, root)?;

    let home = find(volume, "/home")?;
    let skeleton_home = volume
        .create(home, &plan.user, ext2::FileType::Directory, 0o750, user)
        .map_err(|e| format!("/home/{}: {e:?}", plan.user))?;
    for entry in fs::read_dir("/etc/skel").unwrap_or_default() {
        copy_into(volume, &format!("/etc/skel/{}", entry.name), skeleton_home, &entry.name)?;
    }
    // Copied as root's: they're the user's.
    for entry in volume.list(skeleton_home).map_err(|e| format!("{e:?}"))? {
        volume.set_attributes(entry.inode, None, Some(user), None).map_err(|e| format!("{e:?}"))?;
    }

    // The first generation: this configuration and the live system's
    // packages.
    let packages: Vec<String> = crate::manager::running_packages();
    let generations = find(volume, "/generations")?;
    volume
        .create(generations, "1", ext2::FileType::Directory, 0o755, root)
        .map_err(|e| format!("/generations/1: {e:?}"))?;
    write_file(volume, "/generations/1/system.conf", config.as_bytes(), 0o644, root)?;
    write_file(volume, "/generations/1/packages", (packages.join("\n") + "\n").as_bytes(), 0o644, root)?;
    let info = format!("created = {}\nreason = installed\n", time::unix_time());
    write_file(volume, "/generations/1/info", info.as_bytes(), 0o644, root)?;
    volume.symlink(generations, "current", "1", root).map_err(|e| format!("current: {e:?}"))?;
    Ok(packages)
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
fn write_boot_partition(esp: &mut Device, files: &BootFiles, data_guid: &str, fallback: bool) -> Result<(), String> {
    let mut volume = fat::FileSystem::mount(esp, time::unix_time).map_err(|e| format!("{e:?}"))?;
    let needed = (files.bootloader.len() * 2 + files.kernel.len() + files.initrd.len()) as u64;
    if volume.free_bytes().map_err(|e| format!("{e:?}"))? < needed {
        return Err(format!("it needs {} free", storage::human_size(needed)));
    }
    put_file(&mut volume, "EFI/eliOS/eliboot.efi", &files.bootloader)?;
    put_file(&mut volume, "EFI/eliOS/kernel.elf", &files.kernel)?;
    put_file(&mut volume, "EFI/eliOS/initrd.cpio", &files.initrd)?;
    put_file(&mut volume, "EFI/eliOS/cmdline.txt", format!("data=PARTUUID={data_guid}\n").as_bytes())?;
    let date = time::DateTime::from_unix(time::unix_time());
    let menu = format!(
        "1\t{:04}-{:02}-{:02} {:02}:{:02}  installed (current)\n",
        date.year, date.month, date.day, date.hour, date.minute
    );
    put_file(&mut volume, "EFI/eliOS/generations.txt", menu.as_bytes())?;
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
