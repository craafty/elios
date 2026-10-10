//! Builds the 4-level page tables the kernel starts on.
//!
//! While boot services are running, the firmware identity-maps all memory,
//! so a table's physical address is also a usable pointer to it.

use bootinfo::HHDM_OFFSET;

use crate::memory::{self, ELIOS_PAGE_TABLES, PAGE_SIZE};
use crate::uefi::Error;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
const HUGE_PAGE: u64 = 1 << 7;
pub const NO_EXECUTE: u64 = 1 << 63;

const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const HUGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

type Table = [u64; 512];

pub struct PageTables {
    pml4: u64,
    nx_supported: bool,
}

impl PageTables {
    pub fn new(nx_supported: bool) -> Result<Self, Error> {
        Ok(Self { pml4: alloc_table()?, nx_supported })
    }

    /// Physical address of the PML4, for CR3.
    pub fn root(&self) -> u64 {
        self.pml4
    }

    /// Maps one 4 KiB page.
    pub fn map_page(&mut self, virt: u64, phys: u64, flags: u64) -> Result<(), Error> {
        let pdpt = self.next_table(self.pml4, index(virt, 39))?;
        let pd = self.next_table(pdpt, index(virt, 30))?;
        let pt = self.next_table(pd, index(virt, 21))?;
        self.set_leaf(pt, index(virt, 12), phys | flags)
    }

    /// Maps one 2 MiB page.
    fn map_huge_page(&mut self, virt: u64, phys: u64, flags: u64) -> Result<(), Error> {
        let pdpt = self.next_table(self.pml4, index(virt, 39))?;
        let pd = self.next_table(pdpt, index(virt, 30))?;
        self.set_leaf(pd, index(virt, 21), phys | flags | HUGE_PAGE)
    }

    /// Maps physical memory `0..end` twice: at its own address, so eliboot
    /// keeps running after it switches page tables, and at [`HHDM_OFFSET`],
    /// where the kernel will use it.
    pub fn map_physical_memory(&mut self, end: u64) -> Result<(), Error> {
        let end = end.next_multiple_of(HUGE_PAGE_SIZE);
        for phys in (0..end).step_by(HUGE_PAGE_SIZE as usize) {
            self.map_huge_page(phys, phys, WRITABLE)?;
            self.map_huge_page(HHDM_OFFSET + phys, phys, WRITABLE | NO_EXECUTE)?;
        }
        Ok(())
    }

    fn set_leaf(&mut self, table: u64, index: usize, entry: u64) -> Result<(), Error> {
        let slot = &mut table_at(table)[index];
        if *slot & PRESENT != 0 {
            return Err(Error::new("page tables: address mapped twice"));
        }
        let entry = if self.nx_supported { entry } else { entry & !NO_EXECUTE };
        *slot = entry | PRESENT;
        Ok(())
    }

    /// Returns the table that entry `index` of `table` points to, creating it
    /// if needed. Permissions are only restricted at the leaf level.
    fn next_table(&mut self, table: u64, index: usize) -> Result<u64, Error> {
        let slot = &mut table_at(table)[index];
        if *slot & PRESENT == 0 {
            let next = alloc_table()?;
            *slot = next | PRESENT | WRITABLE;
            return Ok(next);
        }
        if *slot & HUGE_PAGE != 0 {
            return Err(Error::new("page tables: mapping overlaps a huge page"));
        }
        Ok(*slot & ADDRESS_MASK)
    }
}

fn alloc_table() -> Result<u64, Error> {
    memory::allocate_zeroed_pages(ELIOS_PAGE_TABLES, 1)
}

fn table_at(phys: u64) -> &'static mut Table {
    // SAFETY: `phys` is always a table we allocated, which the firmware's
    // identity map makes addressable.
    unsafe { &mut *(phys as *mut Table) }
}

fn index(virt: u64, shift: u32) -> usize {
    ((virt >> shift) & 0x1FF) as usize
}

const _: () = assert!(PAGE_SIZE as usize == size_of::<Table>());
