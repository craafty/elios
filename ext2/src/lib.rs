//! The second extended file system (ext2): formatting, reading and writing.
//!
//! eliOS keeps installed systems on ext2 because it stores what FAT can't:
//! owners, permissions, symbolic links and hard links. It's also Linux's
//! classic file system, so an eliOS disk can be inspected (and checked with
//! `e2fsck`) on Linux. Used by the kernel, and by the installer to prepare a
//! disk.
//!
//! # Layout
//!
//! The volume is an array of blocks (1, 2 or 4 KiB), split into *block
//! groups*. Each group has a bitmap of its free blocks, a bitmap of its free
//! inodes, and a table of inodes; some groups also keep a backup of the
//! superblock (which describes the whole volume) and of the group
//! descriptor table (which says where each group's bitmaps and table are).
//!
//! An *inode* describes one file: type, permissions, owner, size, times,
//! link count, and where its data is: 12 direct block numbers, then a block
//! of block numbers (single indirect), a block of those (double) and one
//! more level (triple). A block number of 0 is a hole that reads as zeros.
//!
//! A directory is a file of variable-length entries (inode number, record
//! length, name length, file type, name). Deleting an entry merges its space
//! into the previous entry; adding one splits a record with room to spare.
//!
//! # This implementation
//!
//! Supported: revision 1 volumes with the `filetype`, `sparse_super` and
//! `large_file` features, block sizes 1-4 KiB, 128- or 256-byte inodes.
//! Volumes made by Linux's `mke2fs -t ext2` also have `resize_inode`,
//! `dir_index` and `ext_attr`, which are safe to leave alone: indexed
//! directories are read as plain ones, and lose their index flag when
//! changed (as the Linux ext2 driver does). Anything else (journals that
//! need replaying, extents, checksums) is refused or mounted read-only.
//!
//! Metadata blocks (bitmaps, inode tables, directories, indirect blocks)
//! go through a cache; changes are written back at the end of every
//! operation, so the disk is never more than one operation behind. File
//! data goes straight to the device, in runs of contiguous blocks.

#![no_std]

extern crate alloc;

mod data;
mod directory;
mod format;
mod layout;
#[cfg(test)]
mod tests;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

pub use self::format::{FormatOptions, has_superblock};
pub use self::layout::{FileType, Inode};
use self::layout::{GroupDescriptor, Superblock};

pub const SECTOR_SIZE: usize = 512;

/// The root directory's inode number.
pub const ROOT: u32 = 2;

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
    NameTooLong,
    /// A file would be bigger than the block map can describe.
    TooBig,
    /// A file would have more than 65,000 links.
    TooManyLinks,
    /// A directory can't move into itself.
    Loop,
    ReadOnly,
    /// The volume isn't ext2, or its structures don't make sense.
    Corrupt,
    /// Valid, but uses something this implementation doesn't handle.
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

/// A directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: alloc::string::String,
    pub inode: u32,
    pub kind: FileType,
}

/// What [`FileSystem::unlink`] left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unlinked {
    pub inode: u32,
    /// No names refer to the inode any more: once nothing has it open, it
    /// should be freed with [`FileSystem::release`].
    pub orphaned: bool,
}

/// Who a new file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// The most metadata blocks kept in the cache.
const CACHE_BLOCKS: usize = 2048;
/// The most links an inode may have (ext2's limit).
const MAX_LINKS: u16 = 65_000;
/// Incompatible features this implementation understands: only `filetype`.
const INCOMPAT_FILETYPE: u32 = 0x2;
/// Read-only-compatible features it maintains: `sparse_super` and
/// `large_file`. Volumes with others can still be read.
const RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2;
/// Superblock states.
const STATE_CLEAN: u16 = 1;
const STATE_DIRTY: u16 = 0;
/// The inode flag marking a hashed-index (`dir_index`) directory.
const INDEX_FLAG: u32 = 0x1000;

struct CachedBlock {
    data: Vec<u8>,
    dirty: bool,
    last_used: u64,
}

/// A mounted ext2 volume.
pub struct FileSystem<D: BlockDevice> {
    device: D,
    block_size: usize,
    superblock: Superblock,
    groups: Vec<GroupDescriptor>,
    /// Whether the superblock or a group descriptor needs writing.
    superblock_dirty: bool,
    groups_dirty: bool,
    cache: BTreeMap<u32, CachedBlock>,
    /// Counts cache uses, to find the least recently used blocks.
    clock_tick: u64,
    read_only: bool,
    /// Seconds since the Unix epoch, for timestamps.
    clock: fn() -> u64,
}

impl<D: BlockDevice> FileSystem<D> {
    /// Writes an empty volume on `device` (see [`FormatOptions`]).
    pub fn format(device: &mut D, options: &FormatOptions) -> Result<()> {
        format::format(device, options)
    }

    /// Mounts the volume on `device`. `read_only` refuses every change;
    /// volumes with features that can't be maintained are always read-only.
    pub fn mount(mut device: D, clock: fn() -> u64, read_only: bool) -> Result<Self> {
        let mut raw = vec![0; 1024];
        device.read(2, &mut raw)?;
        let superblock = Superblock::parse(raw)?;
        if superblock.feature_incompat & !INCOMPAT_FILETYPE != 0 {
            return Err(Error::Unsupported);
        }
        let read_only = read_only || superblock.feature_ro_compat & !RO_COMPAT_SUPPORTED != 0;
        let block_size = superblock.block_size();

        let group_count = superblock.group_count();
        let table_bytes = (group_count as usize * layout::GROUP_DESCRIPTOR_SIZE).next_multiple_of(block_size);
        let mut table = vec![0; table_bytes];
        let table_block = superblock.first_data_block + 1;
        device.read(u64::from(table_block) * (block_size / SECTOR_SIZE) as u64, &mut table)?;
        let groups = table
            .chunks(layout::GROUP_DESCRIPTOR_SIZE)
            .take(group_count as usize)
            .map(GroupDescriptor::parse)
            .collect();

        let mut fs = Self {
            device,
            block_size,
            superblock,
            groups,
            superblock_dirty: false,
            groups_dirty: false,
            cache: BTreeMap::new(),
            clock_tick: 0,
            read_only,
            clock,
        };
        fs.check_layout()?;
        if !fs.read_only {
            // Marked in use until unmounted cleanly, so a crash is noticed.
            let now = fs.now();
            fs.superblock.state = STATE_DIRTY;
            fs.superblock.mount_time = now;
            fs.superblock.mount_count = fs.superblock.mount_count.wrapping_add(1);
            fs.superblock_dirty = true;
            fs.flush()?;
        }
        Ok(fs)
    }

    /// Sanity checks that keep a damaged volume from sending reads and
    /// writes anywhere.
    fn check_layout(&self) -> Result<()> {
        let blocks = self.superblock.blocks_count;
        if u64::from(blocks) * (self.block_size / SECTOR_SIZE) as u64 > self.device.sectors() {
            return Err(Error::Corrupt);
        }
        let table_blocks = self.inode_table_blocks();
        for group in &self.groups {
            let inside =
                |block: u32, count: u32| block > 0 && block.checked_add(count).is_some_and(|end| end <= blocks);
            if !inside(group.block_bitmap, 1)
                || !inside(group.inode_bitmap, 1)
                || !inside(group.inode_table, table_blocks)
            {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }

    /// Writes everything back and marks the volume cleanly unmounted. The
    /// volume stays usable (it's marked in use again by the next change).
    pub fn unmount(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.superblock.state = STATE_CLEAN;
        self.superblock_dirty = true;
        self.flush()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Stops (or allows) changes, as `mount -o remount,ro` does. A volume
    /// with features this implementation can't maintain stays read-only.
    pub fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        if read_only && !self.read_only {
            self.unmount()?;
        }
        if !read_only && self.superblock.feature_ro_compat & !RO_COMPAT_SUPPORTED != 0 {
            return Err(Error::ReadOnly);
        }
        if !read_only && self.read_only {
            self.superblock.state = STATE_DIRTY;
            self.superblock_dirty = true;
        }
        self.read_only = read_only;
        self.flush()
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn total_blocks(&self) -> u64 {
        u64::from(self.superblock.blocks_count)
    }

    pub fn free_blocks(&self) -> u64 {
        u64::from(self.superblock.free_blocks)
    }

    pub fn total_inodes(&self) -> u64 {
        u64::from(self.superblock.inodes_count)
    }

    pub fn free_inodes(&self) -> u64 {
        u64::from(self.superblock.free_inodes)
    }

    /// The volume's label (`e2label`), if it has one.
    pub fn label(&self) -> alloc::string::String {
        self.superblock.label()
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.superblock.uuid
    }

    fn now(&self) -> u32 {
        (self.clock)().min(u64::from(u32::MAX)) as u32
    }

    fn writable(&self) -> Result<()> {
        if self.read_only { Err(Error::ReadOnly) } else { Ok(()) }
    }

    fn sectors_per_block(&self) -> u64 {
        (self.block_size / SECTOR_SIZE) as u64
    }

    // Raw blocks.

    /// Reads blocks straight from the device (for file data), bypassing the
    /// cache. Blocks still waiting in the cache are taken from there.
    fn read_blocks(&mut self, first: u32, buffer: &mut [u8]) -> Result<()> {
        self.device.read(u64::from(first) * self.sectors_per_block(), buffer)?;
        for (index, chunk) in buffer.chunks_mut(self.block_size).enumerate() {
            if let Some(cached) = self.cache.get(&(first + index as u32)) {
                chunk.copy_from_slice(&cached.data);
            }
        }
        Ok(())
    }

    /// Writes blocks straight to the device (for file data). Cached copies
    /// are dropped: a block freed from a directory may come back as data.
    fn write_blocks(&mut self, first: u32, data: &[u8]) -> Result<()> {
        for index in 0..(data.len() / self.block_size) as u32 {
            self.cache.remove(&(first + index));
        }
        self.device.write(u64::from(first) * self.sectors_per_block(), data)
    }

    /// A metadata block, through the cache.
    fn block(&mut self, number: u32) -> Result<&[u8]> {
        self.load(number)?;
        Ok(&self.cache.get(&number).expect("just loaded").data)
    }

    /// A metadata block to change; it's written back by the next
    /// [`flush`](Self::flush).
    fn block_mut(&mut self, number: u32) -> Result<&mut [u8]> {
        self.load(number)?;
        let cached = self.cache.get_mut(&number).expect("just loaded");
        cached.dirty = true;
        Ok(&mut cached.data)
    }

    /// A block that's about to be completely overwritten: no need to read
    /// it first.
    fn fresh_block(&mut self, number: u32) -> &mut [u8] {
        self.clock_tick += 1;
        let tick = self.clock_tick;
        let size = self.block_size;
        let cached = self.cache.entry(number).or_insert_with(|| CachedBlock {
            data: vec![0; size],
            dirty: true,
            last_used: tick,
        });
        cached.data.fill(0);
        cached.dirty = true;
        cached.last_used = tick;
        &mut cached.data
    }

    fn load(&mut self, number: u32) -> Result<()> {
        if number == 0 || number >= self.superblock.blocks_count {
            return Err(Error::Corrupt);
        }
        self.clock_tick += 1;
        let tick = self.clock_tick;
        if let Some(cached) = self.cache.get_mut(&number) {
            cached.last_used = tick;
            return Ok(());
        }
        if self.cache.len() >= CACHE_BLOCKS {
            self.evict()?;
        }
        let mut data = vec![0; self.block_size];
        self.device.read(u64::from(number) * self.sectors_per_block(), &mut data)?;
        self.cache.insert(number, CachedBlock { data, dirty: false, last_used: tick });
        Ok(())
    }

    /// Drops the least recently used quarter of the cache (writing back
    /// any changed blocks first).
    fn evict(&mut self) -> Result<()> {
        self.flush()?;
        let mut ages: Vec<(u64, u32)> = self.cache.iter().map(|(number, block)| (block.last_used, *number)).collect();
        ages.sort_unstable();
        for (_, number) in ages.iter().take(CACHE_BLOCKS / 4) {
            self.cache.remove(number);
        }
        Ok(())
    }

    /// Writes every changed block, group descriptor and the superblock to
    /// the device.
    pub fn flush(&mut self) -> Result<()> {
        let sectors_per_block = self.sectors_per_block();
        for (number, block) in self.cache.iter_mut().filter(|(_, block)| block.dirty) {
            self.device.write(u64::from(*number) * sectors_per_block, &block.data)?;
            block.dirty = false;
        }
        if self.groups_dirty {
            let mut table = vec![0; (self.groups.len() * layout::GROUP_DESCRIPTOR_SIZE).next_multiple_of(SECTOR_SIZE)];
            for (group, slot) in self.groups.iter().zip(table.chunks_mut(layout::GROUP_DESCRIPTOR_SIZE)) {
                group.write(slot);
            }
            let first = u64::from(self.superblock.first_data_block + 1) * sectors_per_block;
            self.device.write(first, &table)?;
            self.groups_dirty = false;
        }
        if self.superblock_dirty {
            self.superblock.write_time = self.now();
            let raw = self.superblock.serialize();
            self.device.write(2, &raw)?;
            self.superblock_dirty = false;
        }
        Ok(())
    }

    /// Runs a change, then writes it back whether it worked or not (a
    /// failure halfway may still have changed things that must stay
    /// consistent).
    fn change<T>(&mut self, operation: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.writable()?;
        let result = operation(self);
        let flushed = self.flush();
        let value = result?;
        flushed?;
        Ok(value)
    }

    // Inodes.

    fn inode_table_blocks(&self) -> u32 {
        (self.superblock.inodes_per_group * u32::from(self.superblock.inode_size)).div_ceil(self.block_size as u32)
    }

    /// Where inode `number` is stored: (block, offset in it).
    fn inode_location(&self, number: u32) -> Result<(u32, usize)> {
        if number == 0 || number > self.superblock.inodes_count {
            return Err(Error::Corrupt);
        }
        let index = number - 1;
        let group = (index / self.superblock.inodes_per_group) as usize;
        let within = (index % self.superblock.inodes_per_group) as usize;
        let inode_size = usize::from(self.superblock.inode_size);
        let offset = within * inode_size;
        let table = self.groups.get(group).ok_or(Error::Corrupt)?.inode_table;
        Ok((table + (offset / self.block_size) as u32, offset % self.block_size))
    }

    /// Reads inode `number`.
    pub fn inode(&mut self, number: u32) -> Result<Inode> {
        let (block, offset) = self.inode_location(number)?;
        let size = usize::from(self.superblock.inode_size);
        let data = self.block(block)?;
        Ok(Inode::parse(&data[offset..offset + size]))
    }

    fn write_inode(&mut self, number: u32, inode: &Inode) -> Result<()> {
        let (block, offset) = self.inode_location(number)?;
        let size = usize::from(self.superblock.inode_size);
        let data = self.block_mut(block)?;
        inode.write(&mut data[offset..offset + size]);
        Ok(())
    }

    /// Changes an inode's permissions, owner, or modification time.
    pub fn set_attributes(
        &mut self,
        number: u32,
        permissions: Option<u16>,
        owner: Option<Owner>,
        modified: Option<u32>,
    ) -> Result<Inode> {
        self.change(|fs| {
            let mut inode = fs.inode(number)?;
            if let Some(permissions) = permissions {
                inode.mode = (inode.mode & layout::TYPE_MASK) | (permissions & 0o7777);
            }
            if let Some(owner) = owner {
                inode.uid = owner.uid;
                inode.gid = owner.gid;
            }
            if let Some(modified) = modified {
                inode.modified = modified;
            }
            inode.changed = fs.now();
            fs.write_inode(number, &inode)?;
            Ok(inode)
        })
    }

    // Allocation.

    fn group_of_block(&self, block: u32) -> usize {
        ((block - self.superblock.first_data_block) / self.superblock.blocks_per_group) as usize
    }

    /// Takes a free block, preferably `goal` or the next free one after it
    /// in the same group (so files stay contiguous).
    fn allocate_block(&mut self, goal: u32) -> Result<u32> {
        let groups = self.groups.len();
        let goal = goal.clamp(self.superblock.first_data_block, self.superblock.blocks_count - 1);
        let goal_group = self.group_of_block(goal);
        for step in 0..groups {
            let group = (goal_group + step) % groups;
            if self.groups[group].free_blocks == 0 {
                continue;
            }
            let first_block = self.superblock.first_data_block + group as u32 * self.superblock.blocks_per_group;
            let in_group = (self.superblock.blocks_count - first_block).min(self.superblock.blocks_per_group);
            let start = if step == 0 { goal - first_block } else { 0 };
            let bitmap_block = self.groups[group].block_bitmap;
            let bitmap = self.block(bitmap_block)?;
            let Some(bit) = layout::find_clear_bit(bitmap, start, in_group) else { continue };
            self.block_mut(bitmap_block)?[bit as usize / 8] |= 1 << (bit % 8);
            self.groups[group].free_blocks -= 1;
            self.superblock.free_blocks = self.superblock.free_blocks.saturating_sub(1);
            self.groups_dirty = true;
            self.superblock_dirty = true;
            return Ok(first_block + bit);
        }
        Err(Error::NoSpace)
    }

    fn free_block(&mut self, block: u32) -> Result<()> {
        if block < self.superblock.first_data_block || block >= self.superblock.blocks_count {
            return Err(Error::Corrupt);
        }
        let group = self.group_of_block(block);
        let bit = (block - self.superblock.first_data_block) % self.superblock.blocks_per_group;
        let bitmap_block = self.groups[group].block_bitmap;
        let byte = &mut self.block_mut(bitmap_block)?[bit as usize / 8];
        if *byte & (1 << (bit % 8)) == 0 {
            return Err(Error::Corrupt);
        }
        *byte &= !(1 << (bit % 8));
        self.cache.remove(&block);
        self.groups[group].free_blocks += 1;
        self.superblock.free_blocks += 1;
        self.groups_dirty = true;
        self.superblock_dirty = true;
        Ok(())
    }

    /// Takes a free inode. Directories go to the group with the most free
    /// inodes (spreading the tree out); files stay near their directory.
    fn allocate_inode(&mut self, directory: bool, parent: u32) -> Result<u32> {
        let groups = self.groups.len();
        let parent_group = ((parent.max(1) - 1) / self.superblock.inodes_per_group) as usize % groups;
        let order: Vec<usize> = if directory {
            let mut order: Vec<usize> = (0..groups).collect();
            order.sort_by_key(|&group| {
                core::cmp::Reverse((self.groups[group].free_inodes, self.groups[group].free_blocks))
            });
            order
        } else {
            (0..groups).map(|step| (parent_group + step) % groups).collect()
        };
        for group in order {
            if self.groups[group].free_inodes == 0 {
                continue;
            }
            // The reserved inodes (below the first ordinary one) are never
            // handed out.
            let first_inode = group as u32 * self.superblock.inodes_per_group + 1;
            let start = self.superblock.first_inode.saturating_sub(first_inode);
            let bitmap_block = self.groups[group].inode_bitmap;
            let per_group = self.superblock.inodes_per_group;
            let bitmap = self.block(bitmap_block)?;
            let Some(bit) = layout::find_clear_bit(bitmap, start, per_group) else { continue };
            self.block_mut(bitmap_block)?[bit as usize / 8] |= 1 << (bit % 8);
            self.groups[group].free_inodes -= 1;
            if directory {
                self.groups[group].used_directories += 1;
            }
            self.superblock.free_inodes = self.superblock.free_inodes.saturating_sub(1);
            self.groups_dirty = true;
            self.superblock_dirty = true;
            return Ok(first_inode + bit);
        }
        Err(Error::NoSpace)
    }

    fn free_inode(&mut self, number: u32, directory: bool) -> Result<()> {
        let index = number - 1;
        let group = (index / self.superblock.inodes_per_group) as usize;
        let bit = index % self.superblock.inodes_per_group;
        let bitmap_block = self.groups.get(group).ok_or(Error::Corrupt)?.inode_bitmap;
        let byte = &mut self.block_mut(bitmap_block)?[bit as usize / 8];
        *byte &= !(1 << (bit % 8));
        self.groups[group].free_inodes += 1;
        if directory {
            self.groups[group].used_directories = self.groups[group].used_directories.saturating_sub(1);
        }
        self.superblock.free_inodes += 1;
        self.groups_dirty = true;
        self.superblock_dirty = true;
        Ok(())
    }

    /// The first block of the group inode `number` lives in: where its
    /// data should go.
    fn goal_for(&self, number: u32) -> u32 {
        let group = (number - 1) / self.superblock.inodes_per_group;
        self.superblock.first_data_block + group * self.superblock.blocks_per_group
    }

    /// A new inode of type and permissions `mode`, with one link (two for
    /// a directory: its entry and its own `.`).
    fn new_inode(&mut self, mode: u16, owner: Owner, parent: u32) -> Result<(u32, Inode)> {
        let directory = mode & layout::TYPE_MASK == layout::TYPE_DIRECTORY;
        let number = self.allocate_inode(directory, parent)?;
        let now = self.now();
        let inode = Inode {
            mode,
            uid: owner.uid,
            gid: owner.gid,
            size: 0,
            accessed: now,
            changed: now,
            modified: now,
            created: now,
            deleted: 0,
            links: if directory { 2 } else { 1 },
            sectors: 0,
            flags: 0,
            block: [0; 15],
            extra_size: self.superblock.want_extra_size(),
        };
        self.write_inode(number, &inode)?;
        Ok((number, inode))
    }

    /// Frees an inode no name refers to any more, and its blocks. Call it
    /// once nothing has the file open (see [`Unlinked`]).
    pub fn release(&mut self, number: u32) -> Result<()> {
        self.change(|fs| {
            let mut inode = fs.inode(number)?;
            if inode.links != 0 || inode.mode == 0 {
                return Ok(());
            }
            if !inode.is_fast_symlink() {
                fs.free_data(&mut inode, 0)?;
            }
            inode.size = 0;
            inode.deleted = fs.now();
            fs.write_inode(number, &inode)?;
            fs.free_inode(number, inode.kind() == FileType::Directory)
        })
    }

    /// Clears a directory's hashed-index flag before it's changed: the
    /// index would no longer match (Linux rebuilds it with `e2fsck -D`).
    fn drop_index(&mut self, directory: u32, inode: &mut Inode) -> Result<()> {
        if inode.flags & INDEX_FLAG != 0 {
            inode.flags &= !INDEX_FLAG;
            self.write_inode(directory, inode)?;
        }
        Ok(())
    }
}
