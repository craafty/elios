//! A write-back cache in front of a partition, for ext2 volumes.
//!
//! The ext2 code writes its metadata back at the end of every operation
//! (the bitmaps, the inode, the group descriptors, the superblock), and file
//! data as it comes. Straight to the disk, a program writing a file 4 KiB at
//! a time makes five or six small writes for each, most of them rewriting
//! the same few blocks. A USB flash stick is at its slowest doing exactly
//! that: every small write makes it rewrite a whole erase block, tens of
//! milliseconds, so a download that should take a minute takes twenty.
//!
//! So writes are kept here, in 4 KiB pages, and reach the disk later, as
//! Linux's page cache does it: sorted, with neighbors merged into runs of up
//! to 1 MiB, and every block written once however often it changed. That
//! happens when a program asks (`sync`, `fsync`), every few seconds
//! ([`start_flushing`]), when too much is waiting, and when the volume is
//! unmounted or the machine shuts down. A crash or a pulled stick loses
//! at most those few seconds, as on Linux. Reads see what's waiting.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::block::Partition;
use crate::task;

const SECTOR: usize = 512;
const SECTORS_PER_PAGE: u64 = 8;
const PAGE: usize = SECTOR * SECTORS_PER_PAGE as usize;
/// The most waiting before it's all written (32 MiB).
const MOST_PAGES: usize = 8192;
/// The longest single write.
const MOST_RUN_SECTORS: u64 = 2048;
/// How often waiting writes go to the disk anyway.
const FLUSH_EVERY_MS: u64 = 5000;

struct Page {
    /// Which of its sectors are waiting (bit per sector).
    valid: u8,
    data: Box<[u8; PAGE]>,
}

pub struct WriteBack {
    partition: Partition,
    pages: BTreeMap<u64, Page>,
}

impl WriteBack {
    pub fn new(partition: Partition) -> Self {
        Self { partition, pages: BTreeMap::new() }
    }

    /// Writes everything waiting, in sorted, merged runs.
    pub fn flush(&mut self) -> Result<(), &'static str> {
        let mut run: Vec<u8> = Vec::new();
        let mut run_start = 0;
        for (&number, page) in &self.pages {
            for index in 0..SECTORS_PER_PAGE {
                if page.valid & (1 << index) == 0 {
                    continue;
                }
                let sector = number * SECTORS_PER_PAGE + index;
                let run_sectors = (run.len() / SECTOR) as u64;
                if !run.is_empty() && (sector != run_start + run_sectors || run_sectors >= MOST_RUN_SECTORS) {
                    self.partition.write(run_start, &run)?;
                    run.clear();
                }
                if run.is_empty() {
                    run_start = sector;
                }
                let offset = index as usize * SECTOR;
                run.extend_from_slice(&page.data[offset..offset + SECTOR]);
            }
        }
        if !run.is_empty() {
            self.partition.write(run_start, &run)?;
        }
        // Only now: after a failure everything is still here to try again.
        self.pages.clear();
        Ok(())
    }

    fn waiting(&self) -> usize {
        self.pages.len()
    }
}

impl ext2::BlockDevice for WriteBack {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> ext2::Result<()> {
        let count = (buffer.len() / SECTOR) as u64;
        let pages = sector / SECTORS_PER_PAGE..(sector + count).div_ceil(SECTORS_PER_PAGE);
        if self.pages.range(pages.clone()).next().is_none() {
            return self.partition.read(sector, buffer).map_err(|_| ext2::Error::Io);
        }
        // Read from the disk only if some sector isn't waiting here.
        let covered = (sector..sector + count).all(|s| {
            self.pages.get(&(s / SECTORS_PER_PAGE)).is_some_and(|page| page.valid & (1 << (s % SECTORS_PER_PAGE)) != 0)
        });
        if !covered {
            self.partition.read(sector, buffer).map_err(|_| ext2::Error::Io)?;
        }
        for (&number, page) in self.pages.range(pages) {
            for index in 0..SECTORS_PER_PAGE {
                let s = number * SECTORS_PER_PAGE + index;
                if page.valid & (1 << index) != 0 && s >= sector && s < sector + count {
                    let to = (s - sector) as usize * SECTOR;
                    let from = index as usize * SECTOR;
                    buffer[to..to + SECTOR].copy_from_slice(&page.data[from..from + SECTOR]);
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> ext2::Result<()> {
        if sector + (data.len() / SECTOR) as u64 > self.partition.sectors {
            return Err(ext2::Error::Io);
        }
        for (offset, chunk) in data.as_chunks::<SECTOR>().0.iter().enumerate() {
            let s = sector + offset as u64;
            let page =
                self.pages.entry(s / SECTORS_PER_PAGE).or_insert_with(|| Page { valid: 0, data: Box::new([0; PAGE]) });
            let index = s % SECTORS_PER_PAGE;
            page.valid |= 1 << index;
            let at = index as usize * SECTOR;
            page.data[at..at + SECTOR].copy_from_slice(chunk);
        }
        if self.waiting() >= MOST_PAGES {
            self.flush().map_err(|_| ext2::Error::Io)?;
        }
        Ok(())
    }

    fn sectors(&self) -> u64 {
        self.partition.sectors
    }

    fn sync(&mut self) -> ext2::Result<()> {
        self.flush().map_err(|problem| {
            warn!("fs: writing back to {}: {problem}", self.partition.disk.name());
            ext2::Error::Io
        })
    }
}

/// Starts the thread that writes waiting changes back every few seconds
/// (once, whichever volume asks first).
pub fn start_flushing() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    task::spawn("writeback", || {
        loop {
            task::sleep_ms(FLUSH_EVERY_MS);
            super::mount::sync_all();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::ext2fs::tests::MemoryDisk;
    use super::*;
    use crate::drivers::block::Disk;
    use crate::sync::SpinLock;
    use alloc::sync::Arc;
    use alloc::vec;
    use ext2::BlockDevice;

    crate::ktest! {
        fn write_back_cache_reads_its_own_writes_and_merges_them() {
            let disk: Arc<dyn Disk> = Arc::new(MemoryDisk { data: SpinLock::new(vec![0; 256 * SECTOR]) });
            let mut cache = WriteBack::new(Partition::whole(&disk));
            // Odd sectors: a page partly waiting, read across pages.
            cache.write(3, &[7; 3 * SECTOR]).unwrap();
            cache.write(9, &[9; SECTOR]).unwrap();
            let mut on_disk = vec![0xEE; 16 * SECTOR];
            disk.read(0, &mut on_disk).unwrap();
            assert!(on_disk.iter().all(|&b| b == 0), "written through");
            let mut seen = vec![0; 16 * SECTOR];
            cache.read(0, &mut seen).unwrap();
            for (sector, bytes) in seen.chunks(SECTOR).enumerate() {
                let want = match sector {
                    3..=5 => 7,
                    9 => 9,
                    _ => 0,
                };
                assert!(bytes.iter().all(|&b| b == want), "sector {sector}");
            }
            // Rewritten before it's flushed: the last version wins.
            cache.write(4, &[8; SECTOR]).unwrap();
            cache.sync().unwrap();
            assert_eq!(cache.waiting(), 0);
            disk.read(0, &mut on_disk).unwrap();
            assert_eq!(on_disk, {
                let mut want = vec![0; 16 * SECTOR];
                want[3 * SECTOR..6 * SECTOR].fill(7);
                want[4 * SECTOR..5 * SECTOR].fill(8);
                want[9 * SECTOR..10 * SECTOR].fill(9);
                want
            });
            assert!(cache.write(255, &[1; 2 * SECTOR]).is_err(), "past the end");
        }
    }
}
