//! 4-level x86_64 page tables.
//!
//! Tables are reached through the direct map, so any page table can be
//! edited, active or not.

use alloc::vec::Vec;

use super::{frames, phys_to_virt};
use crate::arch;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const CACHE_DISABLE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;

const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    /// 4 KiB
    Small,
    /// 2 MiB
    Large,
    /// 1 GiB
    Huge,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Small => 4 << 10,
            PageSize::Large => 2 << 20,
            PageSize::Huge => 1 << 30,
        }
    }

    /// The table level whose entries map pages of this size (1 = page table,
    /// 4 = PML4).
    const fn level(self) -> u32 {
        match self {
            PageSize::Small => 1,
            PageSize::Large => 2,
            PageSize::Huge => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Something is already mapped there.
    AlreadyMapped,
    /// No memory left for a page table.
    OutOfMemory,
    /// The addresses aren't aligned to the page size.
    Misaligned,
}

/// One mapped page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    /// Physical address of the start of the page.
    pub phys: u64,
    pub size: PageSize,
    /// The entry's flag bits.
    pub flags: u64,
}

pub struct PageTable {
    root: u64,
}

type Entries = [u64; 512];

fn entries(table: u64) -> &'static mut Entries {
    // SAFETY: `table` is always a page table frame, and all RAM is in the
    // direct map. Callers serialize access to a given address space.
    unsafe { &mut *phys_to_virt(table).cast::<Entries>() }
}

fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * (level - 1))) & 0x1FF) as usize
}

/// Whether the CPU supports 1 GiB pages.
fn gigabyte_pages() -> bool {
    arch::cpuid(0x8000_0001, 0)[3] & (1 << 26) != 0
}

impl PageTable {
    /// An empty address space.
    pub fn new() -> Result<Self, MapError> {
        Ok(Self { root: frames::allocate_zeroed().ok_or(MapError::OutOfMemory)? })
    }

    /// Wraps existing tables, such as the ones the bootloader built.
    ///
    /// # Safety
    /// `root` must be a valid PML4 that outlives the returned value.
    pub unsafe fn from_root(root: u64) -> Self {
        Self { root }
    }

    /// Physical address of the PML4, for CR3.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// Returns the table at `level` covering `virt`, creating missing
    /// intermediate tables. Their permissions are as loose as possible; the
    /// leaf entry decides access.
    fn table_for(&mut self, virt: u64, level: u32, user: bool) -> Result<u64, MapError> {
        let mut table = self.root;
        for current in (level + 1..=4).rev() {
            let entry = &mut entries(table)[index(virt, current)];
            if *entry & PRESENT == 0 {
                let next = frames::allocate_zeroed().ok_or(MapError::OutOfMemory)?;
                *entry = next | PRESENT | WRITABLE;
            } else if *entry & HUGE != 0 {
                return Err(MapError::AlreadyMapped);
            }
            if user {
                *entry |= USER;
            }
            table = *entry & ADDRESS_MASK;
        }
        Ok(table)
    }

    /// Makes sure the top-level entry covering `virt` points at a table.
    /// Address spaces that copy the kernel's top-level entries then see every
    /// kernel mapping made later, too.
    pub fn reserve_top_level(&mut self, virt: u64) -> Result<(), MapError> {
        self.table_for(virt, 3, false).map(|_| ())
    }

    pub fn map(&mut self, virt: u64, phys: u64, size: PageSize, flags: u64) -> Result<(), MapError> {
        if !virt.is_multiple_of(size.bytes()) || !phys.is_multiple_of(size.bytes()) {
            return Err(MapError::Misaligned);
        }
        let table = self.table_for(virt, size.level(), flags & USER != 0)?;
        let entry = &mut entries(table)[index(virt, size.level())];
        if *entry & PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        let huge = if size == PageSize::Small { 0 } else { HUGE };
        *entry = phys | flags | huge | PRESENT;
        Ok(())
    }

    /// Maps `len` bytes (a multiple of 4 KiB) using the largest pages the
    /// alignment of each piece allows.
    pub fn map_range(&mut self, virt: u64, phys: u64, len: u64, flags: u64) -> Result<(), MapError> {
        let sizes: &[PageSize] = if gigabyte_pages() {
            &[PageSize::Huge, PageSize::Large, PageSize::Small]
        } else {
            &[PageSize::Large, PageSize::Small]
        };
        let mut offset = 0;
        while offset < len {
            let (virt, phys, remaining) = (virt + offset, phys + offset, len - offset);
            let size = *sizes
                .iter()
                .find(|size| virt % size.bytes() == 0 && phys % size.bytes() == 0 && remaining >= size.bytes())
                .ok_or(MapError::Misaligned)?;
            self.map(virt, phys, size, flags)?;
            offset += size.bytes();
        }
        Ok(())
    }

    /// Removes the page containing `virt` and returns what was mapped. Frees
    /// nothing: the caller owns the physical memory.
    pub fn unmap(&mut self, virt: u64) -> Option<Mapping> {
        let (entry, mapping) = self.leaf(virt)?;
        *entry = 0;
        arch::flush_tlb_page(virt);
        Some(mapping)
    }

    /// Changes the flags of the page mapping `virt`, keeping its address.
    pub fn set_flags(&mut self, virt: u64, flags: u64) -> Option<()> {
        let (entry, mapping) = self.leaf(virt)?;
        let huge = if mapping.size == PageSize::Small { 0 } else { HUGE };
        *entry = mapping.phys | flags | huge | PRESENT;
        arch::flush_tlb_page(virt);
        Some(())
    }

    /// Every page mapped in the lower (user) half, with its address.
    pub fn user_pages(&self) -> Vec<(u64, Mapping)> {
        let mut pages = Vec::new();
        for (pml4_index, &pml4e) in entries(self.root)[..256].iter().enumerate() {
            if pml4e & PRESENT == 0 {
                continue;
            }
            for (pdpt_index, &pdpte) in entries(pml4e & ADDRESS_MASK).iter().enumerate() {
                if pdpte & PRESENT == 0 || pdpte & HUGE != 0 {
                    continue;
                }
                for (pd_index, &pde) in entries(pdpte & ADDRESS_MASK).iter().enumerate() {
                    if pde & PRESENT == 0 || pde & HUGE != 0 {
                        continue;
                    }
                    for (pt_index, &pte) in entries(pde & ADDRESS_MASK).iter().enumerate() {
                        if pte & PRESENT != 0 {
                            let virt = (pml4_index as u64) << 39
                                | (pdpt_index as u64) << 30
                                | (pd_index as u64) << 21
                                | (pt_index as u64) << 12;
                            let mapping =
                                Mapping { phys: pte & ADDRESS_MASK, size: PageSize::Small, flags: pte & !ADDRESS_MASK };
                            pages.push((virt, mapping));
                        }
                    }
                }
            }
        }
        pages
    }

    /// Frees the page tables of the lower half and the PML4 itself. The
    /// pages they mapped must already have been dealt with.
    pub fn free_user_tables(self) {
        for &pml4e in &entries(self.root)[..256] {
            if pml4e & PRESENT == 0 {
                continue;
            }
            let pdpt = pml4e & ADDRESS_MASK;
            for &pdpte in entries(pdpt).iter() {
                if pdpte & PRESENT == 0 || pdpte & HUGE != 0 {
                    continue;
                }
                let pd = pdpte & ADDRESS_MASK;
                for &pde in entries(pd).iter() {
                    if pde & PRESENT != 0 && pde & HUGE == 0 {
                        frames::free(pde & ADDRESS_MASK);
                    }
                }
                frames::free(pd);
            }
            frames::free(pdpt);
        }
        frames::free(self.root);
    }

    /// Copies the top-level entries for the kernel half from `kernel`, so
    /// this address space shares all kernel mappings.
    pub fn share_kernel_half(&mut self, kernel: u64) {
        let source = *entries(kernel);
        entries(self.root)[256..].copy_from_slice(&source[256..]);
    }

    /// The page containing `virt`, if mapped.
    pub fn translate(&self, virt: u64) -> Option<Mapping> {
        self.leaf(virt).map(|(_, mapping)| mapping)
    }

    /// The physical address `virt` maps to, if mapped.
    pub fn translate_address(&self, virt: u64) -> Option<u64> {
        self.translate(virt).map(|mapping| mapping.phys + virt % mapping.size.bytes())
    }

    /// The leaf entry mapping `virt`, and what it maps. Only `unmap` (which
    /// has `&mut self`) writes through the returned entry.
    fn leaf(&self, virt: u64) -> Option<(&'static mut u64, Mapping)> {
        let mut table = self.root;
        for level in (1..=4).rev() {
            let entry = &mut entries(table)[index(virt, level)];
            if *entry & PRESENT == 0 {
                return None;
            }
            let size = match level {
                1 => Some(PageSize::Small),
                2 if *entry & HUGE != 0 => Some(PageSize::Large),
                3 if *entry & HUGE != 0 => Some(PageSize::Huge),
                _ => None,
            };
            if let Some(size) = size {
                let address_bits = ADDRESS_MASK & !(size.bytes() - 1);
                let mapping = Mapping { phys: *entry & address_bits, size, flags: *entry & !ADDRESS_MASK };
                return Some((entry, mapping));
            }
            table = *entry & ADDRESS_MASK;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn map_translate_unmap() {
            let mut table = PageTable::new().unwrap();
            let frame = frames::allocate().unwrap();
            table.map(0x4000_1000, frame, PageSize::Small, WRITABLE).unwrap();
            assert_eq!(table.translate_address(0x4000_1234), Some(frame + 0x234));
            assert_eq!(table.map(0x4000_1000, frame, PageSize::Small, 0), Err(MapError::AlreadyMapped));
            let mapping = table.unmap(0x4000_1000).unwrap();
            assert_eq!(mapping, Mapping { phys: frame, size: PageSize::Small, flags: WRITABLE | PRESENT });
            assert_eq!(table.translate(0x4000_1000), None);
            frames::free(frame);
        }

        fn map_range_uses_large_pages_where_aligned() {
            let mut table = PageTable::new().unwrap();
            // 4 KiB short of a 2 MiB boundary, running 4 KiB past the next one.
            let start = 0x1F_F000;
            table.map_range(start, start, (2 << 20) + 8192, 0).unwrap();
            assert_eq!(table.translate(0x1F_F000).unwrap().size, PageSize::Small);
            assert_eq!(table.translate(0x20_0000).unwrap().size, PageSize::Large);
            assert_eq!(table.translate_address(0x3F_FFFF), Some(0x3F_FFFF));
            assert_eq!(table.translate(0x40_0000).unwrap().size, PageSize::Small);
            assert_eq!(table.translate(0x40_1000), None);
        }

        fn misaligned_and_overlapping_maps_are_refused() {
            let mut table = PageTable::new().unwrap();
            assert_eq!(table.map(0x1000, 0x20_0000, PageSize::Large, 0), Err(MapError::Misaligned));
            table.map(0x20_0000, 0x20_0000, PageSize::Large, 0).unwrap();
            assert_eq!(table.map(0x20_1000, 0x1000, PageSize::Small, 0), Err(MapError::AlreadyMapped));
        }
    }
}
