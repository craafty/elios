//! User address spaces.
//!
//! An address space is a set of areas (ranges of virtual memory with access
//! rights and something behind them) and the page tables mapping them.
//! Pages are mapped on first touch, so reserving a large area (a stack, or
//! an `mmap`) costs nothing up front. What a touched page gets depends on
//! the area's backing:
//!
//! - **Private memory** gets a fresh zeroed page.
//! - **A memory object** (a file's data or page cache, a memfd, shared
//!   anonymous memory) gets the object's page. In a shared mapping the
//!   page is mapped as it is, so everyone mapping the object sees the
//!   same bytes; in a private one it's mapped read-only and copied on the
//!   first write ("copy-on-write"), so the program gets its own copy.
//! - **Device memory** (a framebuffer) gets the device's page.
//!
//! `fork` shares every page instead of copying: private pages become
//! read-only in both processes and are copied by whichever writes first.
//! A frame's owner count (see `memory::frames`) says when a copy-on-write
//! page has become the last owner's alone and needn't be copied.
//!
//! Getting an object's page may mean reading it from a disk, which can't
//! happen with the address space's (spin)lock held, so a fault that needs
//! one returns [`Fault::NeedsPage`]; [`resolve_fault`] fetches the page
//! with the lock released, then installs it.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::smp;
use crate::errno::{Errno, Result};
use crate::memory::object::MemoryObject;
use crate::memory::paging::{COPY_ON_WRITE, DEVICE_PAGE, NO_EXECUTE, PageSize, PageTable, USER, WRITABLE};
use crate::memory::{self, PAGE_SIZE, frames, phys_to_virt};
use crate::sync::SpinLock;

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

/// What's behind an area.
#[derive(Clone)]
pub enum Backing {
    /// Private memory: zero-filled pages of the process's own.
    Anonymous,
    /// A memory object's pages, starting at byte `offset` of the object.
    Object { object: Arc<MemoryObject>, offset: u64, shared: bool },
    /// Device memory from physical address `phys`, with page table cache
    /// bits `cache`.
    Device { phys: u64, cache: u64 },
}

#[derive(Clone)]
struct Area {
    end: u64,
    protection: u32,
    backing: Backing,
    /// What's mapped, for /proc/PID/maps and fault reports: a file's path,
    /// or a label like `[stack]`.
    name: Option<Arc<str>>,
    /// For a program image the kernel loaded: what its addresses are
    /// relative to (0 for one that isn't position-independent).
    image_base: Option<u64>,
}

/// An area, as /proc/PID/maps describes it.
pub struct AreaInfo {
    pub start: u64,
    pub end: u64,
    pub protection: u32,
    pub shared: bool,
    /// Where in the file (or memory object) the area starts.
    pub offset: u64,
    pub name: Option<Arc<str>>,
}

impl Area {
    /// Whether pages are shared with others when written (so they're never
    /// copied on write).
    fn shared(&self) -> bool {
        matches!(self.backing, Backing::Object { shared: true, .. } | Backing::Device { .. })
    }
}

/// The outcome of a page fault.
pub enum Fault {
    Resolved,
    /// The access isn't allowed (or memory ran out): a segmentation fault.
    Invalid,
    /// Page `index` of the object is needed; get it with the lock released
    /// and call [`AddressSpace::install_page`].
    NeedsPage {
        object: Arc<MemoryObject>,
        index: u64,
    },
}

pub struct AddressSpace {
    table: PageTable,
    /// Areas by start address; they never overlap.
    areas: BTreeMap<u64, Area>,
    /// The heap grown by `brk` runs from `brk_start` to `brk`.
    pub brk_start: u64,
    pub brk: u64,
    /// Pages currently mapped.
    resident_pages: u64,
    /// Where scratch pages (`map_scratch_page`) are mapped.
    scratch_pages: Vec<u64>,
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

/// Copies a frame into a fresh one.
fn copy_frame(source: u64) -> Result<u64> {
    let frame = frames::allocate().ok_or(Errno::ENOMEM)?;
    // SAFETY: both are RAM in the direct map; the new one is ours alone and
    // we hold a reference to the source.
    unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(source), phys_to_virt(frame), PAGE_SIZE as usize) };
    Ok(frame)
}

impl AddressSpace {
    /// An address space with nothing in the user half.
    pub fn new() -> Result<Self> {
        let mut table = PageTable::new().map_err(|_| Errno::ENOMEM)?;
        table.share_kernel_half(memory::kernel_page_table_root());
        Ok(Self { table, areas: BTreeMap::new(), brk_start: 0, brk: 0, resident_pages: 0, scratch_pages: Vec::new() })
    }

    /// Physical address of the page tables, for CR3.
    pub fn root(&self) -> u64 {
        self.table.root()
    }

    /// The areas, in address order.
    pub fn areas(&self) -> Vec<AreaInfo> {
        self.areas
            .iter()
            .map(|(&start, area)| AreaInfo {
                start,
                end: area.end,
                protection: area.protection,
                shared: area.shared(),
                offset: match area.backing {
                    Backing::Object { offset, .. } => offset,
                    _ => 0,
                },
                name: area.name.clone(),
            })
            .collect()
    }

    /// Names the areas that have no name yet (after loading a program
    /// image, its areas are the unnamed ones), loaded at `base`.
    pub fn name_unnamed(&mut self, name: &str, base: u64) {
        let name: Arc<str> = name.into();
        for area in self.areas.values_mut().filter(|area| area.name.is_none()) {
            area.name = Some(name.clone());
            area.image_base = Some(base);
        }
    }

    /// Names the area containing `address`.
    pub fn set_name(&mut self, address: u64, name: Arc<str>) {
        if let Some((start, _)) = self.area_at(address) {
            self.areas.get_mut(&start).unwrap().name = Some(name);
        }
    }

    /// Reads a word of user memory if its page is mapped (without
    /// faulting anything in), for debugging views like /proc/PID/stacks.
    pub fn peek(&self, address: u64) -> Option<u64> {
        if !address.is_multiple_of(8) {
            return None;
        }
        let mapping = self.table.translate(address & !(PAGE_SIZE - 1)).filter(|m| m.flags & DEVICE_PAGE == 0)?;
        // SAFETY: the frame is RAM (not device memory), in the direct map,
        // and referenced by this address space, which the caller holds.
        Some(unsafe { core::ptr::read_volatile(phys_to_virt(mapping.phys + address % PAGE_SIZE) as *const u64) })
    }

    /// The memory object and offset behind `address`, if it's in shared
    /// memory (which other processes may map too): futexes there are
    /// identified by it rather than by address.
    pub fn shared_object_at(&self, address: u64) -> Option<(Arc<MemoryObject>, u64)> {
        let (start, area) = self.area_at(address)?;
        match &area.backing {
            Backing::Object { object, offset, shared: true } => Some((object.clone(), offset + (address - start))),
            _ => None,
        }
    }

    /// Whether `address` is in an executable area.
    pub fn executable(&self, address: u64) -> bool {
        self.area_at(address).is_some_and(|(_, area)| area.protection & PROT_EXEC != 0)
    }

    /// Describes an address for a fault report: the name of what's mapped
    /// there and the offset from where that is mapped first (for a library,
    /// its load address, which is what `addr2line` wants).
    pub fn describe(&self, address: u64) -> Option<(Arc<str>, u64)> {
        let (_, area) = self.area_at(address)?;
        let name = area.name.clone()?;
        if let Some(base) = area.image_base {
            return Some((name, address - base));
        }
        let base = self.areas.iter().find(|(_, other)| other.name.as_ref() == Some(&name)).map(|(&start, other)| {
            start
                - match other.backing {
                    Backing::Object { offset, .. } => offset,
                    _ => 0,
                }
        })?;
        Some((name, address.wrapping_sub(base)))
    }

    pub fn resident_bytes(&self) -> u64 {
        self.resident_pages * PAGE_SIZE
    }

    fn area_at(&self, address: u64) -> Option<(u64, &Area)> {
        let (&start, area) = self.areas.range(..=address).next_back()?;
        (address < area.end).then_some((start, area))
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.areas.range(..end).next_back().is_some_and(|(_, area)| area.end > start)
    }

    /// Adds an area of private memory. Fails if it overlaps an existing one.
    pub fn add_area(&mut self, start: u64, end: u64, protection: u32) -> Result<()> {
        self.add_backed_area(start, end, protection, Backing::Anonymous)
    }

    /// Adds an area with the given backing.
    pub fn add_backed_area(&mut self, start: u64, end: u64, protection: u32, backing: Backing) -> Result<()> {
        self.add_named_area(start, end, protection, backing, None)
    }

    /// Adds an area with the given backing and name.
    pub fn add_named_area(
        &mut self,
        start: u64,
        end: u64,
        protection: u32,
        backing: Backing,
        name: Option<Arc<str>>,
    ) -> Result<()> {
        if !start.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) || start >= end || end > USER_END {
            return Err(Errno::EINVAL);
        }
        if self.overlaps(start, end) {
            return Err(Errno::EEXIST);
        }
        self.drop_scratch_pages(start, end);
        self.areas.insert(start, Area { end, protection, backing, name, image_base: None });
        Ok(())
    }

    /// Unmaps a page, dropping this address space's reference to it.
    /// Returns the frame to free (after the TLB is flushed), if it's one.
    fn unmap_page(&mut self, page: u64) -> Option<u64> {
        let mapping = self.table.unmap(page)?;
        self.resident_pages -= 1;
        (mapping.flags & DEVICE_PAGE == 0).then_some(mapping.phys)
    }

    /// Removes `start..end` from whatever areas cover it, splitting them as
    /// needed, and unmaps the pages.
    pub fn remove(&mut self, start: u64, end: u64) {
        self.split_at(start);
        self.split_at(end);
        let doomed: Vec<u64> = self.areas.range(start..end).map(|(&s, _)| s).collect();
        let mut freed = Vec::new();
        for area_start in doomed {
            let area = self.areas.remove(&area_start).unwrap();
            for page in (area_start..area.end).step_by(PAGE_SIZE as usize) {
                freed.extend(self.unmap_page(page));
            }
        }
        // Other threads of the program may be running on other CPUs: the
        // frames can only go once no CPU can reach them.
        smp::flush_tlb(start, end);
        freed.into_iter().for_each(frames::free);
    }

    /// Whether every page of `start..end` belongs to some area.
    pub fn covers(&self, start: u64, end: u64) -> bool {
        let mut address = start;
        while address < end {
            match self.area_at(address) {
                Some((_, area)) => address = area.end,
                None => return false,
            }
        }
        true
    }

    /// `madvise(MADV_DONTNEED)`: drops the pages of `start..end`, keeping
    /// the areas. The next access gets a fresh page: zeros for private
    /// memory, the file's contents for a file mapping (shared memory keeps
    /// its contents, which live in its object). Allocators rely on this to
    /// hand back memory that reads as zero. Fails if part of the range
    /// isn't mapped.
    pub fn discard(&mut self, start: u64, end: u64) -> Result<()> {
        let mut address = start;
        while address < end {
            let (_, area) = self.area_at(address).ok_or(Errno::ENOMEM)?;
            address = area.end;
        }
        let mut freed = Vec::new();
        let mut page = start;
        while page < end {
            let (_, area) = self.area_at(page).expect("checked above");
            let area_end = area.end.min(end);
            if !matches!(area.backing, Backing::Device { .. }) {
                for page in (page..area_end).step_by(PAGE_SIZE as usize) {
                    freed.extend(self.unmap_page(page));
                }
            }
            page = area_end;
        }
        smp::flush_tlb(start, end);
        freed.into_iter().for_each(frames::free);
        Ok(())
    }

    /// Changes the access rights of `start..end`, which must be covered by
    /// areas. Pages that are copy-on-write stay read-only until written.
    pub fn protect(&mut self, start: u64, end: u64, protection: u32) -> Result<()> {
        let mut address = start;
        while address < end {
            let (_, area) = self.area_at(address).ok_or(Errno::ENOMEM)?;
            address = area.end;
        }
        self.split_at(start);
        self.split_at(end);
        self.drop_scratch_pages(start, end);
        let starts: Vec<u64> = self.areas.range(start..end).map(|(&s, _)| s).collect();
        for area_start in starts {
            let area = self.areas.get_mut(&area_start).unwrap();
            area.protection = protection;
            let area_end = area.end;
            for page in (area_start..area_end).step_by(PAGE_SIZE as usize) {
                if let Some(mapping) = self.table.translate(page) {
                    let kept = mapping.flags & (COPY_ON_WRITE | DEVICE_PAGE | crate::memory::paging::WRITE_THROUGH);
                    let mut flags = page_flags(protection) | kept;
                    if kept & COPY_ON_WRITE != 0 {
                        flags &= !WRITABLE;
                    }
                    self.table.set_flags(page, flags);
                }
            }
        }
        smp::flush_tlb(start, end);
        Ok(())
    }

    /// Splits the area containing `address` so an area boundary falls there.
    fn split_at(&mut self, address: u64) {
        let Some((start, area)) = self.area_at(address) else { return };
        if start == address {
            return;
        }
        let mut upper = area.clone();
        let distance = address - start;
        match &mut upper.backing {
            Backing::Anonymous => {}
            Backing::Object { offset, .. } => *offset += distance,
            Backing::Device { phys, .. } => *phys += distance,
        }
        self.areas.get_mut(&start).unwrap().end = address;
        self.areas.insert(address, upper);
    }

    /// `mremap`: resizes the mapping at `start..start + old_len` (which
    /// must be within one area) to `new_len`, in place if there's room, or
    /// (if `may_move`) by moving it, pages and all. Returns where it is.
    pub fn remap(&mut self, start: u64, old_len: u64, new_len: u64, may_move: bool) -> Result<u64> {
        let old_end = start + old_len;
        match self.area_at(start) {
            Some((_, area)) if area.end >= old_end => {}
            _ => return Err(Errno::EFAULT),
        }
        if new_len <= old_len {
            if new_len < old_len {
                self.remove(start + new_len, old_end);
            }
            return Ok(start);
        }
        self.split_at(start);
        self.split_at(old_end);
        let new_end = start.checked_add(new_len).filter(|&end| end <= USER_END).ok_or(Errno::ENOMEM)?;
        if !self.overlaps(old_end, new_end) {
            self.drop_scratch_pages(old_end, new_end);
            self.areas.get_mut(&start).expect("split above").end = new_end;
            return Ok(start);
        }
        if !may_move {
            return Err(Errno::ENOMEM);
        }
        let target = self.find_free(new_len).ok_or(Errno::ENOMEM)?;
        self.drop_scratch_pages(target, target + new_len);
        let mut area = self.areas.remove(&start).expect("split above");
        area.end = target + new_len;
        self.areas.insert(target, area);
        for offset in (0..old_len).step_by(PAGE_SIZE as usize) {
            if let Some(mapping) = self.table.unmap(start + offset) {
                let flags = mapping.flags & (USER | WRITABLE | NO_EXECUTE | COPY_ON_WRITE | DEVICE_PAGE)
                    | mapping.flags & crate::memory::paging::WRITE_THROUGH;
                if self.table.map(target + offset, mapping.phys, PageSize::Small, flags).is_err() {
                    if flags & DEVICE_PAGE == 0 {
                        frames::free(mapping.phys);
                    }
                    self.resident_pages -= 1;
                }
            }
        }
        smp::flush_tlb(start, old_end);
        Ok(target)
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

    /// Handles a page fault at `address` (or prepares a page the kernel is
    /// about to access for the program).
    pub fn handle_fault(&mut self, address: u64, write: bool, execute: bool) -> Fault {
        let Some((start, area)) = self.area_at(address) else { return Fault::Invalid };
        let allowed = if write {
            area.protection & PROT_WRITE != 0
        } else if execute {
            area.protection & PROT_EXEC != 0
        } else {
            area.protection != 0
        };
        if !allowed {
            return Fault::Invalid;
        }
        let page = address & !(PAGE_SIZE - 1);
        let protection = area.protection;
        let backing = area.backing.clone();
        let existing = match self.table.translate(page) {
            // A scratch page (`map_scratch_page`) a system call still
            // holds, where the program has since mapped memory again: the
            // program's memory takes the address; the system call's
            // cleanup sees the frame isn't its own any more.
            Some(mapping) if mapping.flags & USER == 0 => {
                self.drop_scratch_pages(page, page + PAGE_SIZE);
                None
            }
            existing => existing,
        };
        if let Some(mapping) = existing {
            if write && mapping.flags & COPY_ON_WRITE != 0 {
                return match self.break_copy_on_write(page, mapping.phys, mapping.flags, protection) {
                    Ok(()) => Fault::Resolved,
                    Err(_) => Fault::Invalid,
                };
            }
            // The page exists. If its entry allows the access, this CPU had
            // an outdated translation cached (another CPU just mapped or
            // unprotected it): drop it and retry. Otherwise it's a genuine
            // protection violation.
            let permits = (!write || mapping.flags & WRITABLE != 0) && (!execute || mapping.flags & NO_EXECUTE == 0);
            if permits {
                crate::arch::flush_tlb_page(page);
            }
            return if permits { Fault::Resolved } else { Fault::Invalid };
        }
        match backing {
            Backing::Anonymous => match self.map_fresh_page(page, protection) {
                Ok(_) => Fault::Resolved,
                Err(_) => Fault::Invalid,
            },
            Backing::Object { object, offset, .. } => {
                Fault::NeedsPage { object, index: (offset + page - start) / PAGE_SIZE }
            }
            Backing::Device { phys, cache } => {
                let flags = page_flags(protection) | cache | DEVICE_PAGE;
                if self.table.map(page, phys + page - start, PageSize::Small, flags).is_err() {
                    return Fault::Invalid;
                }
                self.resident_pages += 1;
                Fault::Resolved
            }
        }
    }

    /// Maps `frame` (page `index` of `object`, with a reference for us) at
    /// `address`, if that's still what belongs there; in a private area it
    /// is copied first if `write`, and otherwise mapped copy-on-write.
    /// Drops the reference if the page isn't needed after all.
    pub fn install_page(&mut self, address: u64, object: &Arc<MemoryObject>, index: u64, frame: u64, write: bool) {
        let page = address & !(PAGE_SIZE - 1);
        let wanted = self.area_at(page).and_then(|(start, area)| match &area.backing {
            Backing::Object { object: mine, offset, shared }
                if Arc::ptr_eq(mine, object) && (offset + page - start) / PAGE_SIZE == index =>
            {
                Some((area.protection, *shared))
            }
            _ => None,
        });
        let Some((protection, shared)) = wanted else {
            frames::free(frame);
            return;
        };
        if self.table.translate(page).is_some() {
            // Another thread got here first.
            frames::free(frame);
            return;
        }
        let (frame, flags) = if shared {
            (frame, page_flags(protection))
        } else if write {
            match copy_frame(frame) {
                Ok(copy) => {
                    frames::free(frame);
                    (copy, page_flags(protection))
                }
                Err(_) => {
                    frames::free(frame);
                    return;
                }
            }
        } else {
            (frame, page_flags(protection) & !WRITABLE | COPY_ON_WRITE)
        };
        if self.table.map(page, frame, PageSize::Small, flags).is_err() {
            frames::free(frame);
            return;
        }
        self.resident_pages += 1;
    }

    /// Gives a copy-on-write page to this address space alone: copies it,
    /// unless nobody else has it any more.
    fn break_copy_on_write(&mut self, page: u64, frame: u64, flags: u64, protection: u32) -> Result<()> {
        let kept = flags & crate::memory::paging::WRITE_THROUGH;
        if frames::owners(frame) == 1 {
            self.table.set_flags(page, page_flags(protection) | kept);
        } else {
            let copy = copy_frame(frame)?;
            self.table.unmap(page);
            self.table.map(page, copy, PageSize::Small, page_flags(protection) | kept).map_err(|_| Errno::ENOMEM)?;
            frames::free(frame);
        }
        // Other threads may have the read-only entry cached.
        smp::flush_tlb(page, page + PAGE_SIZE);
        Ok(())
    }

    /// Maps `frame` at `page`, where nothing is mapped, for the kernel
    /// alone (no `USER` bit, so the program still faults there). Used when
    /// another thread unmapped memory a system call was using: see
    /// [`super::handle_kernel_fault`]. Marked as a device page so nothing
    /// else frees the frame.
    pub fn map_scratch_page(&mut self, page: u64, frame: u64) -> bool {
        if self.table.translate(page).is_some() {
            return false;
        }
        if self.table.map(page, frame, PageSize::Small, WRITABLE | NO_EXECUTE | DEVICE_PAGE).is_err() {
            return false;
        }
        self.resident_pages += 1;
        self.scratch_pages.push(page);
        true
    }

    /// Removes a scratch page, if `frame` is still what's mapped at `page`
    /// (an `munmap` covering it may have gone first). The caller flushes
    /// the TLB.
    pub fn unmap_scratch_page(&mut self, page: u64, frame: u64) {
        let Some(index) = self.scratch_pages.iter().position(|&p| p == page) else { return };
        self.scratch_pages.swap_remove(index);
        if self.table.translate(page).is_some_and(|mapping| mapping.phys == frame && mapping.flags & USER == 0) {
            self.table.unmap(page);
            self.resident_pages -= 1;
        }
    }

    /// Removes the scratch pages between `start` and `end`, where the
    /// program's own memory is changing (it maps or reprotects it): what
    /// the program sees there must be its memory, never a scratch frame.
    /// The system call that had them still fails with `EFAULT` and frees
    /// its frame after a TLB flush.
    fn drop_scratch_pages(&mut self, start: u64, end: u64) {
        if self.scratch_pages.is_empty() {
            return;
        }
        let pages = core::mem::take(&mut self.scratch_pages);
        for page in pages {
            if !(start..end).contains(&page) {
                self.scratch_pages.push(page);
            } else if self.table.translate(page).is_some_and(|mapping| mapping.flags & USER == 0) {
                self.table.unmap(page);
                self.resident_pages -= 1;
            }
        }
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

    /// The frame backing `page`, ready to be written by the kernel (copied
    /// if it was copy-on-write). Ignores access rights: the kernel can
    /// write to read-only areas (to load programs, for example). Pages of
    /// objects that live on disk can't be fetched here.
    fn writable_frame(&mut self, page: u64) -> Result<u64> {
        loop {
            if let Some(mapping) = self.table.translate(page) {
                if mapping.flags & COPY_ON_WRITE == 0 {
                    return Ok(mapping.phys);
                }
                let protection = self.area_at(page).ok_or(Errno::EFAULT)?.1.protection;
                self.break_copy_on_write(page, mapping.phys, mapping.flags, protection)?;
                continue;
            }
            match self.handle_fault(page, false, false) {
                Fault::Resolved => {}
                Fault::Invalid => {
                    // Not readable: map it anyway (a program image being
                    // loaded into a read-only area).
                    let (_, area) = self.area_at(page).ok_or(Errno::EFAULT)?;
                    if !matches!(area.backing, Backing::Anonymous) {
                        return Err(Errno::EFAULT);
                    }
                    let protection = area.protection;
                    self.map_fresh_page(page, protection)?;
                }
                Fault::NeedsPage { object, index } => {
                    if object.needs_io() {
                        return Err(Errno::EFAULT);
                    }
                    let frame = object.page(index)?;
                    self.install_page(page, &object, index, frame, true);
                }
            }
        }
    }

    /// Checks that the kernel may access `address..address + len` on the
    /// program's behalf, and maps any pages not yet touched (or copies
    /// copy-on-write ones it will write), so the access can't fault. If a
    /// page must be fetched from an object first, returns that.
    pub fn prepare_access(&mut self, address: u64, len: u64, write: bool) -> Result<Option<(u64, Fault)>> {
        let end = address.checked_add(len).filter(|&end| end <= USER_END).ok_or(Errno::EFAULT)?;
        let mut page = address & !(PAGE_SIZE - 1);
        while page < end {
            let (_, area) = self.area_at(page).ok_or(Errno::EFAULT)?;
            let needed = if write { PROT_WRITE } else { PROT_READ };
            if area.protection & needed == 0 {
                return Err(Errno::EFAULT);
            }
            let ready = self.table.translate(page).is_some_and(|mapping| !write || mapping.flags & COPY_ON_WRITE == 0);
            if !ready {
                match self.handle_fault(page, write, false) {
                    Fault::Resolved => {}
                    Fault::Invalid => return Err(Errno::EFAULT),
                    fault @ Fault::NeedsPage { .. } => return Ok(Some((page, fault))),
                }
            }
            page += PAGE_SIZE;
        }
        Ok(None)
    }

    /// Copies `data` to `address`, which needn't be the active address space.
    pub fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let current = address + done as u64;
            let offset = (current % PAGE_SIZE) as usize;
            let chunk = (PAGE_SIZE as usize - offset).min(data.len() - done);
            let frame = self.writable_frame(current - offset as u64)?;
            // SAFETY: the frame belongs to this address space alone; the
            // direct map reaches it whether or not this space is active.
            unsafe {
                core::ptr::copy_nonoverlapping(data[done..].as_ptr(), phys_to_virt(frame).add(offset), chunk);
            }
            done += chunk;
        }
        Ok(())
    }

    /// A copy of this address space for `fork`, sharing every page: private
    /// ones become copy-on-write in both.
    pub fn duplicate(&mut self) -> Result<Self> {
        let mut copy = Self::new()?;
        copy.areas = self.areas.clone();
        copy.brk_start = self.brk_start;
        copy.brk = self.brk;
        let mut downgraded = false;
        for (virt, mapping) in self.table.user_pages() {
            // Scratch pages (see `map_scratch_page`) belong to a system
            // call of this process, not to the program.
            if mapping.flags & USER == 0 {
                continue;
            }
            let shared = self.area_at(virt).is_some_and(|(_, area)| area.shared());
            let mut flags = mapping.flags & (USER | WRITABLE | NO_EXECUTE | COPY_ON_WRITE | DEVICE_PAGE);
            flags |= mapping.flags & crate::memory::paging::WRITE_THROUGH;
            if !shared && flags & WRITABLE != 0 {
                flags = flags & !WRITABLE | COPY_ON_WRITE;
                self.table.set_flags(virt, flags);
                downgraded = true;
            }
            if flags & DEVICE_PAGE == 0 {
                frames::share(mapping.phys);
            }
            if copy.table.map(virt, mapping.phys, PageSize::Small, flags).is_err() {
                if flags & DEVICE_PAGE == 0 {
                    frames::free(mapping.phys);
                }
                return Err(Errno::ENOMEM);
            }
            copy.resident_pages += 1;
        }
        if downgraded {
            // Other threads of this process must stop writing to them.
            smp::flush_tlb(0, USER_END);
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
            let heap = if old_end > self.brk_start { self.area_at(old_end - 1).map(|(start, _)| start) } else { None };
            let extended = heap.is_some_and(|start| {
                let area = self.areas.get_mut(&start).unwrap();
                let extendable = area.end == old_end && matches!(area.backing, Backing::Anonymous);
                if extendable {
                    area.end = new_end;
                }
                extendable
            });
            self.drop_scratch_pages(old_end, new_end);
            if !extended
                && self
                    .add_named_area(old_end, new_end, PROT_READ | PROT_WRITE, Backing::Anonymous, Some("[heap]".into()))
                    .is_err()
            {
                return self.brk;
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
            if mapping.flags & DEVICE_PAGE == 0 {
                frames::free(mapping.phys);
            }
        }
        // SAFETY: a placeholder that's never used; the real tables are
        // consumed by free_user_tables.
        let table = core::mem::replace(&mut self.table, unsafe { PageTable::from_root(0) });
        table.free_user_tables();
    }
}

/// Resolves a page fault (or prepares an access) in the address space
/// behind `space`, fetching object pages with the lock released. Returns
/// whether the access is now possible.
pub fn resolve_fault(space: &SpinLock<Option<AddressSpace>>, address: u64, write: bool, execute: bool) -> bool {
    resolve(space, address, write, execute, true)
}

/// Like [`resolve_fault`], for a fault the kernel took on a user address.
/// Unless `may_sleep` (the kernel was running with interrupts on), pages
/// that would have to be read from a disk can't be fetched.
pub fn resolve_kernel_fault(
    space: &SpinLock<Option<AddressSpace>>,
    address: u64,
    write: bool,
    may_sleep: bool,
) -> bool {
    resolve(space, address, write, false, may_sleep)
}

fn resolve(space: &SpinLock<Option<AddressSpace>>, address: u64, write: bool, execute: bool, may_sleep: bool) -> bool {
    loop {
        let fault = match space.lock().as_mut() {
            Some(space) => space.handle_fault(address, write, execute),
            None => return false,
        };
        match fault {
            Fault::Resolved => return true,
            Fault::Invalid => return false,
            Fault::NeedsPage { object, index } => {
                let frame = if may_sleep {
                    with_interrupts(|| object.page(index))
                } else if object.needs_io() {
                    return false;
                } else {
                    object.page(index)
                };
                let Ok(frame) = frame else { return false };
                match space.lock().as_mut() {
                    Some(space) => space.install_page(address, &object, index, frame, write),
                    None => {
                        frames::free(frame);
                        return false;
                    }
                }
            }
        }
    }
}

/// Runs `f` with interrupts on (reading a page from a disk sleeps), as they
/// are during a system call; a page fault from user mode arrives with them
/// off.
pub fn with_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let enabled = crate::arch::interrupts_enabled();
    crate::arch::enable_interrupts();
    let result = f();
    if !enabled {
        crate::arch::disable_interrupts();
    }
    result
}
