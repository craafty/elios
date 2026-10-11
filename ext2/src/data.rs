//! File data: finding a file's blocks through its block map, reading,
//! writing (allocating blocks as needed), and truncating.

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::Inode;
use crate::{BlockDevice, Error, FileSystem, FileType, Result};

/// Direct block pointers in an inode.
const DIRECT: u64 = 12;
/// The most blocks read or written in one device transfer.
const MAX_RUN: usize = 64;

impl<D: BlockDevice> FileSystem<D> {
    fn pointers_per_block(&self) -> u64 {
        self.block_size as u64 / 4
    }

    /// How a file block is reached: the inode slot (12, 13 or 14 for the
    /// indirect trees) and the index within that tree, with its depth. Direct
    /// blocks have depth 0.
    fn route(&self, index: u64) -> Result<(usize, u64, u32)> {
        let per = self.pointers_per_block();
        if index < DIRECT {
            return Ok((index as usize, 0, 0));
        }
        let mut rest = index - DIRECT;
        let mut span = per;
        for depth in 1..=3u32 {
            if rest < span {
                return Ok((11 + depth as usize, rest, depth));
            }
            rest -= span;
            span *= per;
        }
        Err(Error::TooBig)
    }

    /// The device block holding block `index` of a file, or 0 for a hole.
    pub(crate) fn map(&mut self, inode: &Inode, index: u64) -> Result<u32> {
        let (slot, mut within, depth) = self.route(index)?;
        let mut block = inode.block[slot];
        let per = self.pointers_per_block();
        for level in (0..depth).rev() {
            if block == 0 {
                return Ok(0);
            }
            let span = per.pow(level);
            let entry = (within / span) as usize;
            within %= span;
            let data = self.block(block)?;
            block = u32::from_le_bytes(data[entry * 4..entry * 4 + 4].try_into().unwrap());
        }
        Ok(block)
    }

    /// Like [`map`](Self::map), but fills a hole with a new block (and any
    /// indirect blocks on the way, zeroed). New data blocks are *not*
    /// zeroed: the caller writes them. Returns the block and whether it's
    /// new.
    pub(crate) fn map_or_allocate(&mut self, inode: &mut Inode, index: u64, goal: u32) -> Result<(u32, bool)> {
        let (slot, mut within, depth) = self.route(index)?;
        let sectors = self.sectors_per_block() as u32;
        let per = self.pointers_per_block();
        if inode.block[slot] == 0 {
            let block = self.allocate_block(goal)?;
            if depth > 0 {
                self.fresh_block(block);
            }
            inode.block[slot] = block;
            inode.sectors += sectors;
            if depth == 0 {
                return Ok((block, true));
            }
        }
        let mut block = inode.block[slot];
        if depth == 0 {
            return Ok((block, false));
        }
        let mut new = false;
        for level in (0..depth).rev() {
            let span = per.pow(level);
            let entry = (within / span) as usize;
            within %= span;
            let data = self.block(block)?;
            let next = u32::from_le_bytes(data[entry * 4..entry * 4 + 4].try_into().unwrap());
            if next != 0 {
                block = next;
                new = false;
                continue;
            }
            let allocated = self.allocate_block(goal)?;
            if level > 0 {
                self.fresh_block(allocated);
            }
            self.block_mut(block)?[entry * 4..entry * 4 + 4].copy_from_slice(&allocated.to_le_bytes());
            inode.sectors += sectors;
            block = allocated;
            new = true;
        }
        Ok((block, new))
    }

    /// Reads from a file at `offset`. Returns the bytes read (fewer at the
    /// end of the file).
    pub fn read(&mut self, number: u32, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        let inode = self.inode(number)?;
        if inode.kind() == FileType::Directory {
            return Err(Error::IsDirectory);
        }
        self.read_data(&inode, offset, buffer)
    }

    pub(crate) fn read_data(&mut self, inode: &Inode, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        if offset >= inode.size {
            return Ok(0);
        }
        let len = buffer.len().min((inode.size - offset) as usize);
        let block_size = self.block_size as u64;
        let mut done = 0;
        while done < len {
            let position = offset + done as u64;
            let first_index = position / block_size;
            let within = (position % block_size) as usize;
            let first = self.map(inode, first_index)?;
            // Gather the blocks that follow on the device too.
            let wanted = (within + len - done).div_ceil(self.block_size).min(MAX_RUN);
            let mut run = 1;
            while run < wanted && first != 0 && self.map(inode, first_index + run as u64)? == first + run as u32 {
                run += 1;
            }
            let count = (run * self.block_size - within).min(len - done);
            if first == 0 {
                buffer[done..done + count].fill(0);
            } else {
                let mut data = vec![0; run * self.block_size];
                self.read_blocks(first, &mut data)?;
                buffer[done..done + count].copy_from_slice(&data[within..within + count]);
            }
            done += count;
        }
        Ok(len)
    }

    /// Writes to a file at `offset`, growing it as needed.
    pub fn write(&mut self, number: u32, offset: u64, data: &[u8]) -> Result<usize> {
        self.change(|fs| {
            let mut inode = fs.inode(number)?;
            if inode.kind() == FileType::Directory {
                return Err(Error::IsDirectory);
            }
            fs.write_data(number, &mut inode, offset, data)?;
            let now = fs.now();
            inode.modified = now;
            inode.changed = now;
            fs.write_inode(number, &inode)?;
            Ok(data.len())
        })
    }

    pub(crate) fn write_data(&mut self, number: u32, inode: &mut Inode, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len() as u64).ok_or(Error::TooBig)?;
        if data.is_empty() {
            return Ok(());
        }
        let block_size = self.block_size;
        let mut goal = match inode.size {
            0 => self.goal_for(number),
            size => self.map(inode, (size - 1) / block_size as u64)?.max(self.goal_for(number)) + 1,
        };
        // Whole blocks waiting to be written in one go: (first block, start
        // in `data`, count).
        let mut run: Option<(u32, usize, usize)> = None;
        let mut position = offset;
        while position < end {
            let index = position / block_size as u64;
            let within = (position % block_size as u64) as usize;
            let count = (block_size - within).min((end - position) as usize);
            let start = (position - offset) as usize;
            let (block, new) = self.map_or_allocate(inode, index, goal)?;
            goal = block + 1;
            if count == block_size {
                match &mut run {
                    Some((first, _, blocks)) if *first + *blocks as u32 == block && *blocks < MAX_RUN => *blocks += 1,
                    _ => {
                        if let Some((first, from, blocks)) = run.take() {
                            self.write_blocks(first, &data[from..from + blocks * block_size])?;
                        }
                        run = Some((block, start, 1));
                    }
                }
            } else {
                // Part of a block: keep what's around it.
                let mut buffer = vec![0; block_size];
                if !new {
                    self.read_blocks(block, &mut buffer)?;
                }
                buffer[within..within + count].copy_from_slice(&data[start..start + count]);
                self.write_blocks(block, &buffer)?;
            }
            position += count as u64;
        }
        if let Some((first, from, blocks)) = run {
            self.write_blocks(first, &data[from..from + blocks * block_size])?;
        }
        inode.size = inode.size.max(end);
        Ok(())
    }

    /// Changes a file's size: shrinking frees the blocks past the end;
    /// growing leaves a hole, which reads as zeros.
    pub fn truncate(&mut self, number: u32, len: u64) -> Result<()> {
        self.change(|fs| {
            let mut inode = fs.inode(number)?;
            if inode.kind() == FileType::Directory {
                return Err(Error::IsDirectory);
            }
            fs.route(len.saturating_sub(1) / fs.block_size as u64)?;
            if len < inode.size {
                fs.free_data(&mut inode, len)?;
                // The rest of the last block must read as zeros if the
                // file grows again.
                let within = (len % fs.block_size as u64) as usize;
                if within != 0 {
                    let block = fs.map(&inode, len / fs.block_size as u64)?;
                    if block != 0 {
                        let mut buffer = vec![0; fs.block_size];
                        fs.read_blocks(block, &mut buffer)?;
                        buffer[within..].fill(0);
                        fs.write_blocks(block, &buffer)?;
                    }
                }
            }
            inode.size = len;
            let now = fs.now();
            inode.modified = now;
            inode.changed = now;
            fs.write_inode(number, &inode)
        })
    }

    /// Frees every block holding data at or past byte `from`.
    pub(crate) fn free_data(&mut self, inode: &mut Inode, from: u64) -> Result<()> {
        let first = from.div_ceil(self.block_size as u64);
        let sectors = self.sectors_per_block() as u32;
        for slot in (first.min(DIRECT) as usize)..DIRECT as usize {
            if inode.block[slot] != 0 {
                self.free_block(inode.block[slot])?;
                inode.block[slot] = 0;
                inode.sectors = inode.sectors.saturating_sub(sectors);
            }
        }
        let per = self.pointers_per_block();
        let mut base = DIRECT;
        let mut span = per;
        for depth in 1..=3u32 {
            let slot = 11 + depth as usize;
            if inode.block[slot] != 0 && first < base + span {
                let start = first.saturating_sub(base);
                let freed = self.free_tree(inode.block[slot], depth, start, &mut inode.sectors)?;
                if freed {
                    inode.block[slot] = 0;
                }
            }
            base += span;
            span *= per;
        }
        Ok(())
    }

    /// Frees the blocks of an indirect tree from index `start` on. Returns
    /// whether the tree's own block was freed too (nothing was left in it).
    fn free_tree(&mut self, block: u32, depth: u32, start: u64, sectors: &mut u32) -> Result<bool> {
        let per = self.pointers_per_block();
        let span = per.pow(depth - 1);
        let entries: Vec<u32> =
            self.block(block)?.chunks(4).map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap())).collect();
        let per_block = self.sectors_per_block() as u32;
        let mut remaining = false;
        for (entry, &child) in entries.iter().enumerate() {
            let entry_start = entry as u64 * span;
            if child == 0 {
                continue;
            }
            if entry_start + span <= start {
                remaining = true;
                continue;
            }
            let freed = if depth == 1 {
                self.free_block(child)?;
                *sectors = sectors.saturating_sub(per_block);
                true
            } else {
                self.free_tree(child, depth - 1, start.saturating_sub(entry_start), sectors)?
            };
            if freed {
                self.block_mut(block)?[entry * 4..entry * 4 + 4].fill(0);
            } else {
                remaining = true;
            }
        }
        if remaining {
            return Ok(false);
        }
        self.free_block(block)?;
        *sectors = sectors.saturating_sub(per_block);
        Ok(true)
    }
}
