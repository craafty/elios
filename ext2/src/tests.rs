//! Host tests. Where the host has e2fsprogs, volumes are also checked with
//! Linux's `e2fsck`, which knows ext2's rules far better than these tests.

extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use std::process::Command;

use super::*;

const OWNER: Owner = Owner { uid: 1000, gid: 1000 };

fn clock() -> u64 {
    1_791_590_400 // 2026-10-10
}

/// A disk in memory.
struct MemoryDevice {
    data: Vec<u8>,
}

impl BlockDevice for &mut MemoryDevice {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> Result<()> {
        let start = sector as usize * SECTOR_SIZE;
        buffer.copy_from_slice(self.data.get(start..start + buffer.len()).ok_or(Error::Io)?);
        Ok(())
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> Result<()> {
        let start = sector as usize * SECTOR_SIZE;
        self.data.get_mut(start..start + data.len()).ok_or(Error::Io)?.copy_from_slice(data);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        (self.data.len() / SECTOR_SIZE) as u64
    }
}

fn device(megabytes: usize) -> MemoryDevice {
    MemoryDevice { data: vec![0; megabytes << 20] }
}

fn format(device: &mut MemoryDevice, block_size: Option<usize>) {
    let options = FormatOptions { label: "test", uuid: [7; 16], time: clock() as u32, block_size };
    FileSystem::format(&mut &mut *device, &options).unwrap();
}

/// Runs `e2fsck -fn` on the image, if e2fsck is installed; panics with its
/// report if it finds anything wrong.
fn fsck(device: &MemoryDevice, name: &str) {
    let path = std::env::temp_dir().join(std::format!("eli-ext2-{name}-{}.img", std::process::id()));
    std::fs::write(&path, &device.data).unwrap();
    let output = Command::new("e2fsck").arg("-fn").arg(&path).output();
    let _ = std::fs::remove_file(&path);
    let Ok(output) = output else { return };
    let report = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "e2fsck found problems in {name}:\n{report}");
}

/// The e2fsck check really catches damage (if e2fsck is installed).
#[test]
fn damage_is_noticed() {
    if Command::new("e2fsck").arg("-V").output().is_err() {
        return;
    }
    let mut disk = device(8);
    format(&mut disk, Some(1024));
    // Group 0's free block count, in the descriptor table at block 2.
    disk.data[2048 + 12] ^= 1;
    let result = std::panic::catch_unwind(move || fsck(&disk, "damaged"));
    assert!(result.is_err(), "e2fsck didn't notice a wrong free block count");
}

#[test]
fn a_fresh_volume_is_clean() {
    for (size, block_size) in [(8, None), (64, Some(1024)), (64, Some(2048)), (600, None)] {
        let mut disk = device(size);
        format(&mut disk, block_size);
        let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
        let names: Vec<String> = fs.list(ROOT).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["lost+found"]);
        fs.unmount().unwrap();
        drop(fs);
        fsck(&disk, &std::format!("fresh-{size}"));
    }
}

#[test]
fn files_round_trip() {
    let mut disk = device(16);
    format(&mut disk, None);
    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    let file = fs.create(ROOT, "hello.txt", FileType::Regular, 0o644, OWNER).unwrap();
    fs.write(file, 0, b"hello, world").unwrap();
    assert_eq!(fs.lookup(ROOT, "hello.txt"), Ok(file));
    let inode = fs.inode(file).unwrap();
    assert_eq!((inode.size, inode.uid, inode.gid, inode.permissions()), (12, 1000, 1000, 0o644));
    let mut buffer = [0; 32];
    assert_eq!(fs.read(file, 7, &mut buffer), Ok(5));
    assert_eq!(&buffer[..5], b"world");
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "round-trip");
}

/// A pattern that differs at every position, to catch misplaced blocks.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

#[test]
fn big_files_use_indirect_blocks() {
    // With 1 KiB blocks, 12 direct + 256 single indirect blocks cover
    // 268 KiB; 3 MiB needs the double indirect tree.
    let mut disk = device(32);
    format(&mut disk, Some(1024));
    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    let file = fs.create(ROOT, "big", FileType::Regular, 0o600, OWNER).unwrap();
    let data = pattern(3 << 20, 1);
    // In uneven pieces, so writes start and end mid-block.
    let mut offset = 0;
    for piece in data.chunks(77_777) {
        fs.write(file, offset, piece).unwrap();
        offset += piece.len() as u64;
    }
    let mut back = vec![0; data.len()];
    assert_eq!(fs.read(file, 0, &mut back), Ok(data.len()));
    assert!(back == data);
    let free_before = fs.free_blocks();
    fs.truncate(file, 100_000).unwrap();
    assert!(fs.free_blocks() > free_before + 2900);
    let mut back = vec![0; 200_000];
    assert_eq!(fs.read(file, 0, &mut back), Ok(100_000));
    assert!(back[..100_000] == data[..100_000]);
    // Growing again leaves a hole of zeros, including the rest of the
    // block that was cut.
    fs.write(file, 150_000, b"end").unwrap();
    assert_eq!(fs.read(file, 99_990, &mut back[..60_013]), Ok(50_013));
    assert!(back[..10] == data[99_990..100_000]);
    assert!(back[10..50_010].iter().all(|&b| b == 0));
    assert_eq!(&back[50_010..50_013], b"end");
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "indirect");
}

#[test]
fn directories_grow_and_shrink() {
    let mut disk = device(16);
    format(&mut disk, Some(1024));
    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    let directory = fs.create(ROOT, "many", FileType::Directory, 0o755, OWNER).unwrap();
    for index in 0..300 {
        let name = std::format!("a file with a fairly long name, number {index}");
        fs.create(directory, &name, FileType::Regular, 0o644, OWNER).unwrap();
    }
    assert_eq!(fs.list(directory).unwrap().len(), 300);
    assert!(fs.inode(directory).unwrap().size > 10 * 1024);
    for index in (0..300).step_by(2) {
        let name = std::format!("a file with a fairly long name, number {index}");
        let removed = fs.unlink(directory, &name).unwrap();
        assert!(removed.orphaned);
        fs.release(removed.inode).unwrap();
    }
    let names: Vec<String> = fs.list(directory).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names.len(), 150);
    assert!(names.iter().all(|name| name.ends_with(['1', '3', '5', '7', '9'])));
    assert_eq!(
        fs.create(directory, "a file with a fairly long name, number 1", FileType::Regular, 0, OWNER),
        Err(Error::Exists)
    );
    assert_eq!(fs.remove_directory(ROOT, "many").err(), Some(Error::NotEmpty));
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "directories");
}

#[test]
fn links_renames_and_symlinks() {
    let mut disk = device(16);
    format(&mut disk, None);
    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    let a = fs.create(ROOT, "a", FileType::Directory, 0o755, OWNER).unwrap();
    let b = fs.create(ROOT, "b", FileType::Directory, 0o755, OWNER).unwrap();
    let inner = fs.create(a, "inner", FileType::Directory, 0o755, OWNER).unwrap();
    let file = fs.create(a, "file", FileType::Regular, 0o644, OWNER).unwrap();
    fs.write(file, 0, b"contents").unwrap();
    assert_eq!(fs.inode(ROOT).unwrap().links, 5);
    assert_eq!(fs.inode(a).unwrap().links, 3);

    // Hard links.
    fs.link(b, "same file", file).unwrap();
    assert_eq!(fs.inode(file).unwrap().links, 2);
    assert!(!fs.unlink(a, "file").unwrap().orphaned);
    assert_eq!(fs.lookup(b, "same file"), Ok(file));

    // Moving a directory updates `..` and both parents' link counts.
    fs.rename(a, "inner", b, "moved").unwrap();
    assert_eq!(fs.lookup(inner, ".."), Ok(b));
    assert_eq!((fs.inode(a).unwrap().links, fs.inode(b).unwrap().links), (2, 3));
    assert_eq!(fs.rename(ROOT, "b", inner, "loop"), Err(Error::Loop));
    // Replacing a file.
    let other = fs.create(b, "other", FileType::Regular, 0o644, OWNER).unwrap();
    let replaced = fs.rename(b, "same file", b, "other").unwrap().unwrap();
    assert_eq!(replaced, Unlinked { inode: other, orphaned: true });
    fs.release(other).unwrap();

    // Symbolic links, short (in the inode) and long (in a block).
    fs.symlink(ROOT, "short", "a/file", OWNER).unwrap();
    let long_target: String = core::iter::repeat_n("long/", 30).collect();
    let long = fs.symlink(ROOT, "long", &long_target, OWNER).unwrap();
    let short = fs.lookup(ROOT, "short").unwrap();
    assert_eq!(fs.read_link(short).unwrap(), "a/file");
    assert_eq!(fs.read_link(long).unwrap(), long_target);
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "links");
}

#[test]
fn space_runs_out_cleanly() {
    let mut disk = device(4);
    format(&mut disk, Some(1024));
    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    let file = fs.create(ROOT, "filler", FileType::Regular, 0o644, OWNER).unwrap();
    let chunk = vec![0xAB; 64 * 1024];
    let mut offset = 0;
    let error = loop {
        match fs.write(file, offset, &chunk) {
            Ok(_) => offset += chunk.len() as u64,
            Err(error) => break error,
        }
    };
    assert_eq!(error, Error::NoSpace);
    assert_eq!(fs.free_blocks(), 0);
    fs.unlink(ROOT, "filler").unwrap();
    fs.release(file).unwrap();
    assert!(fs.free_blocks() > 3000);
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "full");
}

/// Volumes made by Linux's mke2fs (with its default ext2 features, like
/// hashed directory indexes) can be read and changed, and stay valid.
#[test]
fn volumes_made_by_linux_work() {
    let path = std::env::temp_dir().join(std::format!("eli-ext2-linux-{}.img", std::process::id()));
    let mut disk = device(32);
    std::fs::write(&path, &disk.data).unwrap();
    let made = Command::new("mke2fs").args(["-q", "-F", "-t", "ext2", "-L", "linux"]).arg(&path).status();
    if !made.is_ok_and(|status| status.success()) {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // Plenty of entries, so `e2fsck -D` builds a hashed index.
    let script = (0..400).map(|i| std::format!("write /dev/null entry{i}\n")).collect::<String>() + "mkdir dir\n";
    let script_path = path.with_extension("cmds");
    std::fs::write(&script_path, script).unwrap();
    Command::new("debugfs").arg("-w").arg("-f").arg(&script_path).arg(&path).output().unwrap();
    Command::new("e2fsck").arg("-fyD").arg(&path).output().unwrap();
    disk.data = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&script_path);

    let mut fs = FileSystem::mount(&mut disk, clock, false).unwrap();
    assert_eq!(fs.label(), "linux");
    assert_eq!(fs.list(ROOT).unwrap().len(), 402);
    assert!(fs.lookup(ROOT, "entry399").is_ok());
    let file = fs.create(ROOT, "from eliOS", FileType::Regular, 0o644, OWNER).unwrap();
    fs.write(file, 0, &pattern(100_000, 2)).unwrap();
    for index in 0..100 {
        let removed = fs.unlink(ROOT, &std::format!("entry{index}")).unwrap();
        fs.release(removed.inode).unwrap();
    }
    fs.unmount().unwrap();
    drop(fs);
    fsck(&disk, "linux");
}
