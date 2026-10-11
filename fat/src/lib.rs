//! FAT32 file systems: formatting, reading and writing, with long file
//! names.
//!
//! Used both by the kernel, to read and write disks, and by xtask, to build
//! bootable disk images. UEFI firmware boots from FAT, so FAT32 is what the
//! boot partition has to be; it's also what nearly every other system can
//! read, which makes it a good format for files moving between machines.
//!
//! A FAT32 volume is a boot sector describing the layout, a file allocation
//! table (FAT) with one entry per cluster saying which cluster comes next in
//! a file (cluster chains), and the clusters themselves. Directories are
//! files of 32-byte entries; long names are stored in extra entries placed
//! before the classic 8.3 entry.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const SECTOR_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device failed.
    Io,
    NotFound,
    Exists,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    NoSpace,
    InvalidName,
    /// The volume isn't FAT32, or its structures don't make sense.
    Corrupt,
    /// Valid, but something this implementation doesn't handle.
    Unsupported,
}

pub type Result<T> = core::result::Result<T, Error>;

/// Storage made of 512-byte sectors.
pub trait BlockDevice {
    /// Reads whole sectors starting at `sector`; `buffer` is a multiple of
    /// 512 bytes long.
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> Result<()>;
    fn write(&mut self, sector: u64, data: &[u8]) -> Result<()>;
    fn sectors(&self) -> u64;
}

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0F;

const END_OF_CHAIN: u32 = 0x0FFF_FFFF;
const CLUSTER_MASK: u32 = 0x0FFF_FFFF;
const FIRST_END_MARKER: u32 = 0x0FFF_FFF8;
const SLOT: usize = 32;
const DELETED: u8 = 0xE5;
/// Characters per long-name slot.
const LONG_NAME_CHARS: usize = 13;
/// Device transfers are split into pieces of at most this many sectors.
const MAX_TRANSFER: usize = 128;

/// A file or directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub size: u32,
    /// The first cluster, or 0 for an empty file.
    pub cluster: u32,
    /// Last modification, in FAT's date and time formats.
    pub date: u16,
    pub time: u16,
    short_name: [u8; 11],
    location: Option<Location>,
}

/// Where an entry's directory slots are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Location {
    /// First cluster of the directory holding the entry.
    directory: u32,
    /// Byte offsets (in the directory) of the first slot (long-name parts
    /// come first) and of the short entry.
    first_slot: u32,
    short_slot: u32,
}

impl Entry {
    /// Identifies the entry as long as it isn't renamed: where its short
    /// entry is. The root directory is (0, 0).
    pub fn id(&self) -> (u32, u32) {
        self.location.map_or((0, 0), |l| (l.directory, l.short_slot))
    }

    /// The modification time as seconds since the Unix epoch.
    pub fn modified_unix(&self) -> u64 {
        let year = 1980 + i64::from(self.date >> 9);
        let month = i64::from((self.date >> 5) & 0xF).max(1);
        let day = i64::from(self.date & 0x1F).max(1);
        let seconds = u64::from(self.time >> 11) * 3600
            + u64::from((self.time >> 5) & 0x3F) * 60
            + u64::from(self.time & 0x1F) * 2;
        days_from_civil(year, month, day).max(0) as u64 * 86_400 + seconds
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

/// Converts seconds since the Unix epoch to FAT's (date, time).
pub fn fat_timestamp(unix: u64) -> (u16, u16) {
    let days = (unix / 86_400) as i64;
    let rest = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let date = (((year - 1980).clamp(0, 127) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((rest / 3600) as u16) << 11) | (((rest / 60 % 60) as u16) << 5) | ((rest % 60 / 2) as u16);
    (date, time)
}

pub struct FileSystem<D> {
    device: D,
    sectors_per_cluster: u32,
    /// First sector of the first FAT.
    fat_start: u64,
    fat_sectors: u64,
    fat_count: u32,
    data_start: u64,
    /// Number of data clusters; valid cluster numbers are 2..clusters + 2.
    clusters: u32,
    root_cluster: u32,
    /// The last FAT sector used (from the first FAT), and whether it has
    /// changes not yet written to every copy of the FAT. Changes are
    /// written back at the end of each operation, so a file growing by
    /// thousands of clusters costs a few writes, not thousands.
    fat_cache: Option<(u64, [u8; SECTOR_SIZE], bool)>,
    /// The cluster chain of the file read or written last, by its first
    /// cluster: reading a big file in pieces would otherwise follow the
    /// whole chain through the FAT for every piece.
    chain_cache: Option<(u32, Vec<u32>)>,
    /// Where to start looking for free clusters.
    next_free: u32,
    /// Free clusters, once counted (then kept up to date).
    free_clusters: Option<u32>,
    /// Seconds since the Unix epoch, for timestamps.
    clock: fn() -> u64,
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

impl<D: BlockDevice> FileSystem<D> {
    /// Mounts the FAT32 volume on `device`.
    pub fn mount(mut device: D, clock: fn() -> u64) -> Result<Self> {
        let mut boot = [0; SECTOR_SIZE];
        device.read(0, &mut boot)?;
        if boot[510..512] != [0x55, 0xAA] || u16_at(&boot, 11) as usize != SECTOR_SIZE {
            return Err(Error::Corrupt);
        }
        let sectors_per_cluster = u32::from(boot[13]);
        let reserved = u64::from(u16_at(&boot, 14));
        let fat_count = u32::from(boot[16]);
        let fat_sectors = u64::from(u32_at(&boot, 36));
        if u16_at(&boot, 22) != 0 || fat_sectors == 0 {
            // FAT12 or FAT16.
            return Err(Error::Unsupported);
        }
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() || fat_count == 0 {
            return Err(Error::Corrupt);
        }
        let total = match u16_at(&boot, 19) {
            0 => u64::from(u32_at(&boot, 32)),
            small => u64::from(small),
        };
        let data_start = reserved + fat_sectors * u64::from(fat_count);
        let clusters = (total.saturating_sub(data_start) / u64::from(sectors_per_cluster)) as u32;
        let root_cluster = u32_at(&boot, 44);
        // The specification says a FAT32 volume has at least 65,525
        // clusters, but some tools make smaller ones (Linux's mkfs.vfat, for
        // small EFI partitions). Like Linux, the layout of the boot sector
        // decides; the FAT just has to be big enough for every cluster.
        if clusters == 0
            || fat_sectors * (SECTOR_SIZE as u64 / 4) < u64::from(clusters) + 2
            || total > device.sectors()
            || !(2..clusters + 2).contains(&root_cluster)
        {
            return Err(Error::Corrupt);
        }
        Ok(Self {
            device,
            sectors_per_cluster,
            fat_start: reserved,
            fat_sectors,
            fat_count,
            data_start,
            clusters,
            root_cluster,
            fat_cache: None,
            chain_cache: None,
            next_free: 2,
            free_clusters: None,
            clock,
        })
    }

    /// Formats `device` as FAT32. It needs to be at least 33 MiB: FAT32
    /// requires 65525 clusters.
    pub fn format(device: &mut D, label: &str) -> Result<()> {
        let total = device.sectors();
        // Small volumes need small clusters to have enough of them.
        let sectors_per_cluster: u32 = match total {
            0..532_480 => 1,
            532_480..16_777_216 => 8,
            16_777_216..33_554_432 => 16,
            _ => 32,
        };
        let reserved: u64 = 32;
        let fat_count: u64 = 2;
        let mut fat_sectors = 1;
        loop {
            let clusters = (total - reserved - fat_count * fat_sectors) / u64::from(sectors_per_cluster);
            let needed = ((clusters + 2) * 4).div_ceil(SECTOR_SIZE as u64);
            if needed <= fat_sectors {
                break;
            }
            fat_sectors = needed;
        }
        let clusters = (total - reserved - fat_count * fat_sectors) / u64::from(sectors_per_cluster);
        if !(65_525..=0x0FFF_FFF5).contains(&clusters) {
            return Err(Error::Unsupported);
        }

        let mut boot = [0u8; SECTOR_SIZE];
        boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"ELIOS   ");
        boot[11..13].copy_from_slice(&(SECTOR_SIZE as u16).to_le_bytes());
        boot[13] = sectors_per_cluster as u8;
        boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        boot[16] = fat_count as u8;
        boot[21] = 0xF8;
        boot[24..26].copy_from_slice(&63u16.to_le_bytes());
        boot[26..28].copy_from_slice(&255u16.to_le_bytes());
        boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
        boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[48..50].copy_from_slice(&1u16.to_le_bytes());
        boot[50..52].copy_from_slice(&6u16.to_le_bytes());
        boot[64] = 0x80;
        boot[66] = 0x29;
        boot[67..71].copy_from_slice(&0xE105_2026u32.to_le_bytes());
        let mut volume_label = [b' '; 11];
        for (slot, byte) in volume_label.iter_mut().zip(label.bytes().map(|b| b.to_ascii_uppercase())) {
            *slot = byte;
        }
        boot[71..82].copy_from_slice(&volume_label);
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510] = 0x55;
        boot[511] = 0xAA;

        let mut info = [0u8; SECTOR_SIZE];
        info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        // Free count and next free: unknown.
        info[488..496].fill(0xFF);
        info[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());

        device.write(0, &boot)?;
        device.write(1, &info)?;
        device.write(6, &boot)?;
        device.write(7, &info)?;

        // Empty FATs, except the reserved entries and the root directory.
        let zeros = vec![0u8; SECTOR_SIZE * MAX_TRANSFER];
        for copy in 0..fat_count {
            let start = reserved + copy * fat_sectors;
            let mut done = 0;
            while done < fat_sectors {
                let count = (fat_sectors - done).min(MAX_TRANSFER as u64);
                device.write(start + done, &zeros[..count as usize * SECTOR_SIZE])?;
                done += count;
            }
            let mut first = [0u8; SECTOR_SIZE];
            first[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
            first[4..8].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
            first[8..12].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
            device.write(start, &first)?;
        }
        // An empty root directory, holding just the volume label.
        let data_start = reserved + fat_count * fat_sectors;
        let mut root = vec![0u8; SECTOR_SIZE * sectors_per_cluster as usize];
        root[..11].copy_from_slice(&volume_label);
        root[11] = ATTR_VOLUME_ID;
        device.write(data_start, &root)?;
        Ok(())
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.device
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR_SIZE
    }

    fn cluster_sector(&self, cluster: u32) -> u64 {
        self.data_start + u64::from(cluster - 2) * u64::from(self.sectors_per_cluster)
    }

    fn valid_cluster(&self, cluster: u32) -> bool {
        (2..self.clusters + 2).contains(&cluster)
    }

    /// The root directory.
    pub fn root(&self) -> Entry {
        Entry {
            name: String::from("/"),
            directory: true,
            size: 0,
            cluster: self.root_cluster,
            date: 0,
            time: 0,
            short_name: [b' '; 11],
            location: None,
        }
    }

    /// Makes the FAT sector holding `cluster`'s entry the cached one.
    /// Returns the entry's offset in it.
    fn load_fat_sector(&mut self, cluster: u32) -> Result<usize> {
        let offset = u64::from(cluster) * 4;
        let sector = self.fat_start + offset / SECTOR_SIZE as u64;
        if self.fat_cache.as_ref().is_none_or(|(cached, _, _)| *cached != sector) {
            self.flush_fat()?;
            let mut data = [0; SECTOR_SIZE];
            self.device.read(sector, &mut data)?;
            self.fat_cache = Some((sector, data, false));
        }
        Ok((offset % SECTOR_SIZE as u64) as usize)
    }

    fn fat_entry(&mut self, cluster: u32) -> Result<u32> {
        let within = self.load_fat_sector(cluster)?;
        let (_, data, _) = self.fat_cache.as_ref().unwrap();
        Ok(u32_at(data, within) & CLUSTER_MASK)
    }

    fn set_fat_entry(&mut self, cluster: u32, value: u32) -> Result<()> {
        let within = self.load_fat_sector(cluster)?;
        let (_, data, dirty) = self.fat_cache.as_mut().unwrap();
        // Keep the top four bits, which are reserved.
        let old = u32_at(data, within);
        data[within..within + 4].copy_from_slice(&((old & !CLUSTER_MASK) | (value & CLUSTER_MASK)).to_le_bytes());
        *dirty = true;
        Ok(())
    }

    /// Writes the cached FAT sector to every copy of the FAT, if changed.
    fn flush_fat(&mut self) -> Result<()> {
        if let Some((sector, data, dirty)) = &mut self.fat_cache
            && *dirty
        {
            for copy in 0..u64::from(self.fat_count) {
                self.device.write(*sector + copy * self.fat_sectors, data)?;
            }
            *dirty = false;
        }
        Ok(())
    }

    /// Runs a change, then writes the FAT back, whether it worked or not.
    fn change<T>(&mut self, operation: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let result = operation(self);
        let flushed = self.flush_fat();
        let value = result?;
        flushed?;
        Ok(value)
    }

    /// The clusters of a chain, in order.
    fn chain(&mut self, first: u32) -> Result<Vec<u32>> {
        let mut clusters = Vec::new();
        let mut cluster = first;
        while cluster != 0 && cluster < FIRST_END_MARKER {
            if !self.valid_cluster(cluster) || clusters.len() > self.clusters as usize {
                return Err(Error::Corrupt);
            }
            clusters.push(cluster);
            cluster = self.fat_entry(cluster)?;
        }
        Ok(clusters)
    }

    /// Takes a free cluster, marking it as the end of a chain.
    fn allocate(&mut self) -> Result<u32> {
        for step in 0..self.clusters {
            let cluster = 2 + (self.next_free - 2 + step) % self.clusters;
            if self.fat_entry(cluster)? == 0 {
                self.set_fat_entry(cluster, END_OF_CHAIN)?;
                self.next_free = cluster;
                if let Some(free) = &mut self.free_clusters {
                    *free -= 1;
                }
                return Ok(cluster);
            }
        }
        Err(Error::NoSpace)
    }

    fn zero_cluster(&mut self, cluster: u32) -> Result<()> {
        let zeros = vec![0; self.cluster_bytes()];
        let sector = self.cluster_sector(cluster);
        self.device.write(sector, &zeros)
    }

    /// A file's chain, from the cache if it's the one used last.
    fn take_chain(&mut self, first: u32) -> Result<Vec<u32>> {
        match self.chain_cache.take() {
            Some((cached, chain)) if cached == first && first != 0 => Ok(chain),
            _ => self.chain(first),
        }
    }

    fn free_chain(&mut self, first: u32) -> Result<()> {
        self.chain_cache = None;
        for cluster in self.chain(first)? {
            self.set_fat_entry(cluster, 0)?;
            if let Some(free) = &mut self.free_clusters {
                *free += 1;
            }
        }
        Ok(())
    }

    /// Reads or writes `len` bytes at `offset` within a chain, in as few
    /// device requests as the layout allows (runs of consecutive clusters).
    fn transfer(&mut self, chain: &[u32], offset: usize, buffer: &mut [u8], write: bool) -> Result<()> {
        let cluster_bytes = self.cluster_bytes();
        let mut done = 0;
        while done < buffer.len() {
            let position = offset + done;
            let index = position / cluster_bytes;
            // Extend over following clusters that are physically next.
            let mut run = 1;
            while index + run < chain.len() && chain[index + run] == chain[index] + run as u32 {
                run += 1;
            }
            let run_start = self.cluster_sector(chain[index]) * SECTOR_SIZE as u64;
            let run_bytes = run * cluster_bytes;
            let within = position - index * cluster_bytes;
            let count = (run_bytes - within).min(buffer.len() - done);

            // Whole sectors covering the piece.
            let first_sector = (run_start + within as u64) / SECTOR_SIZE as u64;
            let head = within % SECTOR_SIZE;
            let mut remaining = count;
            let mut sector = first_sector;
            let mut skip = head;
            let mut cursor = done;
            while remaining > 0 {
                let sectors = (skip + remaining).div_ceil(SECTOR_SIZE).min(MAX_TRANSFER);
                let mut scratch = vec![0u8; sectors * SECTOR_SIZE];
                let piece = (sectors * SECTOR_SIZE - skip).min(remaining);
                let partial = skip != 0 || !piece.is_multiple_of(SECTOR_SIZE);
                if !write || partial {
                    self.device.read(sector, &mut scratch)?;
                }
                if write {
                    scratch[skip..skip + piece].copy_from_slice(&buffer[cursor..cursor + piece]);
                    self.device.write(sector, &scratch)?;
                } else {
                    buffer[cursor..cursor + piece].copy_from_slice(&scratch[skip..skip + piece]);
                }
                remaining -= piece;
                cursor += piece;
                sector += sectors as u64;
                skip = 0;
            }
            done += count;
        }
        Ok(())
    }

    /// The raw contents of a directory, and its cluster chain.
    fn directory_data(&mut self, directory: &Entry) -> Result<(Vec<u32>, Vec<u8>)> {
        if !directory.directory {
            return Err(Error::NotDirectory);
        }
        let chain = self.chain(directory.cluster)?;
        let mut data = vec![0; chain.len() * self.cluster_bytes()];
        self.transfer(&chain, 0, &mut data, false)?;
        Ok((chain, data))
    }

    /// The entries of a directory, without `.` and `..`.
    pub fn list(&mut self, directory: &Entry) -> Result<Vec<Entry>> {
        let (_, data) = self.directory_data(directory)?;
        Ok(parse_directory(&data, directory.cluster))
    }

    /// Finds an entry by name (ignoring case, as FAT does).
    pub fn lookup(&mut self, directory: &Entry, name: &str) -> Result<Entry> {
        self.list(directory)?.into_iter().find(|entry| entry.name.eq_ignore_ascii_case(name)).ok_or(Error::NotFound)
    }

    /// Reads from a file at `offset`. Returns the number of bytes read.
    pub fn read(&mut self, file: &Entry, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        if file.directory {
            return Err(Error::IsDirectory);
        }
        let size = u64::from(file.size);
        if offset >= size {
            return Ok(0);
        }
        let count = (buffer.len() as u64).min(size - offset) as usize;
        let chain = self.take_chain(file.cluster)?;
        if chain.len() * self.cluster_bytes() < file.size as usize {
            return Err(Error::Corrupt);
        }
        self.transfer(&chain, offset as usize, &mut buffer[..count], false)?;
        self.chain_cache = Some((file.cluster, chain));
        Ok(count)
    }

    /// Makes sure a file's chain has room for `bytes`, allocating clusters
    /// (zeroed with `zero`, so gaps read as zeros; a write that's about to
    /// fill them needn't).
    fn grow(&mut self, file: &mut Entry, chain: &mut Vec<u32>, bytes: usize, zero: bool) -> Result<()> {
        let needed = bytes.div_ceil(self.cluster_bytes());
        while chain.len() < needed {
            let cluster = self.allocate()?;
            if zero {
                self.zero_cluster(cluster)?;
            }
            match chain.last() {
                Some(&last) => self.set_fat_entry(last, cluster)?,
                None => file.cluster = cluster,
            }
            chain.push(cluster);
        }
        Ok(())
    }

    /// Writes to a file at `offset`, extending it as needed, and updates its
    /// directory entry (`file` too).
    pub fn write(&mut self, file: &mut Entry, offset: u64, data: &[u8]) -> Result<usize> {
        if file.directory {
            return Err(Error::IsDirectory);
        }
        let end =
            offset.checked_add(data.len() as u64).filter(|&end| end <= u64::from(u32::MAX)).ok_or(Error::NoSpace)?;
        self.change(|fs| {
            let mut chain = fs.take_chain(file.cluster)?;
            fs.grow(file, &mut chain, end as usize, false)?;
            // A write past the end leaves a gap, which must read as zeros.
            if offset > u64::from(file.size) {
                let mut zeros = vec![0; (offset - u64::from(file.size)) as usize];
                fs.transfer(&chain, file.size as usize, &mut zeros, true)?;
            }
            let mut copy = data.to_vec();
            fs.transfer(&chain, offset as usize, &mut copy, true)?;
            fs.chain_cache = Some((file.cluster, chain));
            file.size = file.size.max(end as u32);
            fs.touch(file)?;
            Ok(data.len())
        })
    }

    /// Changes a file's length, freeing or adding (zeroed) clusters.
    pub fn truncate(&mut self, file: &mut Entry, len: u64) -> Result<()> {
        self.change(|fs| fs.truncate_unflushed(file, len))
    }

    fn truncate_unflushed(&mut self, file: &mut Entry, len: u64) -> Result<()> {
        if file.directory {
            return Err(Error::IsDirectory);
        }
        let len = u32::try_from(len).map_err(|_| Error::NoSpace)?;
        self.chain_cache = None;
        let mut chain = self.chain(file.cluster)?;
        if len > file.size {
            self.grow(file, &mut chain, len as usize, true)?;
            // Zero the old tail of the last cluster too.
            let tail =
                (file.size as usize).next_multiple_of(self.cluster_bytes()).min(len as usize) - file.size as usize;
            let mut zeros = vec![0; tail];
            self.transfer(&chain, file.size as usize, &mut zeros, true)?;
        } else {
            let keep = (len as usize).div_ceil(self.cluster_bytes());
            if keep == 0 {
                if file.cluster != 0 {
                    self.free_chain(file.cluster)?;
                }
                file.cluster = 0;
            } else if keep < chain.len() {
                self.free_chain(chain[keep])?;
                self.set_fat_entry(chain[keep - 1], END_OF_CHAIN)?;
            }
        }
        file.size = len;
        self.touch(file)
    }

    /// Writes an entry's size, first cluster and a fresh modification time
    /// back to its directory slot.
    fn touch(&mut self, entry: &mut Entry) -> Result<()> {
        let (date, time) = fat_timestamp((self.clock)());
        entry.date = date;
        entry.time = time;
        let Some(location) = entry.location else { return Ok(()) };
        let slot = short_slot(&entry.short_name, entry.directory, entry.cluster, entry.size, date, time);
        self.write_slot(location.directory, location.short_slot, &slot)
    }

    fn write_slot(&mut self, directory: u32, offset: u32, slot: &[u8; SLOT]) -> Result<()> {
        let chain = self.chain(directory)?;
        let mut data = *slot;
        self.transfer(&chain, offset as usize, &mut data, true)
    }

    /// Creates a file or directory.
    pub fn create(&mut self, parent: &Entry, name: &str, directory: bool) -> Result<Entry> {
        self.change(|fs| fs.create_unflushed(parent, name, directory))
    }

    fn create_unflushed(&mut self, parent: &Entry, name: &str, directory: bool) -> Result<Entry> {
        validate_name(name)?;
        let existing = self.list(parent)?;
        if existing.iter().any(|entry| entry.name.eq_ignore_ascii_case(name)) {
            return Err(Error::Exists);
        }
        let cluster = if directory {
            let cluster = self.allocate()?;
            self.zero_cluster(cluster)?;
            // "." and "..": the parent is recorded as 0 when it's the root.
            let (date, time) = fat_timestamp((self.clock)());
            let parent_cluster = if parent.location.is_none() { 0 } else { parent.cluster };
            let mut dots = [0u8; 2 * SLOT];
            dots[..SLOT].copy_from_slice(&short_slot(b".          ", true, cluster, 0, date, time));
            dots[SLOT..].copy_from_slice(&short_slot(b"..         ", true, parent_cluster, 0, date, time));
            let sector = self.cluster_sector(cluster);
            let mut first = vec![0; SECTOR_SIZE];
            first[..2 * SLOT].copy_from_slice(&dots);
            self.device.write(sector, &first)?;
            cluster
        } else {
            0
        };
        let mut entry = self.add_slots(parent, name, directory, cluster, 0, &existing)?;
        self.touch(&mut entry)?;
        Ok(entry)
    }

    /// Writes the directory slots for a new entry (long name and short
    /// entry) into free space in `parent`, growing it if needed.
    fn add_slots(
        &mut self,
        parent: &Entry,
        name: &str,
        directory: bool,
        cluster: u32,
        size: u32,
        existing: &[Entry],
    ) -> Result<Entry> {
        let (short_name, long) = short_name_for(name, existing);
        let mut slots: Vec<[u8; SLOT]> = Vec::new();
        if long {
            slots.extend(long_name_slots(name, checksum(&short_name)));
        }
        let (date, time) = fat_timestamp((self.clock)());
        slots.push(short_slot(&short_name, directory, cluster, size, date, time));

        let (mut chain, data) = self.directory_data(parent)?;
        let total_slots = data.len() / SLOT;
        let needed = slots.len();
        // Look for `needed` free slots in a row: deleted ones, or anything
        // from the end marker (a slot starting with 0) on.
        let mut run = 0;
        let mut start = total_slots;
        for index in 0..total_slots {
            match data[index * SLOT] {
                0 => {
                    start = index - run;
                    break;
                }
                DELETED => {
                    run += 1;
                    if run == needed {
                        start = index + 1 - needed;
                        break;
                    }
                }
                _ => run = 0,
            }
        }
        if start + needed > total_slots {
            // Not enough room: add (zeroed) clusters to the directory.
            let mut grown = parent.clone();
            self.grow(&mut grown, &mut chain, (start + needed) * SLOT, true)?;
        }
        for (index, slot) in slots.iter().enumerate() {
            let offset = ((start + index) * SLOT) as u32;
            let mut bytes = *slot;
            self.transfer(&chain, offset as usize, &mut bytes, true)?;
        }
        Ok(Entry {
            name: String::from(name),
            directory,
            size,
            cluster,
            date,
            time,
            short_name,
            location: Some(Location {
                directory: parent.cluster,
                first_slot: (start * SLOT) as u32,
                short_slot: ((start + slots.len() - 1) * SLOT) as u32,
            }),
        })
    }

    /// Marks an entry's directory slots free.
    fn remove_slots(&mut self, entry: &Entry) -> Result<()> {
        let location = entry.location.ok_or(Error::Unsupported)?;
        let chain = self.chain(location.directory)?;
        let mut offset = location.first_slot;
        while offset <= location.short_slot {
            let mut marker = [DELETED];
            self.transfer(&chain, offset as usize, &mut marker, true)?;
            offset += SLOT as u32;
        }
        Ok(())
    }

    /// Deletes a file or an empty directory.
    pub fn remove(&mut self, entry: &Entry) -> Result<()> {
        self.change(|fs| fs.remove_unflushed(entry))
    }

    fn remove_unflushed(&mut self, entry: &Entry) -> Result<()> {
        if entry.directory && !self.list(entry)?.is_empty() {
            return Err(Error::NotEmpty);
        }
        self.remove_slots(entry)?;
        if entry.cluster != 0 {
            self.free_chain(entry.cluster)?;
        }
        Ok(())
    }

    /// Moves an entry to `new_parent` under `new_name`. Returns it as it is
    /// now.
    pub fn rename(&mut self, entry: &Entry, new_parent: &Entry, new_name: &str) -> Result<Entry> {
        self.change(|fs| fs.rename_unflushed(entry, new_parent, new_name))
    }

    fn rename_unflushed(&mut self, entry: &Entry, new_parent: &Entry, new_name: &str) -> Result<Entry> {
        validate_name(new_name)?;
        let existing = self.list(new_parent)?;
        if existing.iter().any(|other| other.name.eq_ignore_ascii_case(new_name) && other.id() != entry.id()) {
            return Err(Error::Exists);
        }
        let mut moved = self.add_slots(new_parent, new_name, entry.directory, entry.cluster, entry.size, &existing)?;
        self.remove_slots(entry)?;
        if entry.directory && entry.location.map(|l| l.directory) != Some(new_parent.cluster) {
            // Point the directory's ".." at its new parent.
            let parent_cluster = if new_parent.location.is_none() { 0 } else { new_parent.cluster };
            let mut slot = short_slot(b"..         ", true, parent_cluster, 0, moved.date, moved.time);
            let chain = self.chain(entry.cluster)?;
            self.transfer(&chain, SLOT, &mut slot, true)?;
        }
        moved.date = entry.date;
        moved.time = entry.time;
        Ok(moved)
    }

    /// Free space, in bytes (counts the FAT; takes a moment on big volumes).
    /// Free space, in bytes. The first call counts free clusters in the FAT
    /// (a moment on big volumes); after that the count is kept up to date.
    pub fn free_bytes(&mut self) -> Result<u64> {
        let free = match self.free_clusters {
            Some(free) => free,
            None => {
                // The whole FAT is read, in big pieces (a sector at a time
                // is slow on devices where each request costs, like USB).
                self.flush_fat()?;
                let mut free = 0;
                let entries_per_sector = SECTOR_SIZE as u64 / 4;
                let needed = (u64::from(self.clusters) + 2).div_ceil(entries_per_sector).min(self.fat_sectors);
                let mut sector = 0;
                while sector < needed {
                    let count = (needed - sector).min(MAX_TRANSFER as u64);
                    let mut data = vec![0u8; count as usize * SECTOR_SIZE];
                    self.device.read(self.fat_start + sector, &mut data)?;
                    for (index, entry) in data.chunks(4).enumerate() {
                        let cluster = sector * entries_per_sector + index as u64;
                        if (2..u64::from(self.clusters) + 2).contains(&cluster) && u32_at(entry, 0) & CLUSTER_MASK == 0
                        {
                            free += 1;
                        }
                    }
                    sector += count;
                }
                self.free_clusters = Some(free);
                free
            }
        };
        Ok(u64::from(free) * self.cluster_bytes() as u64)
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_bytes() as u64
    }

    pub fn total_bytes(&self) -> u64 {
        u64::from(self.clusters) * self.cluster_bytes() as u64
    }
}

fn parse_directory(data: &[u8], directory: u32) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut long_parts: Vec<[u16; LONG_NAME_CHARS]> = Vec::new();
    let mut long_start = None;
    let mut long_checksum = 0;
    for (index, slot) in data.as_chunks::<SLOT>().0.iter().enumerate() {
        let offset = (index * SLOT) as u32;
        match slot[0] {
            0 => break,
            DELETED => {
                long_parts.clear();
                long_start = None;
                continue;
            }
            _ => {}
        }
        let attributes = slot[11];
        if attributes & 0x3F == ATTR_LONG_NAME {
            let order = (slot[0] & 0x1F) as usize;
            if slot[0] & 0x40 != 0 {
                long_parts = vec![[0xFFFF; LONG_NAME_CHARS]; order];
                long_start = Some(offset);
                long_checksum = slot[13];
            }
            if (1..=long_parts.len()).contains(&order) {
                let part = &mut long_parts[order - 1];
                let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (character, &position) in part.iter_mut().zip(&positions) {
                    *character = u16_at(slot, position);
                }
            }
            continue;
        }
        if attributes & ATTR_VOLUME_ID != 0 {
            long_parts.clear();
            long_start = None;
            continue;
        }

        let short_name: [u8; 11] = slot[..11].try_into().unwrap();
        let mut name = if !long_parts.is_empty() && checksum(&short_name) == long_checksum {
            let units: Vec<u16> =
                long_parts.iter().flatten().copied().take_while(|&unit| unit != 0 && unit != 0xFFFF).collect();
            String::from_utf16_lossy(&units)
        } else {
            display_short_name(&short_name, slot[12])
        };
        let first_slot = if long_parts.is_empty() { offset } else { long_start.unwrap_or(offset) };
        long_parts.clear();
        long_start = None;
        if name == "." || name == ".." {
            continue;
        }
        if name.is_empty() {
            name = String::from("?");
        }
        entries.push(Entry {
            name,
            directory: attributes & ATTR_DIRECTORY != 0,
            size: u32_at(slot, 28),
            cluster: (u32::from(u16_at(slot, 20)) << 16) | u32::from(u16_at(slot, 26)),
            date: u16_at(slot, 24),
            time: u16_at(slot, 22),
            short_name,
            location: Some(Location { directory, first_slot, short_slot: offset }),
        });
    }
    entries
}

/// An 8.3 name as shown: lowercased where Windows' case flags say so.
fn display_short_name(short: &[u8; 11], case_flags: u8) -> String {
    let base: String = short[..8].iter().map(|&b| b as char).collect::<String>().trim_end().into();
    let extension: String = short[8..].iter().map(|&b| b as char).collect::<String>().trim_end().into();
    let base = if case_flags & 0x08 != 0 { base.to_ascii_lowercase() } else { base };
    let extension = if case_flags & 0x10 != 0 { extension.to_ascii_lowercase() } else { extension };
    if extension.is_empty() { base } else { alloc::format!("{base}.{extension}") }
}

fn short_slot(name: &[u8; 11], directory: bool, cluster: u32, size: u32, date: u16, time: u16) -> [u8; SLOT] {
    let mut slot = [0u8; SLOT];
    slot[..11].copy_from_slice(name);
    slot[11] = if directory { ATTR_DIRECTORY } else { ATTR_ARCHIVE };
    slot[14..16].copy_from_slice(&time.to_le_bytes());
    slot[16..18].copy_from_slice(&date.to_le_bytes());
    slot[18..20].copy_from_slice(&date.to_le_bytes());
    slot[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    slot[22..24].copy_from_slice(&time.to_le_bytes());
    slot[24..26].copy_from_slice(&date.to_le_bytes());
    slot[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    slot[28..32].copy_from_slice(&size.to_le_bytes());
    slot
}

/// The checksum of a short name that long-name slots carry.
fn checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &byte| ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte))
}

fn long_name_slots(name: &str, checksum: u8) -> Vec<[u8; SLOT]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let count = units.len().div_ceil(LONG_NAME_CHARS);
    let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    // Stored last part first.
    (1..=count)
        .rev()
        .map(|order| {
            let mut slot = [0u8; SLOT];
            slot[0] = order as u8 | if order == count { 0x40 } else { 0 };
            slot[11] = ATTR_LONG_NAME;
            slot[13] = checksum;
            for (index, &position) in positions.iter().enumerate() {
                let at = (order - 1) * LONG_NAME_CHARS + index;
                // After the name: one NUL, then 0xFFFF padding.
                let unit = match at.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[at],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                slot[position..position + 2].copy_from_slice(&unit.to_le_bytes());
            }
            slot
        })
        .collect()
}

fn validate_name(name: &str) -> Result<()> {
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.ends_with(' ')
        || name.chars().any(|c| c < ' ' || "\"*/:<>?\\|".contains(c));
    if invalid { Err(Error::InvalidName) } else { Ok(()) }
}

const SHORT_NAME_CHARACTERS: &str = "!#$%&'()-@^_`{}~";

fn short_name_character(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || SHORT_NAME_CHARACTERS.contains(c)
}

/// The 8.3 name for `name`, and whether long-name slots are needed: names
/// that aren't already valid uppercase 8.3 names get a generated one
/// (`BASE~1.EXT`) plus the long name.
fn short_name_for(name: &str, existing: &[Entry]) -> ([u8; 11], bool) {
    let pack = |base: &str, extension: &str| {
        let mut short = [b' '; 11];
        for (slot, byte) in short[..8].iter_mut().zip(base.bytes()) {
            *slot = byte;
        }
        for (slot, byte) in short[8..].iter_mut().zip(extension.bytes()) {
            *slot = byte;
        }
        short
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, extension),
        _ => (name, ""),
    };
    let fits = (1..=8).contains(&stem.len())
        && extension.len() <= 3
        && stem.chars().all(short_name_character)
        && extension.chars().all(short_name_character);
    if fits {
        return (pack(stem, extension), false);
    }

    let clean = |text: &str, limit: usize| -> String {
        text.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let upper = c.to_ascii_uppercase();
                if short_name_character(upper) { upper } else { '_' }
            })
            .take(limit)
            .collect()
    };
    let base = clean(stem, 6);
    let extension = clean(extension, 3);
    for number in 1..1_000_000u32 {
        let suffix = alloc::format!("~{number}");
        let keep = 8 - suffix.len();
        let candidate = alloc::format!("{}{suffix}", &base[..base.len().min(keep)]);
        let short = pack(&candidate, &extension);
        if !existing.iter().any(|entry| entry.short_name == short) {
            return (short, true);
        }
    }
    (pack("NONAME", &extension), true)
}

/// A block device in memory, for tests and for building images.
pub struct MemoryDevice {
    pub data: Vec<u8>,
}

impl MemoryDevice {
    pub fn new(sectors: u64) -> Self {
        Self { data: vec![0; sectors as usize * SECTOR_SIZE] }
    }
}

impl BlockDevice for MemoryDevice {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> Result<()> {
        let start = sector as usize * SECTOR_SIZE;
        let source = self.data.get(start..start + buffer.len()).ok_or(Error::Io)?;
        buffer.copy_from_slice(source);
        Ok(())
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> Result<()> {
        let start = sector as usize * SECTOR_SIZE;
        let target = self.data.get_mut(start..start + data.len()).ok_or(Error::Io)?;
        target.copy_from_slice(data);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        (self.data.len() / SECTOR_SIZE) as u64
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn clock() -> u64 {
        1_791_590_400 // 2026-10-10
    }

    fn volume(megabytes: u64) -> FileSystem<MemoryDevice> {
        let mut device = MemoryDevice::new(megabytes * 2048);
        FileSystem::format(&mut device, "TEST").unwrap();
        FileSystem::mount(device, clock).unwrap()
    }

    #[test]
    fn files_round_trip() {
        let mut fs = volume(40);
        let root = fs.root();
        let mut file = fs.create(&root, "hello.txt", false).unwrap();
        fs.write(&mut file, 0, b"hello, world").unwrap();
        let found = fs.lookup(&root, "HELLO.TXT").unwrap();
        assert_eq!(found.name, "hello.txt");
        assert_eq!(found.size, 12);
        let mut buffer = [0; 32];
        assert_eq!(fs.read(&found, 7, &mut buffer).unwrap(), 5);
        assert_eq!(&buffer[..5], b"world");
    }

    #[test]
    fn long_names_and_many_entries() {
        let mut fs = volume(40);
        let root = fs.root();
        let directory = fs.create(&root, "A Directory With A Long Name", true).unwrap();
        for index in 0..100 {
            fs.create(&directory, &std::format!("file number {index:03}.data"), false).unwrap();
        }
        let names: Vec<String> = fs.list(&directory).unwrap().into_iter().map(|entry| entry.name).collect();
        assert_eq!(names.len(), 100);
        assert_eq!(names[42], "file number 042.data");
        assert_eq!(fs.lookup(&root, "a directory with a long name").unwrap().cluster, directory.cluster);
    }

    #[test]
    fn large_files_span_clusters_and_free_them() {
        let mut fs = volume(40);
        let root = fs.root();
        let free = fs.free_bytes().unwrap();
        let mut file = fs.create(&root, "big", false).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        fs.write(&mut file, 0, &data).unwrap();
        let mut back = vec![0; data.len()];
        assert_eq!(fs.read(&file, 0, &mut back).unwrap(), data.len());
        assert_eq!(back, data);
        // Overwrite a stretch in the middle, crossing cluster boundaries.
        fs.write(&mut file, 1000, &[0xAB; 5000]).unwrap();
        fs.read(&file, 0, &mut back).unwrap();
        assert!(back[1000..6000].iter().all(|&b| b == 0xAB) && back[999] == data[999] && back[6000] == data[6000]);
        fs.truncate(&mut file, 10).unwrap();
        fs.remove(&file).unwrap();
        assert_eq!(fs.free_bytes().unwrap(), free);
        assert_eq!(fs.lookup(&root, "big").unwrap_err(), Error::NotFound);
    }

    #[test]
    fn rename_and_remove_directories() {
        let mut fs = volume(40);
        let root = fs.root();
        let a = fs.create(&root, "a", true).unwrap();
        let b = fs.create(&root, "b", true).unwrap();
        let mut file = fs.create(&a, "note", false).unwrap();
        fs.write(&mut file, 0, b"moved").unwrap();
        let moved = fs.rename(&file, &b, "renamed note").unwrap();
        assert_eq!(fs.lookup(&a, "note").unwrap_err(), Error::NotFound);
        let mut buffer = [0; 5];
        let found = fs.lookup(&b, "renamed note").unwrap();
        fs.read(&found, 0, &mut buffer).unwrap();
        assert_eq!(&buffer, b"moved");
        assert_eq!(fs.remove(&b).unwrap_err(), Error::NotEmpty);
        fs.remove(&moved).unwrap();
        fs.remove(&b).unwrap();
        fs.remove(&a).unwrap();
        assert!(fs.list(&root).unwrap().is_empty());
    }

    #[test]
    fn reopening_sees_the_same_files() {
        let mut fs = volume(40);
        let root = fs.root();
        let mut file = fs.create(&root, "persist.txt", false).unwrap();
        fs.write(&mut file, 0, b"still here").unwrap();
        let device = core::mem::replace(fs.device(), MemoryDevice::new(0));
        let mut fs = FileSystem::mount(device, clock).unwrap();
        let root = fs.root();
        let file = fs.lookup(&root, "persist.txt").unwrap();
        assert_eq!(file.size, 10);
        assert_eq!(file.modified_unix() / 86_400, clock() / 86_400);
    }

    /// Lots of growing, shrinking and deleting, checked by Linux's
    /// `fsck.vfat` (if it's installed), which knows FAT's rules well.
    #[test]
    fn linux_finds_no_errors() {
        let mut fs = volume(40);
        let root = fs.root();
        let directory = fs.create(&root, "a directory", true).unwrap();
        let mut files = alloc::vec::Vec::new();
        for index in 0..40 {
            let name = alloc::format!("file number {index}.data");
            let mut file = fs.create(&directory, &name, false).unwrap();
            let data: alloc::vec::Vec<u8> = (0..(index * 7919) as u32).map(|i| (i % 253) as u8).collect();
            // In pieces, some past the end (leaving gaps).
            for (piece, chunk) in data.chunks(3000).enumerate() {
                fs.write(&mut file, (piece * 3100) as u64, chunk).unwrap();
            }
            files.push(file);
        }
        for (index, file) in files.iter_mut().enumerate() {
            match index % 3 {
                0 => fs.remove(file).unwrap(),
                1 => fs.truncate(file, 1000).unwrap(),
                _ => {}
            }
        }
        let mut gap = fs.create(&root, "gap", false).unwrap();
        fs.write(&mut gap, 100_000, b"end").unwrap();
        let mut back = [1u8; 64];
        fs.read(&gap, 50_000, &mut back).unwrap();
        assert!(back.iter().all(|&b| b == 0), "a gap didn't read as zeros");

        let image = core::mem::replace(fs.device(), MemoryDevice::new(0)).data;
        let path = std::env::temp_dir().join(alloc::format!("eli-fat-{}.img", std::process::id()));
        std::fs::write(&path, &image).unwrap();
        let output = std::process::Command::new("fsck.vfat").arg("-n").arg(&path).output();
        let _ = std::fs::remove_file(&path);
        let Ok(output) = output else { return };
        let report = alloc::string::String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "fsck.vfat found problems:\n{report}");
    }

    #[test]
    fn timestamps_convert_both_ways() {
        let (date, time) = fat_timestamp(1_791_652_862); // 2026-10-10 17:21:02
        assert_eq!(date, (46 << 9) | (10 << 5) | 10);
        assert_eq!(time, (17 << 11) | (21 << 5) | 1);
    }
}
