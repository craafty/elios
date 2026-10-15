//! Memory devices read and write themselves (DMA): physically contiguous,
//! zeroed pages, freed when dropped.

use core::ptr;

use crate::memory::{PAGE_SIZE, frames, phys_to_virt};

pub struct DmaBuffer {
    pub phys: u64,
    pages: usize,
}

impl DmaBuffer {
    /// `pages` contiguous pages, starting at a multiple of `align` pages.
    pub fn new(pages: usize, align: usize) -> Option<Self> {
        let phys = frames::allocate_contiguous(pages, align)?;
        // SAFETY: freshly allocated RAM, in the direct map.
        unsafe { ptr::write_bytes(phys_to_virt(phys), 0, pages * PAGE_SIZE as usize) };
        Some(Self { phys, pages })
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE_SIZE as usize
    }

    fn pointer<T>(&self, offset: usize) -> *mut T {
        assert!(offset + size_of::<T>() <= self.len(), "DMA access out of bounds");
        // SAFETY: within the allocation (checked above).
        unsafe { phys_to_virt(self.phys).add(offset).cast() }
    }

    // The device may change the memory at any time, hence volatile.

    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: in bounds (see `pointer`); unaligned reads are fine on
        // x86.
        unsafe { ptr::read_unaligned(self.pointer::<u16>(offset).cast_const()) }
    }

    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: as for `read16`.
        unsafe { ptr::read_unaligned(self.pointer::<u32>(offset).cast_const()) }
    }

    pub fn write8(&self, offset: usize, value: u8) {
        // SAFETY: in bounds.
        unsafe { ptr::write_volatile(self.pointer(offset), value) };
    }

    pub fn write16(&self, offset: usize, value: u16) {
        // SAFETY: in bounds; unaligned writes are fine on x86.
        unsafe { ptr::write_unaligned(self.pointer(offset), value) };
    }

    pub fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as for `write16`.
        unsafe { ptr::write_unaligned(self.pointer(offset), value) };
    }

    /// Sets every byte to zero.
    pub fn zero(&self) {
        // SAFETY: the whole allocation.
        unsafe { ptr::write_bytes(phys_to_virt(self.phys), 0, self.len()) };
    }

    /// Copies `bytes` in at `offset`.
    pub fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.len(), "DMA write out of bounds");
        // SAFETY: in bounds (checked above); the source is ordinary memory.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), phys_to_virt(self.phys).add(offset), bytes.len()) };
    }

    /// Copies `len` bytes out from `offset`.
    pub fn read_bytes(&self, offset: usize, len: usize) -> alloc::vec::Vec<u8> {
        assert!(offset + len <= self.len(), "DMA read out of bounds");
        let mut bytes = alloc::vec![0; len];
        // SAFETY: in bounds (checked above).
        unsafe { ptr::copy_nonoverlapping(phys_to_virt(self.phys).add(offset), bytes.as_mut_ptr(), len) };
        bytes
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        for page in 0..self.pages as u64 {
            frames::free(self.phys + page * PAGE_SIZE);
        }
    }
}
