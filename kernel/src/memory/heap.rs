//! The kernel heap, which backs `Box`, `Vec`, `String` and friends.
//!
//! Small allocations (up to 2 KiB) come from size classes: each class hands
//! out blocks of one power-of-two size, carved from 4 KiB pages and kept on a
//! free list. Blocks are aligned to their size, which covers any alignment up
//! to that size.
//!
//! Larger allocations get whole pages in the heap's virtual region, each
//! backed by its own physical frame, so they needn't be physically
//! contiguous. Freeing one unmaps it and returns the frames, except that
//! a few freed runs of each length up to 64 KiB are kept mapped for the
//! next allocation of that length. System calls allocate and free buffers
//! of a few pages all the time, and unmapping means a TLB shootdown, which
//! interrupts every CPU.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use super::paging::{GLOBAL, NO_EXECUTE, PageSize, WRITABLE};
use super::{HEAP_END, HEAP_START, PAGE_SIZE, frames, with_kernel_table};
use crate::arch::smp;
use crate::sync::SpinLock;

const MIN_BLOCK: usize = 16;
const MAX_BLOCK: usize = 2048;
/// 16, 32, ... 2048.
const SIZE_CLASSES: usize = (MAX_BLOCK / MIN_BLOCK).trailing_zeros() as usize + 1;
/// Free page ranges remembered for reuse. If more pile up, the extra virtual
/// space is simply never reused (the physical frames are freed regardless).
const MAX_FREE_RANGES: usize = 64;
/// Freed runs of up to this many pages are kept mapped for reuse...
const CACHED_RUN_PAGES: usize = 16;
/// ...up to this many pages' worth of each length.
const CACHE_PAGES_PER_LENGTH: u64 = 64;

struct FreeBlock {
    next: *mut FreeBlock,
}

#[derive(Clone, Copy)]
struct PageRange {
    start: u64,
    pages: u64,
}

impl PageRange {
    const EMPTY: Self = Self { start: 0, pages: 0 };

    fn end(&self) -> u64 {
        self.start + self.pages * PAGE_SIZE
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Bytes handed out and not yet freed (rounded up to block or page size).
    pub bytes_in_use: u64,
    /// Physical memory backing the heap.
    pub bytes_mapped: u64,
}

struct Heap {
    free_blocks: [*mut FreeBlock; SIZE_CLASSES],
    /// Freed page ranges, sorted by address, never touching each other.
    free_ranges: [PageRange; MAX_FREE_RANGES],
    free_range_count: usize,
    /// Everything from here to the end of the region is unused.
    next: u64,
    /// Freed runs kept mapped, by length in pages (minus one), and how
    /// many each list holds.
    cached_runs: [*mut FreeBlock; CACHED_RUN_PAGES],
    cached_counts: [u64; CACHED_RUN_PAGES],
    stats: Stats,
}

// SAFETY: the raw pointers are into heap memory that the Heap owns.
unsafe impl Send for Heap {}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap {
    free_blocks: [ptr::null_mut(); SIZE_CLASSES],
    free_ranges: [PageRange::EMPTY; MAX_FREE_RANGES],
    free_range_count: 0,
    next: HEAP_START,
    cached_runs: [ptr::null_mut(); CACHED_RUN_PAGES],
    cached_counts: [0; CACHED_RUN_PAGES],
    stats: Stats { bytes_in_use: 0, bytes_mapped: 0 },
});

pub struct KernelHeap;

#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap;

// SAFETY: blocks are handed out at most once until freed, and are big and
// aligned enough for their layout.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.lock().allocate(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP.lock().free(ptr, layout);
    }
}

pub fn stats() -> Stats {
    HEAP.lock().stats
}

/// The size class for a layout, or `None` if it needs whole pages.
fn size_class(layout: Layout) -> Option<usize> {
    let size = layout.size().max(layout.align()).max(MIN_BLOCK).next_power_of_two();
    (size <= MAX_BLOCK).then(|| (size / MIN_BLOCK).trailing_zeros() as usize)
}

fn pages_for(layout: Layout) -> u64 {
    (layout.size() as u64).div_ceil(PAGE_SIZE)
}

/// Which cache list a run of `pages` belongs on, if it's cached at all
/// (runs are page aligned, which covers alignments up to a page).
fn cached_length(pages: u64, layout: Layout) -> Option<usize> {
    (pages as usize <= CACHED_RUN_PAGES && layout.align() as u64 <= PAGE_SIZE).then(|| pages as usize - 1)
}

impl Heap {
    fn allocate(&mut self, layout: Layout) -> *mut u8 {
        match size_class(layout) {
            Some(class) => self.allocate_block(class),
            None => {
                let pages = pages_for(layout);
                let block = match cached_length(pages, layout) {
                    Some(length) if !self.cached_runs[length].is_null() => {
                        let block = self.cached_runs[length];
                        // SAFETY: runs on the list hold a valid link.
                        self.cached_runs[length] = unsafe { (*block).next };
                        self.cached_counts[length] -= 1;
                        block.cast()
                    }
                    _ => self.allocate_pages(pages, (layout.align() as u64).max(PAGE_SIZE)),
                };
                if !block.is_null() {
                    self.stats.bytes_in_use += pages * PAGE_SIZE;
                }
                block
            }
        }
    }

    fn free(&mut self, block: *mut u8, layout: Layout) {
        match size_class(layout) {
            Some(class) => {
                let block = block.cast::<FreeBlock>();
                // SAFETY: the block is ours again and big enough for a link.
                unsafe { block.write(FreeBlock { next: self.free_blocks[class] }) };
                self.free_blocks[class] = block;
                self.stats.bytes_in_use -= (MIN_BLOCK << class) as u64;
            }
            None => {
                let pages = pages_for(layout);
                self.stats.bytes_in_use -= pages * PAGE_SIZE;
                if let Some(length) = cached_length(pages, layout)
                    && (self.cached_counts[length] + 1) * pages <= CACHE_PAGES_PER_LENGTH
                {
                    let run = block.cast::<FreeBlock>();
                    // SAFETY: the run is ours again, mapped, and big enough
                    // for a link.
                    unsafe { run.write(FreeBlock { next: self.cached_runs[length] }) };
                    self.cached_runs[length] = run;
                    self.cached_counts[length] += 1;
                    return;
                }
                self.free_pages(block as u64, pages);
            }
        }
    }

    fn allocate_block(&mut self, class: usize) -> *mut u8 {
        let block_size = MIN_BLOCK << class;
        if self.free_blocks[class].is_null() {
            let page = self.allocate_pages(1, PAGE_SIZE);
            if page.is_null() {
                return page;
            }
            // Thread the page's blocks onto the free list, lowest first.
            for offset in (0..PAGE_SIZE as usize).step_by(block_size).rev() {
                // SAFETY: the page was just mapped for us.
                let block = unsafe { page.add(offset) }.cast::<FreeBlock>();
                unsafe { block.write(FreeBlock { next: self.free_blocks[class] }) };
                self.free_blocks[class] = block;
            }
        }
        let block = self.free_blocks[class];
        // SAFETY: blocks on the free list hold a valid link.
        self.free_blocks[class] = unsafe { (*block).next };
        self.stats.bytes_in_use += block_size as u64;
        block.cast()
    }

    /// Reserves virtual pages and backs each with a fresh frame.
    fn allocate_pages(&mut self, pages: u64, align: u64) -> *mut u8 {
        let Some(start) = self.reserve(pages, align) else {
            return ptr::null_mut();
        };
        let mapped = with_kernel_table(|table| {
            for page in 0..pages {
                let Some(frame) = frames::allocate() else { return page };
                let virt = start + page * PAGE_SIZE;
                if table.map(virt, frame, PageSize::Small, WRITABLE | NO_EXECUTE | GLOBAL).is_err() {
                    frames::free(frame);
                    return page;
                }
            }
            pages
        });
        self.stats.bytes_mapped += mapped * PAGE_SIZE;
        if mapped < pages {
            // Out of memory partway: undo.
            self.free_pages(start, mapped);
            self.release(start + mapped * PAGE_SIZE, pages - mapped);
            return ptr::null_mut();
        }
        start as *mut u8
    }

    fn free_pages(&mut self, start: u64, pages: u64) {
        // In batches: other CPUs must drop their cached translations before
        // the frames can be reused, and the heap can't allocate a list.
        const BATCH: u64 = 64;
        for first in (0..pages).step_by(BATCH as usize) {
            let count = (pages - first).min(BATCH);
            let batch_start = start + first * PAGE_SIZE;
            let mut freed = [0; BATCH as usize];
            with_kernel_table(|table| {
                for (index, frame) in freed[..count as usize].iter_mut().enumerate() {
                    let page = batch_start + index as u64 * PAGE_SIZE;
                    *frame = table.unmap(page).expect("freeing unmapped heap page").phys;
                }
            });
            smp::flush_tlb(batch_start, batch_start + count * PAGE_SIZE);
            freed[..count as usize].iter().for_each(|&frame| frames::free(frame));
        }
        self.stats.bytes_mapped -= pages * PAGE_SIZE;
        self.release(start, pages);
    }

    /// Finds `pages` of unused virtual space: the first freed range that
    /// fits, or else fresh space from the end.
    fn reserve(&mut self, pages: u64, align: u64) -> Option<u64> {
        let len = pages * PAGE_SIZE;
        for index in 0..self.free_range_count {
            let range = self.free_ranges[index];
            let start = range.start.next_multiple_of(align);
            if start + len > range.end() {
                continue;
            }
            // Keep whatever is left on either side.
            let before = PageRange { start: range.start, pages: (start - range.start) / PAGE_SIZE };
            let after = PageRange { start: start + len, pages: (range.end() - start - len) / PAGE_SIZE };
            self.remove_range(index);
            for leftover in [after, before] {
                if leftover.pages > 0 && self.free_range_count < MAX_FREE_RANGES {
                    self.insert_range(index, leftover);
                }
            }
            return Some(start);
        }

        let start = self.next.next_multiple_of(align);
        if start + len > HEAP_END {
            return None;
        }
        if start > self.next {
            self.release(self.next, (start - self.next) / PAGE_SIZE);
        }
        self.next = start + len;
        Some(start)
    }

    /// Returns virtual pages (already unmapped) for reuse.
    fn release(&mut self, start: u64, pages: u64) {
        if pages == 0 {
            return;
        }
        let mut range = PageRange { start, pages };
        let mut index = self.free_ranges[..self.free_range_count].partition_point(|r| r.start < start);
        if index > 0 && self.free_ranges[index - 1].end() == range.start {
            index -= 1;
            range.start = self.free_ranges[index].start;
            range.pages += self.free_ranges[index].pages;
            self.remove_range(index);
        }
        if index < self.free_range_count && range.end() == self.free_ranges[index].start {
            range.pages += self.free_ranges[index].pages;
            self.remove_range(index);
        }
        if range.end() == self.next {
            self.next = range.start;
        } else if self.free_range_count < MAX_FREE_RANGES {
            self.insert_range(index, range);
        }
    }

    fn insert_range(&mut self, index: usize, range: PageRange) {
        self.free_ranges.copy_within(index..self.free_range_count, index + 1);
        self.free_ranges[index] = range;
        self.free_range_count += 1;
    }

    fn remove_range(&mut self, index: usize) {
        self.free_ranges.copy_within(index + 1..self.free_range_count, index);
        self.free_range_count -= 1;
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write as _;

    use super::*;

    crate::ktest! {
        fn boxes_vecs_and_strings_work() {
            let boxed = Box::new(42u64);
            assert_eq!(*boxed, 42);
            let numbers: Vec<u32> = (0..10_000).collect();
            assert_eq!(numbers.iter().map(|&n| u64::from(n)).sum::<u64>(), 49_995_000);
            let mut text = String::new();
            write!(text, "{} + {} = {}", 2, 2, 2 + 2).unwrap();
            assert_eq!(text, "2 + 2 = 4");
        }

        fn freed_blocks_are_reused() {
            let first = Box::new([0u8; 100]);
            let address = &raw const *first;
            drop(first);
            let second = Box::new([1u8; 100]);
            assert_eq!(&raw const *second, address);
        }

        fn allocations_honor_alignment() {
            for (size, align) in [(1, 1), (24, 8), (64, 64), (100, 512), (4096, 4096), (5000, 8192), (64, 4096)] {
                let layout = Layout::from_size_align(size, align).unwrap();
                // SAFETY: the layout is non-zero-sized; freed right after.
                unsafe {
                    let block = KERNEL_HEAP.alloc(layout);
                    assert!(!block.is_null());
                    assert_eq!(block as usize % align, 0, "{size} bytes aligned to {align}");
                    block.write_bytes(0xCC, size);
                    KERNEL_HEAP.dealloc(block, layout);
                }
            }
        }

        fn large_allocations_give_their_memory_back() {
            // The first round may build page tables, which stay.
            drop(Vec::<u8>::with_capacity(1 << 20));
            let free_before = frames::stats().free_bytes;
            let big = alloc::vec![7u8; 1 << 20];
            assert!(frames::stats().free_bytes <= free_before - (1 << 20));
            assert!(big.iter().all(|&b| b == 7));
            drop(big);
            assert_eq!(frames::stats().free_bytes, free_before);
        }

        fn random_allocations_keep_their_contents() {
            // Allocate and free random sizes, checking that no allocation's
            // contents are disturbed by the others.
            let mut seed = 0x2545_F491_4F6C_DD1Du64;
            let mut random = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut slots: [Option<(Vec<u8>, u8)>; 32] = Default::default();
            for _ in 0..1000 {
                let slot = (random() % 32) as usize;
                if let Some((block, fill)) = slots[slot].take() {
                    assert!(block.iter().all(|&b| b == fill), "allocation was corrupted");
                } else {
                    let size = 1 + (random() % 9000) as usize;
                    let fill = random() as u8;
                    slots[slot] = Some((alloc::vec![fill; size], fill));
                }
            }
        }
    }
}
