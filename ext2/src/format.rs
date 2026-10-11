//! Formatting: writing an empty ext2 volume, the way `mke2fs -t ext2`
//! would (minus the features this implementation doesn't maintain).

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::{self, GROUP_DESCRIPTOR_SIZE, GroupDescriptor, Inode, TYPE_DIRECTORY};
use crate::{BlockDevice, Error, FileType, ROOT, Result, SECTOR_SIZE};

/// The inode size used for new volumes, with room for nanosecond times
/// and a creation time (as mke2fs does).
const INODE_SIZE: u32 = 256;
const EXTRA_SIZE: u16 = 32;
const FIRST_INODE: u32 = 11;
const LOST_AND_FOUND: u32 = 11;
/// A group too small to hold this many data blocks besides its metadata
/// isn't worth having.
const MIN_GROUP_DATA: u32 = 64;

#[derive(Debug, Clone)]
pub struct FormatOptions<'a> {
    /// The volume label (up to 16 bytes).
    pub label: &'a str,
    pub uuid: [u8; 16],
    /// Seconds since the Unix epoch, for the timestamps.
    pub time: u32,
    /// 1024, 2048 or 4096; chosen from the size if `None`.
    pub block_size: Option<usize>,
}

/// Whether a group keeps a backup of the superblock and group descriptors
/// (`sparse_super`: groups 0 and 1, and powers of 3, 5 and 7).
pub fn has_superblock(group: u32) -> bool {
    if group <= 1 {
        return true;
    }
    [3u32, 5, 7].iter().any(|&base| {
        let mut power = base;
        while power < group {
            power = power.saturating_mul(base);
        }
        power == group
    })
}

struct Geometry {
    blocks: u32,
    first_data_block: u32,
    blocks_per_group: u32,
    groups: u32,
    inodes_per_group: u32,
    table_blocks: u32,
    descriptor_blocks: u32,
}

impl Geometry {
    fn new(sectors: u64, block_size: usize) -> Result<Self> {
        let blocks_available = (sectors / (block_size / SECTOR_SIZE) as u64).min(u64::from(u32::MAX)) as u32;
        let first_data_block = u32::from(block_size == 1024);
        let blocks_per_group = 8 * block_size as u32;
        let bytes = u64::from(blocks_available) * block_size as u64;
        // One inode per 16 KiB (as mke2fs does), or 64 KiB on big disks,
        // where files tend to be bigger and zeroing the tables costs time.
        let ratio = if bytes >= 4 << 30 { 65536 } else { 16384 };
        let mut blocks = blocks_available;
        loop {
            if blocks <= first_data_block {
                return Err(Error::NoSpace);
            }
            let groups = (blocks - first_data_block).div_ceil(blocks_per_group);
            let per_block = block_size as u32 / INODE_SIZE;
            let wanted = (u64::from(blocks) * block_size as u64 / ratio).max(u64::from(groups) * 16);
            let inodes_per_group = (wanted.div_ceil(u64::from(groups)) as u32)
                .next_multiple_of(per_block.max(8))
                .min(8 * block_size as u32);
            let table_blocks = inodes_per_group / per_block;
            let descriptor_blocks = (groups as usize * GROUP_DESCRIPTOR_SIZE).div_ceil(block_size) as u32;
            let geometry = Self {
                blocks,
                first_data_block,
                blocks_per_group,
                groups,
                inodes_per_group,
                table_blocks,
                descriptor_blocks,
            };
            let last = groups - 1;
            if geometry.group_blocks(last) >= geometry.overhead(last) + MIN_GROUP_DATA {
                return Ok(geometry);
            }
            // The last group would be too small: leave its blocks unused.
            if groups == 1 {
                return Err(Error::NoSpace);
            }
            blocks = first_data_block + last * blocks_per_group;
        }
    }

    fn group_start(&self, group: u32) -> u32 {
        self.first_data_block + group * self.blocks_per_group
    }

    fn group_blocks(&self, group: u32) -> u32 {
        (self.blocks - self.group_start(group)).min(self.blocks_per_group)
    }

    /// Blocks at the start of a group taken by its metadata.
    fn overhead(&self, group: u32) -> u32 {
        let backup = if has_superblock(group) { 1 + self.descriptor_blocks } else { 0 };
        backup + 2 + self.table_blocks
    }
}

/// Fills a bitmap's bits from `from` to the end of the block (bits for
/// blocks or inodes that don't exist must read as used).
fn set_bits(bitmap: &mut [u8], from: u32, to: u32) {
    for bit in from..to {
        bitmap[bit as usize / 8] |= 1 << (bit % 8);
    }
}

fn write_block(device: &mut impl BlockDevice, block_size: usize, block: u32, data: &[u8]) -> Result<()> {
    device.write(u64::from(block) * (block_size / SECTOR_SIZE) as u64, data)
}

/// Writes an empty ext2 volume on `device`: a root directory (owned by
/// root, 0755) and `lost+found`.
pub fn format(device: &mut impl BlockDevice, options: &FormatOptions) -> Result<()> {
    let sectors = device.sectors();
    let block_size = options.block_size.unwrap_or(if sectors * 512 >= 512 << 20 { 4096 } else { 1024 });
    if ![1024, 2048, 4096].contains(&block_size) {
        return Err(Error::Unsupported);
    }
    let geometry = Geometry::new(sectors, block_size)?;
    let g = &geometry;
    let inodes = g.inodes_per_group * g.groups;

    // Group 0 holds the root directory and lost+found; their blocks come
    // right after its metadata.
    // 16 KiB (as mke2fs makes it), within the direct blocks.
    let lost_found_blocks = (16384 / block_size as u32).clamp(1, 12);
    let root_block = g.group_start(0) + g.overhead(0);
    let lost_found_first = root_block + 1;

    let mut descriptors = Vec::new();
    let mut free_blocks = 0;
    let mut free_inodes = 0;
    let zeros = vec![0u8; block_size * 64];
    for group in 0..g.groups {
        let start = g.group_start(group);
        let metadata = start + if has_superblock(group) { 1 + g.descriptor_blocks } else { 0 };
        let mut descriptor = GroupDescriptor::new(metadata, metadata + 1, metadata + 2);

        let mut block_bitmap = vec![0u8; block_size];
        let mut used = g.overhead(group);
        if group == 0 {
            used += 1 + lost_found_blocks;
            descriptor.used_directories = 2;
        }
        set_bits(&mut block_bitmap, 0, used);
        set_bits(&mut block_bitmap, g.group_blocks(group), 8 * block_size as u32);
        let mut inode_bitmap = vec![0u8; block_size];
        let reserved = if group == 0 { FIRST_INODE } else { 0 };
        set_bits(&mut inode_bitmap, 0, reserved);
        set_bits(&mut inode_bitmap, g.inodes_per_group, 8 * block_size as u32);
        descriptor.free_blocks = (g.group_blocks(group) - used) as u16;
        descriptor.free_inodes = (g.inodes_per_group - reserved) as u16;
        free_blocks += g.group_blocks(group) - used;
        free_inodes += g.inodes_per_group - reserved;

        write_block(device, block_size, descriptor.block_bitmap, &block_bitmap)?;
        write_block(device, block_size, descriptor.inode_bitmap, &inode_bitmap)?;
        let mut table_block = 0;
        while table_block < g.table_blocks {
            let count = (g.table_blocks - table_block).min(64);
            let first = descriptor.inode_table + table_block;
            write_block(device, block_size, first, &zeros[..count as usize * block_size])?;
            table_block += count;
        }
        descriptors.push(descriptor);
    }

    // The superblock, then a copy (with its own group number) in every
    // backup group, each followed by the descriptor table.
    let mut superblock = vec![0u8; 1024];
    let put32 = |data: &mut [u8], at: usize, value: u32| data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    let put16 = |data: &mut [u8], at: usize, value: u16| data[at..at + 2].copy_from_slice(&value.to_le_bytes());
    put32(&mut superblock, 0, inodes);
    put32(&mut superblock, 4, g.blocks);
    put32(&mut superblock, 12, free_blocks);
    put32(&mut superblock, 16, free_inodes);
    put32(&mut superblock, 20, g.first_data_block);
    put32(&mut superblock, 24, (block_size / 1024).trailing_zeros());
    put32(&mut superblock, 28, (block_size / 1024).trailing_zeros());
    put32(&mut superblock, 32, g.blocks_per_group);
    put32(&mut superblock, 36, g.blocks_per_group);
    put32(&mut superblock, 40, g.inodes_per_group);
    put32(&mut superblock, 48, options.time);
    put16(&mut superblock, 54, u16::MAX);
    put16(&mut superblock, 56, layout::MAGIC);
    put16(&mut superblock, 58, 1);
    put16(&mut superblock, 60, 1);
    put32(&mut superblock, 64, options.time);
    put32(&mut superblock, 76, 1);
    put32(&mut superblock, 84, FIRST_INODE);
    put16(&mut superblock, 88, INODE_SIZE as u16);
    put32(&mut superblock, 96, crate::INCOMPAT_FILETYPE);
    put32(&mut superblock, 100, crate::RO_COMPAT_SUPPORTED);
    superblock[104..120].copy_from_slice(&options.uuid);
    let label = options.label.as_bytes();
    superblock[120..120 + label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
    put32(&mut superblock, 264, options.time);
    put16(&mut superblock, 348, EXTRA_SIZE);
    put16(&mut superblock, 350, EXTRA_SIZE);

    let mut table = vec![0u8; g.descriptor_blocks as usize * block_size];
    for (descriptor, slot) in descriptors.iter().zip(table.chunks_mut(GROUP_DESCRIPTOR_SIZE)) {
        descriptor.write(slot);
    }
    for group in (0..g.groups).filter(|&group| has_superblock(group)) {
        put16(&mut superblock, 90, group as u16);
        if group == 0 {
            // At byte 1024, whatever the block size.
            device.write(2, &superblock)?;
        } else {
            let mut block = vec![0u8; block_size];
            block[..1024].copy_from_slice(&superblock);
            write_block(device, block_size, g.group_start(group), &block)?;
        }
        let table_at = if group == 0 { g.first_data_block + 1 } else { g.group_start(group) + 1 };
        write_block(device, block_size, table_at, &table)?;
    }

    // The root directory and lost+found.
    let directory_code = FileType::Directory.directory_code();
    let record = |block: &mut [u8], offset: usize, inode: u32, length: usize, name: &[u8]| {
        block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
        block[offset + 4..offset + 6].copy_from_slice(&(length as u16).to_le_bytes());
        block[offset + 6] = name.len() as u8;
        block[offset + 7] = directory_code;
        block[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
    };
    let mut root = vec![0u8; block_size];
    record(&mut root, 0, ROOT, 12, b".");
    record(&mut root, 12, ROOT, 12, b"..");
    record(&mut root, 24, LOST_AND_FOUND, block_size - 24, b"lost+found");
    write_block(device, block_size, root_block, &root)?;
    let mut lost_found = vec![0u8; block_size];
    record(&mut lost_found, 0, LOST_AND_FOUND, 12, b".");
    record(&mut lost_found, 12, ROOT, block_size - 12, b"..");
    write_block(device, block_size, lost_found_first, &lost_found)?;
    let mut empty = vec![0u8; block_size];
    empty[4..6].copy_from_slice(&(block_size as u16).to_le_bytes());
    for index in 1..lost_found_blocks {
        write_block(device, block_size, lost_found_first + index, &empty)?;
    }

    let directory = |permissions: u16, links: u16, first: u32, count: u32| {
        let mut inode = Inode {
            mode: TYPE_DIRECTORY | permissions,
            uid: 0,
            gid: 0,
            size: u64::from(count) * block_size as u64,
            accessed: options.time,
            changed: options.time,
            modified: options.time,
            created: options.time,
            deleted: 0,
            links,
            sectors: count * (block_size / SECTOR_SIZE) as u32,
            flags: 0,
            block: [0; 15],
            extra_size: EXTRA_SIZE,
        };
        for (index, block) in inode.block.iter_mut().take(count as usize).enumerate() {
            *block = first + index as u32;
        }
        inode
    };
    // root: its entry in itself, `.`, and lost+found's `..`.
    let inodes_per_block = block_size / INODE_SIZE as usize;
    let mut table_block = vec![0u8; block_size];
    let slot = |number: u32| ((number - 1) as usize % inodes_per_block) * INODE_SIZE as usize;
    directory(0o755, 3, root_block, 1).write(&mut table_block[slot(ROOT)..slot(ROOT) + INODE_SIZE as usize]);
    let table_of = |number: u32| descriptors[0].inode_table + (number - 1) / inodes_per_block as u32;
    if table_of(LOST_AND_FOUND) == table_of(ROOT) {
        let lost = directory(0o700, 2, lost_found_first, lost_found_blocks);
        lost.write(&mut table_block[slot(LOST_AND_FOUND)..slot(LOST_AND_FOUND) + INODE_SIZE as usize]);
        write_block(device, block_size, table_of(ROOT), &table_block)?;
    } else {
        write_block(device, block_size, table_of(ROOT), &table_block)?;
        let mut other = vec![0u8; block_size];
        let lost = directory(0o700, 2, lost_found_first, lost_found_blocks);
        lost.write(&mut other[slot(LOST_AND_FOUND)..slot(LOST_AND_FOUND) + INODE_SIZE as usize]);
        write_block(device, block_size, table_of(LOST_AND_FOUND), &other)?;
    }
    Ok(())
}
