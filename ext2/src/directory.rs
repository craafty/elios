//! Directories: listing, looking up, adding and removing entries, and the
//! operations built on them (create, link, unlink, rename, symbolic links).
//!
//! An entry is: inode number (4 bytes), record length (2), name length (1),
//! file type (1), then the name, padded so the record length is a multiple
//! of 4. Records exactly fill each block; the last one in a block takes up
//! whatever space is left.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::layout::Inode;
use crate::{
    BlockDevice, DirEntry, Error, FileSystem, FileType, INCOMPAT_FILETYPE, MAX_LINKS, Owner, Result, Unlinked,
};

const HEADER: usize = 8;

/// The space a record with a `name_len`-byte name needs.
fn record_size(name_len: usize) -> usize {
    (HEADER + name_len).next_multiple_of(4)
}

fn valid_name(name: &str) -> Result<()> {
    match name {
        "" | "." | ".." => Err(Error::InvalidName),
        _ if name.contains(['/', '\0']) => Err(Error::InvalidName),
        _ if name.len() > 255 => Err(Error::NameTooLong),
        _ => Ok(()),
    }
}

/// One record in a directory block.
struct Record {
    offset: usize,
    inode: u32,
    length: usize,
    name_len: usize,
    code: u8,
}

/// The records in a directory block, checked so a damaged one can't send
/// reads past the block.
fn records(block: &[u8]) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < block.len() {
        if offset + HEADER > block.len() {
            return Err(Error::Corrupt);
        }
        let inode = u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap());
        let length = usize::from(u16::from_le_bytes([block[offset + 4], block[offset + 5]]));
        let name_len = usize::from(block[offset + 6]);
        if length < HEADER || length % 4 != 0 || offset + length > block.len() || HEADER + name_len > length {
            return Err(Error::Corrupt);
        }
        records.push(Record { offset, inode, length, name_len, code: block[offset + 7] });
        offset += length;
    }
    Ok(records)
}

fn write_record(block: &mut [u8], offset: usize, inode: u32, length: usize, name: &[u8], code: u8) {
    block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
    block[offset + 4..offset + 6].copy_from_slice(&(length as u16).to_le_bytes());
    block[offset + 6] = name.len() as u8;
    block[offset + 7] = code;
    block[offset + HEADER..offset + HEADER + name.len()].copy_from_slice(name);
}

impl<D: BlockDevice> FileSystem<D> {
    fn has_file_types(&self) -> bool {
        self.superblock.feature_incompat & INCOMPAT_FILETYPE != 0
    }

    fn directory_inode(&mut self, number: u32) -> Result<Inode> {
        let inode = self.inode(number)?;
        if inode.kind() != FileType::Directory {
            return Err(Error::NotDirectory);
        }
        Ok(inode)
    }

    /// The device blocks of a directory, in order.
    fn directory_blocks(&mut self, inode: &Inode) -> Result<Vec<u32>> {
        let count = inode.size.div_ceil(self.block_size as u64);
        (0..count).map(|index| self.map(inode, index)).collect()
    }

    /// Every entry, including `.` and `..`.
    fn all_entries(&mut self, number: u32) -> Result<Vec<DirEntry>> {
        let inode = self.directory_inode(number)?;
        let mut entries = Vec::new();
        let file_types = self.has_file_types();
        for block in self.directory_blocks(&inode)? {
            if block == 0 {
                continue;
            }
            let data = self.block(block)?;
            for record in records(data)?.iter().filter(|record| record.inode != 0) {
                let name = &data[record.offset + HEADER..record.offset + HEADER + record.name_len];
                let kind = if file_types { FileType::from_directory_code(record.code) } else { FileType::Unknown };
                entries.push(DirEntry { name: String::from_utf8_lossy(name).into(), inode: record.inode, kind });
            }
        }
        for entry in entries.iter_mut().filter(|entry| entry.kind == FileType::Unknown) {
            entry.kind = self.inode(entry.inode)?.kind();
        }
        Ok(entries)
    }

    /// A directory's entries, without `.` and `..`.
    pub fn list(&mut self, directory: u32) -> Result<Vec<DirEntry>> {
        let mut entries = self.all_entries(directory)?;
        entries.retain(|entry| entry.name != "." && entry.name != "..");
        Ok(entries)
    }

    /// The inode number of entry `name` in a directory.
    pub fn lookup(&mut self, directory: u32, name: &str) -> Result<u32> {
        let inode = self.directory_inode(directory)?;
        for block in self.directory_blocks(&inode)? {
            if block == 0 {
                continue;
            }
            let data = self.block(block)?;
            for record in records(data)? {
                if record.inode != 0
                    && &data[record.offset + HEADER..record.offset + HEADER + record.name_len] == name.as_bytes()
                {
                    return Ok(record.inode);
                }
            }
        }
        Err(Error::NotFound)
    }

    /// Adds an entry to a directory, in the first gap big enough, or in a
    /// new block at the end.
    fn add_entry(&mut self, directory: u32, name: &str, target: u32, kind: FileType) -> Result<()> {
        let mut inode = self.directory_inode(directory)?;
        self.drop_index(directory, &mut inode)?;
        let needed = record_size(name.len());
        let code = if self.has_file_types() { kind.directory_code() } else { 0 };
        for block in self.directory_blocks(&inode)? {
            if block == 0 {
                continue;
            }
            let data = self.block(block)?;
            let found = records(data)?.into_iter().find_map(|record| {
                let used = if record.inode == 0 { 0 } else { record_size(record.name_len) };
                (record.length - used >= needed).then_some((record, used))
            });
            if let Some((record, used)) = found {
                let data = self.block_mut(block)?;
                if used == 0 {
                    write_record(data, record.offset, target, record.length, name.as_bytes(), code);
                } else {
                    data[record.offset + 4..record.offset + 6].copy_from_slice(&(used as u16).to_le_bytes());
                    let offset = record.offset + used;
                    write_record(data, offset, target, record.length - used, name.as_bytes(), code);
                }
                return Ok(());
            }
        }
        // No room: a new block, holding just this entry.
        let index = inode.size / self.block_size as u64;
        let goal = self.goal_for(directory);
        let (block, _) = self.map_or_allocate(&mut inode, index, goal)?;
        let block_size = self.block_size;
        let data = self.fresh_block(block);
        write_record(data, 0, target, block_size, name.as_bytes(), code);
        inode.size += block_size as u64;
        self.write_inode(directory, &inode)
    }

    /// Removes entry `name` from a directory, returning its inode number.
    fn remove_entry(&mut self, directory: u32, name: &str) -> Result<u32> {
        let mut inode = self.directory_inode(directory)?;
        self.drop_index(directory, &mut inode)?;
        for block in self.directory_blocks(&inode)? {
            if block == 0 {
                continue;
            }
            let data = self.block(block)?;
            let list = records(data)?;
            let position = list.iter().position(|record| {
                record.inode != 0
                    && &data[record.offset + HEADER..record.offset + HEADER + record.name_len] == name.as_bytes()
            });
            let Some(position) = position else { continue };
            let record = &list[position];
            let target = record.inode;
            let data = self.block_mut(block)?;
            if position == 0 {
                // The first record in a block can't merge backwards: it just
                // becomes empty.
                data[record.offset..record.offset + 4].fill(0);
            } else {
                let previous = &list[position - 1];
                let merged = (previous.length + record.length) as u16;
                data[previous.offset + 4..previous.offset + 6].copy_from_slice(&merged.to_le_bytes());
            }
            return Ok(target);
        }
        Err(Error::NotFound)
    }

    /// Points an existing entry at another inode (for `..` when a directory
    /// moves).
    fn retarget_entry(&mut self, directory: u32, name: &str, target: u32) -> Result<()> {
        let inode = self.directory_inode(directory)?;
        for block in self.directory_blocks(&inode)? {
            if block == 0 {
                continue;
            }
            let data = self.block(block)?;
            let found = records(data)?.into_iter().find(|record| {
                record.inode != 0
                    && &data[record.offset + HEADER..record.offset + HEADER + record.name_len] == name.as_bytes()
            });
            if let Some(record) = found {
                self.block_mut(block)?[record.offset..record.offset + 4].copy_from_slice(&target.to_le_bytes());
                return Ok(());
            }
        }
        Err(Error::NotFound)
    }

    /// Changes an inode's link count by `delta`, and its change time.
    fn adjust_links(&mut self, number: u32, delta: i32) -> Result<Inode> {
        let mut inode = self.inode(number)?;
        let links = i32::from(inode.links) + delta;
        if links > i32::from(MAX_LINKS) {
            return Err(Error::TooManyLinks);
        }
        inode.links = links.max(0) as u16;
        inode.changed = self.now();
        self.write_inode(number, &inode)?;
        Ok(inode)
    }

    /// Marks a directory as changed now.
    fn touch_directory(&mut self, number: u32) -> Result<()> {
        let mut inode = self.inode(number)?;
        let now = self.now();
        inode.modified = now;
        inode.changed = now;
        self.write_inode(number, &inode)
    }

    /// Creates an empty file, directory, or other node in a directory.
    /// Returns the new inode's number.
    pub fn create(
        &mut self,
        directory: u32,
        name: &str,
        kind: FileType,
        permissions: u16,
        owner: Owner,
    ) -> Result<u32> {
        valid_name(name)?;
        self.change(|fs| {
            let parent = fs.directory_inode(directory)?;
            match fs.lookup(directory, name) {
                Ok(_) => return Err(Error::Exists),
                Err(Error::NotFound) => {}
                Err(error) => return Err(error),
            }
            if kind == FileType::Directory && parent.links >= MAX_LINKS {
                return Err(Error::TooManyLinks);
            }
            let (number, mut inode) = fs.new_inode(kind.mode_bits() | (permissions & 0o7777), owner, directory)?;
            if kind == FileType::Directory {
                let block_size = fs.block_size;
                let (block, _) = fs.map_or_allocate(&mut inode, 0, fs.goal_for(number))?;
                let data = fs.fresh_block(block);
                write_record(data, 0, number, 12, b".", FileType::Directory.directory_code());
                write_record(data, 12, directory, block_size - 12, b"..", FileType::Directory.directory_code());
                inode.size = block_size as u64;
                fs.write_inode(number, &inode)?;
                fs.adjust_links(directory, 1)?;
            }
            fs.add_entry(directory, name, number, kind)?;
            fs.touch_directory(directory)?;
            Ok(number)
        })
    }

    /// Creates a symbolic link to `target`. Short targets are kept in the
    /// inode itself (a "fast" link); longer ones in a data block.
    pub fn symlink(&mut self, directory: u32, name: &str, target: &str, owner: Owner) -> Result<u32> {
        if target.is_empty() || target.len() >= self.block_size {
            return Err(Error::NameTooLong);
        }
        let number = self.create(directory, name, FileType::Symlink, 0o777, owner)?;
        self.set_link_target(number, target)?;
        Ok(number)
    }

    /// Sets the target of a symbolic link made by [`create`](Self::create)
    /// (which starts out empty).
    pub fn set_link_target(&mut self, number: u32, target: &str) -> Result<()> {
        if target.is_empty() || target.len() >= self.block_size {
            return Err(Error::NameTooLong);
        }
        self.change(|fs| {
            let mut inode = fs.inode(number)?;
            if inode.kind() != FileType::Symlink || inode.size != 0 {
                return Err(Error::Unsupported);
            }
            if target.len() < 60 {
                let mut bytes = [0u8; 60];
                bytes[..target.len()].copy_from_slice(target.as_bytes());
                for (slot, chunk) in inode.block.iter_mut().zip(bytes.chunks(4)) {
                    *slot = u32::from_le_bytes(chunk.try_into().unwrap());
                }
                inode.size = target.len() as u64;
            } else {
                fs.write_data(number, &mut inode, 0, target.as_bytes())?;
            }
            fs.write_inode(number, &inode)
        })
    }

    /// Where a symbolic link points.
    pub fn read_link(&mut self, number: u32) -> Result<String> {
        let inode = self.inode(number)?;
        if inode.kind() != FileType::Symlink {
            return Err(Error::Unsupported);
        }
        let len = inode.size as usize;
        if len > self.block_size {
            return Err(Error::Corrupt);
        }
        let bytes = if inode.is_fast_symlink() {
            inode.block.iter().flat_map(|word| word.to_le_bytes()).take(len).collect()
        } else {
            let mut buffer = vec![0; len];
            self.read_data(&inode, 0, &mut buffer)?;
            buffer
        };
        String::from_utf8(bytes).map_err(|_| Error::Corrupt)
    }

    /// Adds another name for an existing file (a hard link).
    pub fn link(&mut self, directory: u32, name: &str, target: u32) -> Result<()> {
        valid_name(name)?;
        self.change(|fs| {
            if fs.inode(target)?.kind() == FileType::Directory {
                return Err(Error::IsDirectory);
            }
            if fs.lookup(directory, name).is_ok() {
                return Err(Error::Exists);
            }
            let inode = fs.adjust_links(target, 1)?;
            fs.add_entry(directory, name, target, inode.kind())?;
            fs.touch_directory(directory)
        })
    }

    /// Removes a name of a file that isn't a directory.
    pub fn unlink(&mut self, directory: u32, name: &str) -> Result<Unlinked> {
        self.change(|fs| {
            let target = fs.lookup(directory, name)?;
            if fs.inode(target)?.kind() == FileType::Directory {
                return Err(Error::IsDirectory);
            }
            fs.remove_entry(directory, name)?;
            fs.touch_directory(directory)?;
            let inode = fs.adjust_links(target, -1)?;
            Ok(Unlinked { inode: target, orphaned: inode.links == 0 })
        })
    }

    /// Removes an empty directory.
    pub fn remove_directory(&mut self, directory: u32, name: &str) -> Result<Unlinked> {
        self.change(|fs| {
            let target = fs.lookup(directory, name)?;
            fs.directory_inode(target)?;
            if !fs.list(target)?.is_empty() {
                return Err(Error::NotEmpty);
            }
            fs.remove_entry(directory, name)?;
            fs.touch_directory(directory)?;
            // Its `..` no longer counts toward the parent's links, and
            // nothing counts toward its own.
            fs.adjust_links(directory, -1)?;
            let mut inode = fs.inode(target)?;
            inode.links = 0;
            inode.changed = fs.now();
            fs.write_inode(target, &inode)?;
            Ok(Unlinked { inode: target, orphaned: true })
        })
    }

    /// Moves entry `from_name` of `from` to `to_name` in `to`, replacing a
    /// file (or empty directory) already there. Returns what was replaced.
    pub fn rename(&mut self, from: u32, from_name: &str, to: u32, to_name: &str) -> Result<Option<Unlinked>> {
        valid_name(to_name)?;
        self.change(|fs| {
            let source = fs.lookup(from, from_name)?;
            let kind = fs.inode(source)?.kind();
            let directory = kind == FileType::Directory;
            let existing = match fs.lookup(to, to_name) {
                Ok(existing) if existing == source => return Ok(None),
                Ok(existing) => Some(existing),
                Err(Error::NotFound) => None,
                Err(error) => return Err(error),
            };
            if let Some(existing) = existing {
                let existing_directory = fs.inode(existing)?.kind() == FileType::Directory;
                match (directory, existing_directory) {
                    (false, true) => return Err(Error::IsDirectory),
                    (true, false) => return Err(Error::NotDirectory),
                    (true, true) if !fs.list(existing)?.is_empty() => return Err(Error::NotEmpty),
                    _ => {}
                }
            }
            if directory && from != to {
                // A directory can't move into its own subtree.
                let mut ancestor = to;
                while ancestor != crate::ROOT {
                    if ancestor == source {
                        return Err(Error::Loop);
                    }
                    let parent = fs.lookup(ancestor, "..")?;
                    if parent == ancestor {
                        break;
                    }
                    ancestor = parent;
                }
                if source == to {
                    return Err(Error::Loop);
                }
            }

            let mut replaced = None;
            if let Some(existing) = existing {
                fs.remove_entry(to, to_name)?;
                let inode = if directory {
                    fs.adjust_links(to, -1)?;
                    let mut inode = fs.inode(existing)?;
                    inode.links = 0;
                    fs.write_inode(existing, &inode)?;
                    inode
                } else {
                    fs.adjust_links(existing, -1)?
                };
                replaced = Some(Unlinked { inode: existing, orphaned: inode.links == 0 });
            }
            fs.add_entry(to, to_name, source, kind)?;
            fs.remove_entry(from, from_name)?;
            if directory && from != to {
                fs.retarget_entry(source, "..", to)?;
                fs.adjust_links(from, -1)?;
                fs.adjust_links(to, 1)?;
            }
            fs.adjust_links(source, 0)?;
            fs.touch_directory(from)?;
            if from != to {
                fs.touch_directory(to)?;
            }
            Ok(replaced)
        })
    }
}
