//! The memory allocator for programs, built on `mmap`.
//!
//! Like the kernel's heap: small blocks come from power-of-two size classes
//! carved out of 64 KiB chunks; anything bigger gets its own mapping, which
//! is unmapped when freed.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use crate::sys;

const MIN_BLOCK: usize = 16;
const MAX_BLOCK: usize = 4096;
const CLASSES: usize = (MAX_BLOCK / MIN_BLOCK).trailing_zeros() as usize + 1;
const CHUNK: usize = 64 * 1024;
const PAGE: usize = 4096;

struct FreeBlock {
    next: *mut FreeBlock,
}

struct Heap {
    free: UnsafeCell<[*mut FreeBlock; CLASSES]>,
}

// SAFETY: eliOS processes have a single thread.
unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap { free: UnsafeCell::new([ptr::null_mut(); CLASSES]) };

fn class_of(layout: Layout) -> Option<usize> {
    let size = layout.size().max(layout.align()).max(MIN_BLOCK).next_power_of_two();
    (size <= MAX_BLOCK).then(|| (size / MIN_BLOCK).trailing_zeros() as usize)
}

fn map(len: usize) -> *mut u8 {
    // PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS.
    // SAFETY: an anonymous mapping has no preconditions.
    let result = unsafe { sys::syscall(sys::MMAP, [0, len, 3, 0x22, usize::MAX, 0]) };
    if result < 0 { ptr::null_mut() } else { result as *mut u8 }
}

// SAFETY: blocks are handed out once until freed and suit their layout.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = class_of(layout) else {
            // Page-sized alignment covers anything sensible.
            return if layout.align() <= PAGE { map(layout.size().next_multiple_of(PAGE)) } else { ptr::null_mut() };
        };
        // SAFETY: single-threaded, so no one else touches the free lists.
        let free = unsafe { &mut *self.free.get() };
        if free[class].is_null() {
            let chunk = map(CHUNK);
            if chunk.is_null() {
                return chunk;
            }
            let size = MIN_BLOCK << class;
            for offset in (0..CHUNK).step_by(size).rev() {
                let block = unsafe { chunk.add(offset) }.cast::<FreeBlock>();
                unsafe { block.write(FreeBlock { next: free[class] }) };
                free[class] = block;
            }
        }
        let block = free[class];
        free[class] = unsafe { (*block).next };
        block.cast()
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        match class_of(layout) {
            Some(class) => {
                let free = unsafe { &mut *self.free.get() };
                let block = block.cast::<FreeBlock>();
                unsafe { block.write(FreeBlock { next: free[class] }) };
                free[class] = block;
            }
            None => unsafe {
                sys::syscall(sys::MUNMAP, [block as usize, layout.size().next_multiple_of(PAGE), 0, 0, 0, 0]);
            },
        }
    }
}
