//! The physical frame allocator: hands out 4 KiB pages of RAM.
//!
//! One bit per frame of physical memory records whether it's in use. The
//! bitmap lives at the start of the first free region big enough for it.
//! Single-frame allocations search onward from the last one (next fit), so
//! they're usually found in the first word checked.

use bootinfo::{MemoryKind, MemoryRegion};

use super::{PAGE_SIZE, phys_to_virt};
use crate::sync::SpinLock;

/// Memory below 1 MiB is never handed out: starting other CPUs (later)
/// needs pages there, and firmware leftovers like to live there.
const LOW_MEMORY_END: u64 = 1 << 20;

struct FrameAllocator {
    /// Bit set = frame in use (or not RAM at all).
    bitmap: &'static mut [u64],
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

    let home = regions
        .iter()
        .filter(|r| r.kind == MemoryKind::Usable)
        .map(|r| (r.start.max(LOW_MEMORY_END), r.end()))
        .find(|&(start, end)| end > start && end - start >= bitmap_bytes)
        .expect("no free memory region can hold the frame bitmap");

    // SAFETY: the region is free RAM in the direct map, and nothing else
    // uses it: the allocator is what hands out free RAM.
    let bitmap = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(home.0).cast::<u64>(), words) };
    bitmap.fill(u64::MAX);

    let mut allocator = FrameAllocator { bitmap, managed: 0, free: 0, next: 0 };
    for region in regions.iter().filter(|r| r.kind == MemoryKind::Usable) {
        allocator.release_range(region.start, region.end());
    }
    allocator.claim_range(home.0, home.0 + bitmap_bytes.next_multiple_of(PAGE_SIZE));
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
                    (start..start + count).for_each(|frame| self.mark_used(frame));
                    return Some(start as u64 * PAGE_SIZE);
                }
            }
        }
        None
    }

    fn free(&mut self, address: u64) {
        let frame = (address / PAGE_SIZE) as usize;
        assert!(address.is_multiple_of(PAGE_SIZE), "freeing misaligned frame {address:#x}");
        assert!(!self.is_free(frame), "frame {address:#x} freed twice");
        self.mark_free(frame);
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

/// Returns a frame to the free pool. Panics on a double free.
pub fn free(address: u64) {
    with(|frames| frames.free(address));
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
