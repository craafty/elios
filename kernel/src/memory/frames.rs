//! The physical frame allocator: hands out 4 KiB pages of RAM.
//!
//! One bit per frame of physical memory records whether it's in use. The
//! bitmap lives at the start of the first free region big enough for it.
//! Single-frame allocations search onward from the last one (next fit), so
//! they're usually found in the first word checked.
//!
//! Frames can be shared: a page mapped into two processes after `fork`, or
//! a file's page in the page cache and in every program that maps it. A
//! count per frame (next to the bitmap) says how many owners it has;
//! [`share`] adds one, and [`free`] drops one and frees the frame when the
//! last goes.

use bootinfo::{MemoryKind, MemoryRegion};

use super::{PAGE_SIZE, phys_to_virt};
use crate::sync::SpinLock;

/// Memory below 1 MiB is never handed out: starting other CPUs (later)
/// needs pages there, and firmware leftovers like to live there.
const LOW_MEMORY_END: u64 = 1 << 20;

struct FrameAllocator {
    /// Bit set = frame in use (or not RAM at all).
    bitmap: &'static mut [u64],
    /// Owners of each frame in use (0 for ones never handed out).
    counts: &'static mut [u32],
    /// Frames ever made available: free RAM plus what was reclaimed.
    managed: usize,
    free: usize,
    /// Word index where the next search starts.
    next: usize,
}

static FRAMES: SpinLock<Option<FrameAllocator>> = SpinLock::new(None);

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub managed_bytes: u64,
    pub free_bytes: u64,
}

/// Whether memory of this kind can ever become ordinary free RAM.
fn may_become_free(kind: MemoryKind) -> bool {
    matches!(
        kind,
        MemoryKind::Usable | MemoryKind::BootloaderReclaimable | MemoryKind::PageTables | MemoryKind::KernelStack
    )
}

/// Builds the allocator from the boot memory map, with every usable frame
/// free. Must run while the bootloader's direct map is active.
pub fn init(regions: &[MemoryRegion]) {
    let end = regions.iter().filter(|r| may_become_free(r.kind)).map(|r| r.end()).max().unwrap_or(0);
    let frames = (end / PAGE_SIZE) as usize;
    let words = frames.div_ceil(64);
    let bitmap_bytes = (words * 8) as u64;
    let counts_bytes = (words * 64 * 4) as u64;
    let needed = bitmap_bytes + counts_bytes;

    let home = regions
        .iter()
        .filter(|r| r.kind == MemoryKind::Usable)
        .map(|r| (r.start.max(LOW_MEMORY_END), r.end()))
        .find(|&(start, end)| end > start && end - start >= needed)
        .expect("no free memory region can hold the frame bitmap");

    // SAFETY: the region is free RAM in the direct map, and nothing else
    // uses it: the allocator is what hands out free RAM. The counts follow
    // the bitmap (which is a multiple of 8 bytes long).
    let (bitmap, counts) = unsafe {
        (
            core::slice::from_raw_parts_mut(phys_to_virt(home.0).cast::<u64>(), words),
            core::slice::from_raw_parts_mut(phys_to_virt(home.0 + bitmap_bytes).cast::<u32>(), words * 64),
        )
    };
    bitmap.fill(u64::MAX);
    counts.fill(0);

    let mut allocator = FrameAllocator { bitmap, counts, managed: 0, free: 0, next: 0 };
    for region in regions.iter().filter(|r| r.kind == MemoryKind::Usable) {
        allocator.release_range(region.start, region.end());
    }
    allocator.claim_range(home.0, home.0 + needed.next_multiple_of(PAGE_SIZE));
    *FRAMES.lock() = Some(allocator);
}

impl FrameAllocator {
    fn is_free(&self, frame: usize) -> bool {
        self.bitmap[frame / 64] & (1 << (frame % 64)) == 0
    }

    fn mark_used(&mut self, frame: usize) {
        self.bitmap[frame / 64] |= 1 << (frame % 64);
        self.free -= 1;
    }

    fn mark_free(&mut self, frame: usize) {
        self.bitmap[frame / 64] &= !(1 << (frame % 64));
        self.free += 1;
    }

    /// Makes the whole frames in `start..end` available, skipping low memory.
    /// Returns how many frames were added.
    fn release_range(&mut self, start: u64, end: u64) -> usize {
        let first = start.max(LOW_MEMORY_END).div_ceil(PAGE_SIZE) as usize;
        let last = ((end / PAGE_SIZE) as usize).min(self.bitmap.len() * 64);
        let mut added = 0;
        for frame in first..last {
            if !self.is_free(frame) {
                self.mark_free(frame);
                added += 1;
            }
        }
        self.managed += added;
        added
    }

    fn claim_range(&mut self, start: u64, end: u64) {
        for frame in (start / PAGE_SIZE) as usize..end.div_ceil(PAGE_SIZE) as usize {
            if self.is_free(frame) {
                self.mark_used(frame);
                self.managed -= 1;
            }
        }
    }

    fn allocate(&mut self) -> Option<u64> {
        let words = self.bitmap.len();
        for offset in 0..words {
            let index = (self.next + offset) % words;
            let word = self.bitmap[index];
            if word != u64::MAX {
                let frame = index * 64 + word.trailing_ones() as usize;
                self.mark_used(frame);
                self.counts[frame] = 1;
                self.next = index;
                return Some(frame as u64 * PAGE_SIZE);
            }
        }
        None
    }

    fn allocate_contiguous(&mut self, count: usize, align: usize) -> Option<u64> {
        let total = self.bitmap.len() * 64;
        let mut start = ((LOW_MEMORY_END / PAGE_SIZE) as usize).next_multiple_of(align);
        while start + count <= total {
            match (start..start + count).find(|&frame| !self.is_free(frame)) {
                // Skip past the frame that's in use.
                Some(used) => start = (used + 1).next_multiple_of(align),
                None => {
                    for frame in start..start + count {
                        self.mark_used(frame);
                        self.counts[frame] = 1;
                    }
                    return Some(start as u64 * PAGE_SIZE);
                }
            }
        }
        None
    }

    /// Drops one owner of a frame, freeing it with the last. (Frames that
    /// were never handed out, like the bootloader's, have no count and are
    /// freed outright.)
    fn free(&mut self, address: u64) {
        let frame = (address / PAGE_SIZE) as usize;
        assert!(address.is_multiple_of(PAGE_SIZE), "freeing misaligned frame {address:#x}");
        assert!(!self.is_free(frame), "frame {address:#x} freed twice");
        if self.counts[frame] > 1 {
            self.counts[frame] -= 1;
            return;
        }
        self.counts[frame] = 0;
        self.mark_free(frame);
    }

    fn share(&mut self, address: u64) {
        let frame = (address / PAGE_SIZE) as usize;
        assert!(self.counts[frame] > 0, "sharing frame {address:#x}, which isn't allocated");
        self.counts[frame] += 1;
    }
}

fn with<R>(f: impl FnOnce(&mut FrameAllocator) -> R) -> R {
    f(FRAMES.lock().as_mut().expect("frame allocator used before init"))
}

/// Allocates one frame. Its contents are whatever was there before.
pub fn allocate() -> Option<u64> {
    with(FrameAllocator::allocate)
}

/// Allocates one frame filled with zeros.
pub fn allocate_zeroed() -> Option<u64> {
    let frame = allocate()?;
    // SAFETY: we own the frame, and all RAM is in the direct map.
    unsafe { core::ptr::write_bytes(phys_to_virt(frame), 0, PAGE_SIZE as usize) };
    Some(frame)
}

/// Allocates `count` physically contiguous frames, starting on a multiple of
/// `align` frames. For devices that need contiguous memory.
pub fn allocate_contiguous(count: usize, align: usize) -> Option<u64> {
    with(|frames| frames.allocate_contiguous(count, align.max(1)))
}

/// Drops one owner of a frame, returning it to the free pool when it has
/// no more. Panics on a double free.
pub fn free(address: u64) {
    with(|frames| frames.free(address));
}

/// Adds an owner to an allocated frame (it's now mapped or cached in one
/// more place); each owner calls [`free`] when done with it.
pub fn share(address: u64) {
    with(|frames| frames.share(address));
}

/// How many owners a frame has: 1 means whoever asks owns it alone.
pub fn owners(address: u64) -> u32 {
    with(|frames| frames.counts[(address / PAGE_SIZE) as usize])
}

/// Adds a range of RAM that was in use (by the bootloader, say) to the free
/// pool. Returns the number of bytes added.
pub fn reclaim(start: u64, end: u64) -> u64 {
    with(|frames| frames.release_range(start, end)) as u64 * PAGE_SIZE
}

pub fn stats() -> Stats {
    with(|frames| Stats {
        managed_bytes: frames.managed as u64 * PAGE_SIZE,
        free_bytes: frames.free as u64 * PAGE_SIZE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn frames_can_be_allocated_and_freed() {
            let before = stats().free_bytes;
            let a = allocate().unwrap();
            let b = allocate().unwrap();
            assert_ne!(a, b);
            assert!(a.is_multiple_of(PAGE_SIZE) && a >= LOW_MEMORY_END);
            assert_eq!(stats().free_bytes, before - 2 * PAGE_SIZE);
            free(a);
            free(b);
            assert_eq!(stats().free_bytes, before);
        }

        fn zeroed_frames_are_zero() {
            let frame = allocate().unwrap();
            // SAFETY: we own the frame.
            unsafe { core::ptr::write_bytes(phys_to_virt(frame), 0xAB, PAGE_SIZE as usize) };
            free(frame);
            // Next fit hands out a nearby frame; whichever we get must be zero.
            let frame = allocate_zeroed().unwrap();
            // SAFETY: as above.
            let bytes = unsafe { core::slice::from_raw_parts(phys_to_virt(frame), PAGE_SIZE as usize) };
            assert!(bytes.iter().all(|&b| b == 0));
            free(frame);
        }

        fn shared_frames_are_freed_by_their_last_owner() {
            let before = stats().free_bytes;
            let frame = allocate().unwrap();
            share(frame);
            share(frame);
            assert_eq!(owners(frame), 3);
            free(frame);
            free(frame);
            assert_eq!(stats().free_bytes, before - PAGE_SIZE);
            free(frame);
            assert_eq!(stats().free_bytes, before);
        }

        fn contiguous_runs_are_aligned_and_free() {
            let start = allocate_contiguous(16, 16).unwrap();
            assert_eq!(start % (16 * PAGE_SIZE), 0);
            with(|frames| {
                for frame in 0..16 {
                    assert!(!frames.is_free((start / PAGE_SIZE) as usize + frame));
                }
            });
            for frame in 0..16 {
                free(start + frame * PAGE_SIZE);
            }
        }
    }
}
