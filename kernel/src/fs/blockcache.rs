//! The block cache: what an ext2 volume read from its disk, and what it
//! wrote but hasn't sent there yet, kept in memory in 4 KiB pages.
//!
//! Reads: the ext2 code reads directories, inodes, block maps and file
//! data from its partition on every operation (it keeps only a small cache
//! of metadata blocks itself). On an internal disk that's fast enough; on
//! a USB flash stick each request takes a millisecond or more, and a
//! program's start (Firefox's: thousands of files and directories looked
//! up, fonts scanned, libraries read) made the same requests again on every
//! start, for minutes. So everything read stays here, as in Linux's page
//! cache, until memory is wanted: the least recently used clean pages go
//! first. The cache takes at most a quarter of the RAM, doesn't grow while
//! less than an eighth is free, and gives pages back (every second, from
//! the flushing thread) when free memory falls under that.
//!
//! Writes: the ext2 code writes its metadata back at the end of every
//! operation (the bitmaps, the inode, the group descriptors, the
//! superblock), and file data as it comes. Straight to the disk, a program
//! writing a file 4 KiB at a time made five or six small writes for each,
//! most of them rewriting the same few blocks, and a flash stick rewrites
//! a whole erase block for every one. So writes wait here and reach the
//! disk later, sorted, with neighbors merged into runs of up to 1 MiB, and
//! every block written once however often it changed: when a program asks
//! (`sync`, `fsync`), every few seconds ([`start`]), when too much is
//! waiting, and when the volume is unmounted or the machine shuts down. A
//! crash or a pulled stick loses at most those few seconds, as on Linux.
//! On a USB stick `fsync` only asks for a write within a second
//! ([`write_soon`]): SQLite (Firefox's profile) calls it several times a
//! second, each time with the superblock, group descriptors, bitmaps,
//! inodes and data to write, five to ten separate requests that a flash
//! stick takes tens of milliseconds each for.
//!
//! Pages are tracked by sector (a bit each), as the ext2 code may write a
//! single sector (the superblock) and partitions needn't end on a page.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::block::Partition;
use crate::memory::{frames, phys_to_virt};
use crate::sync::SpinLock;
use crate::task;

const SECTOR: usize = 512;
const SECTORS_PER_PAGE: u64 = 8;
const PAGE: usize = SECTOR * SECTORS_PER_PAGE as usize;
/// The most waiting to be written before it's all written (32 MiB).
const MOST_DIRTY_PAGES: usize = 8192;
/// The longest single write.
const MOST_RUN_SECTORS: u64 = 2048;
/// How often waiting writes go to the disk anyway, and how often memory
/// is checked.
const FLUSH_EVERY_MS: u64 = 5000;
const TRIM_EVERY_MS: u64 = 1000;
/// Clean pages dropped at a time when the cache is full.
const EVICT_BATCH: usize = 256;

struct Page {
    /// Which sectors hold the disk's (or newer) contents, a bit each.
    valid: u8,
    /// Which sectors are waiting to be written.
    dirty: u8,
    /// When it was last used (the store's clock).
    used: u64,
    /// The frame holding its bytes, the cache's own.
    frame: u64,
}

impl Page {
    fn bytes(&self) -> &[u8] {
        // SAFETY: the frame belongs to this page alone, and all RAM is in
        // the direct map.
        unsafe { core::slice::from_raw_parts(phys_to_virt(self.frame), PAGE) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`, and `&mut self` makes this the only access.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(self.frame), PAGE) }
    }
}

/// The pages, behind a spinlock (never held across disk I/O) so the
/// trimming thread can take clean pages back while the volume is busy.
struct Store {
    pages: BTreeMap<u64, Page>,
    /// Pages by when they were last used, oldest first.
    by_use: BTreeMap<u64, u64>,
    clock: u64,
    /// The pages with sectors waiting to be written.
    dirty: BTreeSet<u64>,
}

impl Store {
    fn touch(&mut self, number: u64) {
        self.clock += 1;
        let clock = self.clock;
        if let Some(page) = self.pages.get_mut(&number) {
            self.by_use.remove(&page.used);
            page.used = clock;
            self.by_use.insert(clock, number);
        }
    }

    /// Page `number`, made (with nothing valid) if it isn't here; `None`
    /// if there's no memory for it.
    fn page_mut(&mut self, number: u64) -> Option<&mut Page> {
        if self.pages.contains_key(&number) {
            self.touch(number);
        } else {
            if self.pages.len() >= most_pages() || memory_short() {
                self.evict(EVICT_BATCH);
            }
            let frame = frames::allocate()?;
            self.clock += 1;
            self.pages.insert(number, Page { valid: 0, dirty: 0, used: self.clock, frame });
            self.by_use.insert(self.clock, number);
        }
        self.pages.get_mut(&number)
    }

    /// Drops up to `count` of the least recently used clean pages; returns
    /// how many.
    fn evict(&mut self, count: usize) -> usize {
        let victims: Vec<(u64, u64)> = self
            .by_use
            .iter()
            .filter(|(_, number)| self.pages[number].dirty == 0)
            .take(count)
            .map(|(&used, &number)| (used, number))
            .collect();
        for (used, number) in &victims {
            self.by_use.remove(used);
            if let Some(page) = self.pages.remove(number) {
                frames::free(page.frame);
            }
        }
        victims.len()
    }

    /// The first run of waiting sectors at or after `from`: its start and
    /// bytes.
    fn next_run(&self, from: u64) -> Option<(u64, Vec<u8>)> {
        let mut run: Vec<u8> = Vec::new();
        let mut start = 0;
        for number in self.dirty.range(from / SECTORS_PER_PAGE..) {
            let page = &self.pages[number];
            for index in 0..SECTORS_PER_PAGE {
                let sector = number * SECTORS_PER_PAGE + index;
                if sector < from || page.dirty & (1 << index) == 0 {
                    continue;
                }
                let run_sectors = (run.len() / SECTOR) as u64;
                if !run.is_empty() && (sector != start + run_sectors || run_sectors >= MOST_RUN_SECTORS) {
                    return Some((start, run));
                }
                if run.is_empty() {
                    start = sector;
                }
                let at = index as usize * SECTOR;
                run.extend_from_slice(&page.bytes()[at..at + SECTOR]);
            }
        }
        (!run.is_empty()).then_some((start, run))
    }

    /// Marks `count` sectors from `start` written.
    fn written(&mut self, start: u64, count: u64) {
        for sector in start..start + count {
            if let Some(page) = self.pages.get_mut(&(sector / SECTORS_PER_PAGE)) {
                page.dirty &= !(1 << (sector % SECTORS_PER_PAGE));
                if page.dirty == 0 {
                    self.dirty.remove(&(sector / SECTORS_PER_PAGE));
                }
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        for page in self.pages.values() {
            frames::free(page.frame);
        }
    }
}

/// Every cache, for trimming.
static CACHES: SpinLock<Vec<Weak<SpinLock<Store>>>> = SpinLock::new(Vec::new());

fn most_pages() -> usize {
    (frames::stats().managed_bytes / 4 / PAGE as u64) as usize
}

/// Whether free memory is under an eighth of it.
fn memory_short() -> bool {
    let stats = frames::stats();
    stats.free_bytes < stats.managed_bytes / 8
}

pub struct BlockCache {
    partition: Partition,
    store: Arc<SpinLock<Store>>,
}

impl BlockCache {
    pub fn new(partition: Partition) -> Self {
        let store = Arc::new(SpinLock::new(Store {
            pages: BTreeMap::new(),
            by_use: BTreeMap::new(),
            clock: 0,
            dirty: BTreeSet::new(),
        }));
        let mut caches = CACHES.lock();
        caches.retain(|cache| cache.strong_count() > 0);
        caches.push(Arc::downgrade(&store));
        Self { partition, store }
    }

    /// Writes everything waiting, in sorted, merged runs. Pages stay, clean.
    pub fn flush(&mut self) -> Result<(), &'static str> {
        let mut from = 0;
        loop {
            let Some((start, run)) = self.store.lock().next_run(from) else {
                return Ok(());
            };
            // Not holding the lock: other volumes' trimming may go on.
            // After a failure the rest is still waiting, to try again.
            self.partition.write(start, &run)?;
            let count = (run.len() / SECTOR) as u64;
            self.store.lock().written(start, count);
            from = start + count;
        }
    }

    fn dirty_pages(&self) -> usize {
        self.store.lock().dirty.len()
    }

    /// Copies sectors `sector..` into `buffer` if every one is here.
    fn read_cached(&self, sector: u64, buffer: &mut [u8]) -> bool {
        let mut store = self.store.lock();
        let count = (buffer.len() / SECTOR) as u64;
        let all_here = (sector..sector + count).all(|s| {
            store.pages.get(&(s / SECTORS_PER_PAGE)).is_some_and(|page| page.valid & (1 << (s % SECTORS_PER_PAGE)) != 0)
        });
        if !all_here {
            return false;
        }
        for s in sector..sector + count {
            let number = s / SECTORS_PER_PAGE;
            let at = (s % SECTORS_PER_PAGE) as usize * SECTOR;
            let to = (s - sector) as usize * SECTOR;
            buffer[to..to + SECTOR].copy_from_slice(&store.pages[&number].bytes()[at..at + SECTOR]);
        }
        for number in sector / SECTORS_PER_PAGE..(sector + count).div_ceil(SECTORS_PER_PAGE) {
            store.touch(number);
        }
        true
    }
}

impl ext2::BlockDevice for BlockCache {
    fn read(&mut self, sector: u64, buffer: &mut [u8]) -> ext2::Result<()> {
        if self.read_cached(sector, buffer) {
            return Ok(());
        }
        // Whole pages from the disk (up to the partition's end), keeping
        // the sectors already here: those may be newer than the disk's.
        let count = (buffer.len() / SECTOR) as u64;
        let first = sector - sector % SECTORS_PER_PAGE;
        let end =
            (sector + count).div_ceil(SECTORS_PER_PAGE).saturating_mul(SECTORS_PER_PAGE).min(self.partition.sectors);
        if end < sector + count {
            return Err(ext2::Error::Io);
        }
        let mut data = vec![0u8; (end - first) as usize * SECTOR];
        self.partition.read(first, &mut data).map_err(|_| ext2::Error::Io)?;
        let mut store = self.store.lock();
        for (offset, chunk) in data.as_chunks::<SECTOR>().0.iter().enumerate() {
            let s = first + offset as u64;
            let bit = 1 << (s % SECTORS_PER_PAGE);
            let at = (s % SECTORS_PER_PAGE) as usize * SECTOR;
            if let Some(page) = store.page_mut(s / SECTORS_PER_PAGE) {
                if page.valid & bit == 0 {
                    page.bytes_mut()[at..at + SECTOR].copy_from_slice(chunk);
                    page.valid |= bit;
                }
                if s >= sector && s < sector + count {
                    let to = (s - sector) as usize * SECTOR;
                    buffer[to..to + SECTOR].copy_from_slice(&page.bytes()[at..at + SECTOR]);
                }
                continue;
            }
            // No memory to keep it: straight from the disk's copy (no
            // page, so nothing newer is waiting).
            if s >= sector && s < sector + count {
                let to = (s - sector) as usize * SECTOR;
                buffer[to..to + SECTOR].copy_from_slice(chunk);
            }
        }
        Ok(())
    }

    fn write(&mut self, sector: u64, data: &[u8]) -> ext2::Result<()> {
        if sector + (data.len() / SECTOR) as u64 > self.partition.sectors {
            return Err(ext2::Error::Io);
        }
        let mut kept = true;
        {
            let mut store = self.store.lock();
            for (offset, chunk) in data.as_chunks::<SECTOR>().0.iter().enumerate() {
                let s = sector + offset as u64;
                let bit = 1 << (s % SECTORS_PER_PAGE);
                let at = (s % SECTORS_PER_PAGE) as usize * SECTOR;
                let Some(page) = store.page_mut(s / SECTORS_PER_PAGE) else {
                    kept = false;
                    break;
                };
                page.bytes_mut()[at..at + SECTOR].copy_from_slice(chunk);
                page.valid |= bit;
                page.dirty |= bit;
                store.dirty.insert(s / SECTORS_PER_PAGE);
            }
        }
        if !kept {
            // No memory: write what's waiting, then this, straight through
            // (what was kept of it is rewritten with the same bytes).
            self.flush().map_err(|_| ext2::Error::Io)?;
            return self.partition.write(sector, data).map_err(|_| ext2::Error::Io);
        }
        if self.dirty_pages() >= MOST_DIRTY_PAGES {
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

/// Gives clean pages back while free memory is short; returns how many.
fn trim() -> usize {
    let caches: Vec<Arc<SpinLock<Store>>> = CACHES.lock().iter().filter_map(Weak::upgrade).collect();
    let mut freed = 0;
    for cache in caches {
        while memory_short() {
            let evicted = cache.lock().evict(EVICT_BATCH);
            freed += evicted;
            if evicted == 0 {
                break;
            }
        }
    }
    freed
}

/// Set by [`write_soon`]: the thread writes back at its next turn.
static WRITE_SOON: AtomicBool = AtomicBool::new(false);

/// Has waiting changes written within a second (for `fsync` on USB
/// sticks: several calls in that time cost one write-back).
pub fn write_soon() {
    WRITE_SOON.store(true, Ordering::Release);
}

/// Starts the thread that writes waiting changes back every few seconds,
/// and gives memory back when it runs short (once, whichever volume asks
/// first).
pub fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    task::spawn("blockcache", || {
        let mut since_flush = 0;
        loop {
            task::sleep_ms(TRIM_EVERY_MS);
            let freed = trim();
            if freed > 0 {
                debug!("fs: memory short: {} KiB of the block cache given back", freed * PAGE / 1024);
            }
            since_flush += TRIM_EVERY_MS;
            if since_flush >= FLUSH_EVERY_MS || WRITE_SOON.swap(false, Ordering::AcqRel) {
                since_flush = 0;
                super::mount::sync_all();
            }
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
    use ext2::BlockDevice;

    crate::ktest! {
        fn block_cache_reads_its_own_writes_and_merges_them() {
            let disk: Arc<dyn Disk> = Arc::new(MemoryDisk { data: SpinLock::new(vec![0; 256 * SECTOR]) });
            let mut cache = BlockCache::new(Partition::whole(&disk));
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
            assert_eq!(cache.dirty_pages(), 0);
            disk.read(0, &mut on_disk).unwrap();
            let mut want = vec![0; 16 * SECTOR];
            want[3 * SECTOR..6 * SECTOR].fill(7);
            want[4 * SECTOR..5 * SECTOR].fill(8);
            want[9 * SECTOR..10 * SECTOR].fill(9);
            assert_eq!(on_disk, want);
            assert!(cache.write(255, &[1; 2 * SECTOR]).is_err(), "past the end");
        }

        fn block_cache_keeps_what_it_read() {
            let disk: Arc<dyn Disk> = Arc::new(MemoryDisk { data: SpinLock::new(vec![5; 64 * SECTOR]) });
            let mut cache = BlockCache::new(Partition::whole(&disk));
            let mut seen = vec![0; 3 * SECTOR];
            cache.read(10, &mut seen).unwrap();
            assert!(seen.iter().all(|&b| b == 5));
            // Changed behind the cache's back: the cache's copy is read
            // (proving it didn't go to the disk), for the whole page.
            disk.write(8, &[6; 8 * SECTOR]).unwrap();
            let mut again = vec![0; 8 * SECTOR];
            cache.read(8, &mut again).unwrap();
            assert!(again.iter().all(|&b| b == 5));
            // A sector waiting to be written survives a read of its page
            // that has to go to the disk.
            cache.write(17, &[9; SECTOR]).unwrap();
            let mut page = vec![0; 8 * SECTOR];
            cache.read(16, &mut page).unwrap();
            assert!(page[SECTOR..2 * SECTOR].iter().all(|&b| b == 9));
            assert!(page[..SECTOR].iter().all(|&b| b == 5) && page[2 * SECTOR..].iter().all(|&b| b == 5));
        }
    }
}
