//! User address spaces.
//!
//! An address space is a set of areas (ranges of virtual memory with access
//! rights) and the page tables mapping them. Pages are allocated on first
//! touch: a page fault inside an area maps a fresh zeroed page there, so
//! reserving a large area (a stack, or an `mmap`) costs nothing up front.

use alloc::collections::BTreeMap;

use crate::errno::{Errno, Result};
use crate::memory::paging::{NO_EXECUTE, PageSize, PageTable, USER, WRITABLE};
use crate::memory::{self, PAGE_SIZE, frames, phys_to_virt};

/// The end of the lower half: user addresses are below this.
pub const USER_END: u64 = 0x0000_8000_0000_0000;
pub const STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const STACK_SIZE: u64 = 8 << 20;
/// `mmap` places mappings below this, working downward.
const MMAP_TOP: u64 = 0x0000_7000_0000_0000;

// Protection bits, with the same values as Linux's PROT_*.
pub const PROT_READ: u32 = 1;
pub const PROT_WRITE: u32 = 2;
pub const PROT_EXEC: u32 = 4;

#[derive(Debug, Clone, Copy)]
struct Area {
    end: u64,
    protection: u32,
}

pub struct AddressSpace {
    table: PageTable,
    /// Areas by start address; they never overlap.
    areas: BTreeMap<u64, Area>,
    /// The heap grown by `brk` runs from `brk_start` to `brk`.
    pub brk_start: u64,
    pub brk: u64,
    /// Pages currently backed by memory.
    resident_pages: u64,
}

fn page_flags(protection: u32) -> u64 {
    let mut flags = USER;
    if protection & PROT_WRITE != 0 {
        flags |= WRITABLE;
    }
    if protection & PROT_EXEC == 0 {
        flags |= NO_EXECUTE;
    }
    flags
}

impl AddressSpace {
    /// An address space with nothing in the user half.
    pub fn new() -> Result<Self> {
        let mut table = PageTable::new().map_err(|_| Errno::ENOMEM)?;
        table.share_kernel_half(memory::kernel_page_table_root());
        Ok(Self { table, areas: BTreeMap::new(), brk_start: 0, brk: 0, resident_pages: 0 })
    }

    /// Physical address of the page tables, for CR3.
    pub fn root(&self) -> u64 {
        self.table.root()
    }

    /// The areas as (start, end, protection), in address order.
    pub fn areas(&self) -> alloc::vec::Vec<(u64, u64, u32)> {
        self.areas.iter().map(|(&start, area)| (start, area.end, area.protection)).collect()
    }

    pub fn resident_bytes(&self) -> u64 {
        self.resident_pages * PAGE_SIZE
    }

    fn area_at(&self, address: u64) -> Option<(u64, Area)> {
        let (&start, &area) = self.areas.range(..=address).next_back()?;
        (address < area.end).then_some((start, area))
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.areas.range(..end).next_back().is_some_and(|(_, area)| area.end > start)
    }

    /// Adds an area. Fails if it overlaps an existing one.
    pub fn add_area(&mut self, start: u64, end: u64, protection: u32) -> Result<()> {
        if !start.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) || start >= end || end > USER_END {
            return Err(Errno::EINVAL);
        }
        if self.overlaps(start, end) {
            return Err(Errno::EEXIST);
        }
        self.areas.insert(start, Area { end, protection });
        Ok(())
    }

    /// Removes `start..end` from whatever areas cover it, splitting them as
    /// needed, and frees the pages.
    pub fn remove(&mut self, start: u64, end: u64) {
        self.split_at(start);
        self.split_at(end);
        let doomed: alloc::vec::Vec<u64> = self.areas.range(start..end).map(|(&s, _)| s).collect();
        for area_start in doomed {
            let area = self.areas.remove(&area_start).unwrap();
            for page in (area_start..area.end).step_by(PAGE_SIZE as usize) {
                if let Some(mapping) = self.table.unmap(page) {
                    frames::free(mapping.phys);
                    self.resident_pages -= 1;
                }
            }
        }
    }

    /// Changes the access rights of `start..end`, which must be covered by
    /// areas.
    pub fn protect(&mut self, start: u64, end: u64, protection: u32) -> Result<()> {
        let mut address = start;
        while address < end {
            let (_, area) = self.area_at(address).ok_or(Errno::ENOMEM)?;
            address = area.end;
        }
        self.split_at(start);
        self.split_at(end);
        let starts: alloc::vec::Vec<u64> = self.areas.range(start..end).map(|(&s, _)| s).collect();
        for area_start in starts {
            let area = self.areas.get_mut(&area_start).unwrap();
            area.protection = protection;
            let area_end = area.end;
            for page in (area_start..area_end).step_by(PAGE_SIZE as usize) {
                self.table.set_flags(page, page_flags(protection));
            }
        }
        Ok(())
    }

    /// Splits the area containing `address` so an area boundary falls there.
    fn split_at(&mut self, address: u64) {
        if let Some((start, area)) = self.area_at(address)
            && start != address
        {
            self.areas.insert(start, Area { end: address, protection: area.protection });
            self.areas.insert(address, area);
        }
    }

    /// Finds room for `len` bytes, below the `mmap` ceiling.
    pub fn find_free(&self, len: u64) -> Option<u64> {
        let mut top = MMAP_TOP;
        // Walk areas from the top down, looking for a gap.
        for (&start, area) in self.areas.range(..MMAP_TOP).rev() {
            if area.end <= top && top - area.end >= len {
                return Some(top - len);
            }
            top = top.min(start);
        }
        top.checked_sub(len).filter(|&start| start >= 0x1_0000_0000)
    }

    /// Resolves a page fault at `address` by mapping a fresh page, if the
    /// access is allowed there. Returns whether it was resolved.
    pub fn handle_fault(&mut self, address: u64, write: bool, execute: bool) -> bool {
        let Some((_, area)) = self.area_at(address) else { return false };
        let allowed = if write {
            area.protection & PROT_WRITE != 0
        } else if execute {
            area.protection & PROT_EXEC != 0
        } else {
            area.protection != 0
        };
        let page = address & !(PAGE_SIZE - 1);
        if !allowed || self.table.translate(page).is_some() {
            // Not permitted, or the page exists and this was a protection
            // violation.
            return false;
        }
        self.map_fresh_page(page, area.protection).is_ok()
    }

    fn map_fresh_page(&mut self, page: u64, protection: u32) -> Result<u64> {
        let frame = frames::allocate_zeroed().ok_or(Errno::ENOMEM)?;
        if self.table.map(page, frame, PageSize::Small, page_flags(protection)).is_err() {
            frames::free(frame);
            return Err(Errno::ENOMEM);
        }
        self.resident_pages += 1;
        Ok(frame)
    }

    /// The frame backing `page`, mapping one if the area allows it. Ignores
    /// access rights: the kernel can write to read-only areas (to load
    /// programs, for example).
    fn frame_for(&mut self, page: u64) -> Result<u64> {
        if let Some(mapping) = self.table.translate(page) {
            return Ok(mapping.phys);
        }
        let (_, area) = self.area_at(page).ok_or(Errno::EFAULT)?;
        self.map_fresh_page(page, area.protection)
    }

    /// Checks that the kernel may access `address..address + len` on the
    /// program's behalf, and maps any pages not yet touched, so the access
    /// can't fault.
    pub fn prepare_access(&mut self, address: u64, len: u64, write: bool) -> Result<()> {
        let end = address.checked_add(len).filter(|&end| end <= USER_END).ok_or(Errno::EFAULT)?;
        let mut page = address & !(PAGE_SIZE - 1);
        while page < end {
            let (_, area) = self.area_at(page).ok_or(Errno::EFAULT)?;
            let needed = if write { PROT_WRITE } else { PROT_READ };
            if area.protection & needed == 0 {
                return Err(Errno::EFAULT);
            }
            self.frame_for(page)?;
            page += PAGE_SIZE;
        }
        Ok(())
    }

    /// Copies `data` to `address`, which needn't be the active address space.
    pub fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let current = address + done as u64;
            let offset = (current % PAGE_SIZE) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(data.len() - done);
            let frame = self.frame_for(current - offset as u64)?;
            // SAFETY: the frame belongs to this address space; the direct
            // map reaches it whether or not this space is active.
            unsafe {
                core::ptr::copy_nonoverlapping(data[done..].as_ptr(), phys_to_virt(frame).add(offset), chunk);
            }
            done += chunk;
        }
        Ok(())
    }

    /// A copy of this address space, page by page (for `fork`).
    pub fn duplicate(&self) -> Result<Self> {
        let mut copy = Self::new()?;
        copy.areas = self.areas.clone();
        copy.brk_start = self.brk_start;
        copy.brk = self.brk;
        for (virt, mapping) in self.table.user_pages() {
            let frame = frames::allocate().ok_or(Errno::ENOMEM)?;
            // SAFETY: both frames are RAM in the direct map; the new one is
            // ours alone.
            unsafe {
                core::ptr::copy_nonoverlapping(phys_to_virt(mapping.phys), phys_to_virt(frame), PAGE_SIZE as usize);
            }
            let flags = mapping.flags & (USER | WRITABLE | NO_EXECUTE);
            if copy.table.map(virt, frame, PageSize::Small, flags).is_err() {
                frames::free(frame);
                return Err(Errno::ENOMEM);
            }
            copy.resident_pages += 1;
        }
        Ok(copy)
    }

    /// Moves the end of the `brk` heap, returning the new end (unchanged if
    /// the request can't be met, which is how `brk` reports failure).
    pub fn set_brk(&mut self, requested: u64) -> u64 {
        if requested < self.brk_start {
            return self.brk;
        }
        let old_end = self.brk.next_multiple_of(PAGE_SIZE);
        let new_end = requested.next_multiple_of(PAGE_SIZE);
        if new_end > old_end {
            if self.overlaps(old_end, new_end) {
                return self.brk;
            }
            // Extend the heap area (or create it).
            let heap = if old_end > self.brk_start { self.area_at(old_end - 1) } else { None };
            match heap {
                Some((start, area)) if area.end == old_end => {
                    self.areas.insert(start, Area { end: new_end, protection: area.protection });
                }
                _ => {
                    if self.add_area(old_end, new_end, PROT_READ | PROT_WRITE).is_err() {
                        return self.brk;
                    }
                }
            }
        } else if new_end < old_end {
            self.remove(new_end, old_end);
        }
        self.brk = requested;
        self.brk
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        for (_, mapping) in self.table.user_pages() {
            frames::free(mapping.phys);
        }
        // SAFETY: a placeholder that's never used; the real tables are
        // consumed by free_user_tables.
        let table = core::mem::replace(&mut self.table, unsafe { PageTable::from_root(0) });
        table.free_user_tables();
    }
}
