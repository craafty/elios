//! The on-disk structures: the superblock, group descriptors and inodes.
//! All little-endian.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Error, Result};

pub const MAGIC: u16 = 0xEF53;
pub const GROUP_DESCRIPTOR_SIZE: usize = 32;

// File types in the top four bits of an inode's mode.
pub const TYPE_MASK: u16 = 0o170000;
pub const TYPE_FIFO: u16 = 0o010000;
pub const TYPE_CHAR_DEVICE: u16 = 0o020000;
pub const TYPE_DIRECTORY: u16 = 0o040000;
pub const TYPE_BLOCK_DEVICE: u16 = 0o060000;
pub const TYPE_REGULAR: u16 = 0o100000;
pub const TYPE_SYMLINK: u16 = 0o120000;
pub const TYPE_SOCKET: u16 = 0o140000;

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn put_u16(data: &mut [u8], at: usize, value: u16) {
    data[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The superblock: the volume's geometry, counters and features. Fields
/// this implementation doesn't use are kept as they were, in `raw`.
pub struct Superblock {
    raw: Vec<u8>,
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub mount_time: u32,
    pub write_time: u32,
    pub mount_count: u16,
    pub state: u16,
    pub first_inode: u32,
    pub inode_size: u16,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
}

impl Superblock {
    pub fn parse(raw: Vec<u8>) -> Result<Self> {
        if u16_at(&raw, 56) != MAGIC {
            return Err(Error::Corrupt);
        }
        let revision = u32_at(&raw, 76);
        let (first_inode, inode_size) = if revision == 0 { (11, 128) } else { (u32_at(&raw, 84), u16_at(&raw, 88)) };
        let superblock = Self {
            inodes_count: u32_at(&raw, 0),
            blocks_count: u32_at(&raw, 4),
            free_blocks: u32_at(&raw, 12),
            free_inodes: u32_at(&raw, 16),
            first_data_block: u32_at(&raw, 20),
            log_block_size: u32_at(&raw, 24),
            blocks_per_group: u32_at(&raw, 32),
            inodes_per_group: u32_at(&raw, 40),
            mount_time: u32_at(&raw, 44),
            write_time: u32_at(&raw, 48),
            mount_count: u16_at(&raw, 52),
            state: u16_at(&raw, 58),
            first_inode,
            inode_size,
            feature_incompat: if revision == 0 { 0 } else { u32_at(&raw, 96) },
            feature_ro_compat: if revision == 0 { 0 } else { u32_at(&raw, 100) },
            uuid: raw[104..120].try_into().unwrap(),
            raw,
        };
        let valid = superblock.log_block_size <= 2
            && superblock.blocks_per_group > 0
            && superblock.blocks_per_group <= 8 * superblock.block_size() as u32
            && superblock.inodes_per_group > 0
            && superblock.inodes_per_group <= 8 * superblock.block_size() as u32
            && superblock.inode_size >= 128
            && superblock.inode_size.is_power_of_two()
            && usize::from(superblock.inode_size) <= superblock.block_size()
            && superblock.blocks_count > superblock.first_data_block
            && superblock.inodes_count == superblock.inodes_per_group * superblock.group_count();
        if !valid {
            return Err(Error::Corrupt);
        }
        Ok(superblock)
    }

    pub fn block_size(&self) -> usize {
        1024 << self.log_block_size
    }

    pub fn group_count(&self) -> u32 {
        (self.blocks_count - self.first_data_block).div_ceil(self.blocks_per_group)
    }

    /// The extra inode space new inodes reserve (`s_want_extra_isize`).
    pub fn want_extra_size(&self) -> u16 {
        if self.inode_size > 128 { u16_at(&self.raw, 350).max(32).min(self.inode_size - 128) } else { 0 }
    }

    pub fn label(&self) -> String {
        let name = &self.raw[120..136];
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        String::from_utf8_lossy(&name[..len]).into()
    }

    pub fn serialize(&mut self) -> Vec<u8> {
        let raw = &mut self.raw;
        put_u32(raw, 12, self.free_blocks);
        put_u32(raw, 16, self.free_inodes);
        put_u32(raw, 44, self.mount_time);
        put_u32(raw, 48, self.write_time);
        put_u16(raw, 52, self.mount_count);
        put_u16(raw, 58, self.state);
        raw.clone()
    }
}

/// Where a block group's bitmaps and inode table are, and its counters.
pub struct GroupDescriptor {
    raw: [u8; GROUP_DESCRIPTOR_SIZE],
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks: u16,
    pub free_inodes: u16,
    pub used_directories: u16,
}

impl GroupDescriptor {
    pub fn parse(data: &[u8]) -> Self {
        Self {
            raw: data.try_into().unwrap(),
            block_bitmap: u32_at(data, 0),
            inode_bitmap: u32_at(data, 4),
            inode_table: u32_at(data, 8),
            free_blocks: u16_at(data, 12),
            free_inodes: u16_at(data, 14),
            used_directories: u16_at(data, 16),
        }
    }

    pub fn new(block_bitmap: u32, inode_bitmap: u32, inode_table: u32) -> Self {
        Self {
            raw: [0; GROUP_DESCRIPTOR_SIZE],
            block_bitmap,
            inode_bitmap,
            inode_table,
            free_blocks: 0,
            free_inodes: 0,
            used_directories: 0,
        }
    }

    pub fn write(&self, slot: &mut [u8]) {
        slot.copy_from_slice(&self.raw);
        put_u32(slot, 0, self.block_bitmap);
        put_u32(slot, 4, self.inode_bitmap);
        put_u32(slot, 8, self.inode_table);
        put_u16(slot, 12, self.free_blocks);
        put_u16(slot, 14, self.free_inodes);
        put_u16(slot, 16, self.used_directories);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    Unknown,
}

impl FileType {
    pub fn from_mode(mode: u16) -> Self {
        match mode & TYPE_MASK {
            TYPE_REGULAR => FileType::Regular,
            TYPE_DIRECTORY => FileType::Directory,
            TYPE_SYMLINK => FileType::Symlink,
            TYPE_CHAR_DEVICE => FileType::CharDevice,
            TYPE_BLOCK_DEVICE => FileType::BlockDevice,
            TYPE_FIFO => FileType::Fifo,
            TYPE_SOCKET => FileType::Socket,
            _ => FileType::Unknown,
        }
    }

    pub fn mode_bits(self) -> u16 {
        match self {
            FileType::Regular | FileType::Unknown => TYPE_REGULAR,
            FileType::Directory => TYPE_DIRECTORY,
            FileType::Symlink => TYPE_SYMLINK,
            FileType::CharDevice => TYPE_CHAR_DEVICE,
            FileType::BlockDevice => TYPE_BLOCK_DEVICE,
            FileType::Fifo => TYPE_FIFO,
            FileType::Socket => TYPE_SOCKET,
        }
    }

    /// The type byte in a directory entry (the `filetype` feature).
    pub fn directory_code(self) -> u8 {
        match self {
            FileType::Unknown => 0,
            FileType::Regular => 1,
            FileType::Directory => 2,
            FileType::CharDevice => 3,
            FileType::BlockDevice => 4,
            FileType::Fifo => 5,
            FileType::Socket => 6,
            FileType::Symlink => 7,
        }
    }

    pub fn from_directory_code(code: u8) -> Self {
        match code {
            1 => FileType::Regular,
            2 => FileType::Directory,
            3 => FileType::CharDevice,
            4 => FileType::BlockDevice,
            5 => FileType::Fifo,
            6 => FileType::Socket,
            7 => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }
}

/// An inode: everything about a file but its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    /// File type and permission bits.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub accessed: u32,
    pub changed: u32,
    pub modified: u32,
    pub created: u32,
    pub deleted: u32,
    pub links: u16,
    /// Space used, in 512-byte sectors (including indirect blocks).
    pub sectors: u32,
    pub flags: u32,
    /// Data block numbers: 12 direct, then single, double and triple
    /// indirect. A fast symbolic link keeps its target here instead.
    pub block: [u32; 15],
    /// How much of the space past the first 128 bytes is in use.
    pub extra_size: u16,
}

impl Inode {
    pub fn parse(data: &[u8]) -> Self {
        let mut block = [0; 15];
        for (index, slot) in block.iter_mut().enumerate() {
            *slot = u32_at(data, 40 + index * 4);
        }
        let extra_size = if data.len() > 128 { u16_at(data, 128) } else { 0 };
        Self {
            mode: u16_at(data, 0),
            uid: u32::from(u16_at(data, 2)) | u32::from(u16_at(data, 120)) << 16,
            size: u64::from(u32_at(data, 4)) | u64::from(u32_at(data, 108)) << 32,
            accessed: u32_at(data, 8),
            changed: u32_at(data, 12),
            modified: u32_at(data, 16),
            deleted: u32_at(data, 20),
            gid: u32::from(u16_at(data, 24)) | u32::from(u16_at(data, 122)) << 16,
            links: u16_at(data, 26),
            sectors: u32_at(data, 28),
            flags: u32_at(data, 32),
            block,
            extra_size,
            created: if extra_size >= 24 && data.len() >= 148 { u32_at(data, 144) } else { 0 },
        }
    }

    /// Writes the fields this implementation knows into an on-disk inode,
    /// leaving the rest (extended attribute blocks and the like) alone.
    pub fn write(&self, data: &mut [u8]) {
        put_u16(data, 0, self.mode);
        put_u16(data, 2, self.uid as u16);
        put_u32(data, 4, self.size as u32);
        put_u32(data, 8, self.accessed);
        put_u32(data, 12, self.changed);
        put_u32(data, 16, self.modified);
        put_u32(data, 20, self.deleted);
        put_u16(data, 24, self.gid as u16);
        put_u16(data, 26, self.links);
        put_u32(data, 28, self.sectors);
        put_u32(data, 32, self.flags);
        for (index, block) in self.block.iter().enumerate() {
            put_u32(data, 40 + index * 4, *block);
        }
        put_u32(data, 108, (self.size >> 32) as u32);
        put_u16(data, 120, (self.uid >> 16) as u16);
        put_u16(data, 122, (self.gid >> 16) as u16);
        if data.len() > 128 {
            put_u16(data, 128, self.extra_size);
            if self.extra_size >= 24 && data.len() >= 148 {
                put_u32(data, 144, self.created);
            }
        }
    }

    pub fn kind(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    pub fn permissions(&self) -> u16 {
        self.mode & 0o7777
    }

    /// A symbolic link whose target is stored in the block map itself.
    pub fn is_fast_symlink(&self) -> bool {
        self.kind() == FileType::Symlink && self.sectors == 0 && self.size < 60
    }
}

/// The first clear bit at or after `start` and before `end`.
pub fn find_clear_bit(bitmap: &[u8], start: u32, end: u32) -> Option<u32> {
    let mut bit = start;
    while bit < end {
        let byte = bitmap[bit as usize / 8];
        if byte == 0xFF && bit.is_multiple_of(8) {
            bit += 8;
            continue;
        }
        if byte & (1 << (bit % 8)) == 0 {
            return Some(bit);
        }
        bit += 1;
    }
    None
}
