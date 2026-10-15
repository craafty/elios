//! Memory management: physical frames, page tables, and the kernel's address
//! space.
//!
//! # Kernel address space
//!
//! The lower half belongs to user programs (from Phase 5). The upper half
//! is the same in every address space:
//!
//! | Start                    | Region                                      |
//! |--------------------------|---------------------------------------------|
//! | `0xFFFF_8000_0000_0000`  | Direct map of all RAM (non-executable)      |
//! | `0xFFFF_C000_0000_0000`  | Kernel heap                                 |
//! | `0xFFFF_D000_0000_0000`  | Device memory (framebuffer, APIC registers) |
//! | `0xFFFF_E000_0000_0000`  | Kernel stacks, each with a guard page below |
//! | `0xFFFF_FFFF_8000_0000`  | Kernel image                                |
//!
//! The direct map is at the same address the bootloader used, so pointers
//! made before the switch to the kernel's own page tables stay valid.

pub mod frames;
pub mod heap;
pub mod object;
pub mod paging;

use core::sync::atomic::{AtomicU64, Ordering};

use bootinfo::{BootInfo, MemoryKind};

use self::paging::{GLOBAL, NO_EXECUTE, PageSize, PageTable, WRITABLE};
use crate::arch;
use crate::sync::SpinLock;

pub const PAGE_SIZE: u64 = 4096;

pub const DIRECT_MAP_START: u64 = bootinfo::HHDM_OFFSET;
pub const HEAP_START: u64 = 0xFFFF_C000_0000_0000;
pub const HEAP_END: u64 = HEAP_START + REGION_SIZE;
pub const DEVICE_START: u64 = 0xFFFF_D000_0000_0000;
pub const DEVICE_END: u64 = DEVICE_START + REGION_SIZE;
pub const STACKS_START: u64 = 0xFFFF_E000_0000_0000;
pub const STACKS_END: u64 = STACKS_START + REGION_SIZE;
/// One top-level page table entry's worth.
const REGION_SIZE: u64 = 512 << 30;

/// The start of the higher half; everything from here up is kernel space.
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

static DIRECT_MAP_OFFSET: AtomicU64 = AtomicU64::new(0);
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);
static KERNEL_PAGE_TABLE: SpinLock<Option<PageTable>> = SpinLock::new(None);
static NEXT_DEVICE: AtomicU64 = AtomicU64::new(DEVICE_START);
static NEXT_STACK: AtomicU64 = AtomicU64::new(STACKS_START);

unsafe extern "C" {
    // Section boundaries from linker.ld; only their addresses matter.
    static __text_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// The virtual address at which physical address `phys` is mapped.
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    (DIRECT_MAP_OFFSET.load(Ordering::Relaxed) + phys) as *mut u8
}

/// Sets up the frame allocator, builds the kernel's own page tables and
/// switches to them. Afterwards the bootloader's identity map is gone.
pub fn init(boot_info: &BootInfo) {
    DIRECT_MAP_OFFSET.store(boot_info.hhdm_offset, Ordering::Relaxed);
    frames::init(boot_info.memory_regions());
    arch::init_page_attributes();
    arch::enable_global_pages();

    let mut table = PageTable::new().expect("out of memory building the kernel page tables");
    // Create every kernel top-level entry now, so address spaces made later
    // can share the whole kernel half by copying those entries.
    for virt in (KERNEL_HALF..=u64::MAX - (REGION_SIZE - 1)).step_by(REGION_SIZE as usize) {
        table.reserve_top_level(virt).expect("out of memory building the kernel page tables");
    }
    map_kernel_image(&mut table);
    map_direct(&mut table, boot_info);

    // SAFETY: the new tables map the kernel image, the stack (direct map)
    // and every structure in use, all at their current addresses.
    unsafe { arch::set_page_table_root(table.root()) };
    KERNEL_ROOT.store(table.root(), Ordering::Relaxed);
    *KERNEL_PAGE_TABLE.lock() = Some(table);
}

/// Maps each part of the kernel with only the access it needs: code is
/// read-only and executable, everything else non-executable.
fn map_kernel_image(table: &mut PageTable) {
    let (text, rodata, data, end) =
        (&raw const __text_start, &raw const __rodata_start, &raw const __data_start, &raw const __kernel_end);
    let sections =
        [(text, rodata, GLOBAL), (rodata, data, GLOBAL | NO_EXECUTE), (data, end, GLOBAL | NO_EXECUTE | WRITABLE)];

    // The kernel's physical pages are wherever the bootloader put them.
    // SAFETY: CR3 holds the bootloader's tables, which stay intact until
    // reclaimed after the switch.
    let bootloader_tables = unsafe { PageTable::from_root(arch::page_table_root()) };
    for (start, end, flags) in sections {
        for page in (start as u64..end as u64).step_by(PAGE_SIZE as usize) {
            let phys = bootloader_tables.translate_address(page).expect("kernel page not mapped by the bootloader");
            table.map(page, phys, PageSize::Small, flags).expect("mapping the kernel image failed");
        }
    }
}

/// Maps all RAM (and the first megabyte, where firmware tables can hide) into
/// the direct map. Device memory is left out; it's mapped on request with the
/// right cache settings by [`map_device`].
fn map_direct(table: &mut PageTable, boot_info: &BootInfo) {
    let mut map_run = |start: u64, end: u64| {
        table
            .map_range(DIRECT_MAP_START + start, start, end - start, WRITABLE | NO_EXECUTE | GLOBAL)
            .expect("building the direct map failed");
    };

    // Regions are sorted; merge touching ones so large pages can span them.
    let mut run = (0, 1 << 20);
    let ram = boot_info.memory_regions().iter().filter(|r| {
        !matches!(
            r.kind,
            MemoryKind::Mmio | MemoryKind::FirmwareRuntimeMmio | MemoryKind::Reserved | MemoryKind::Defective
        )
    });
    for region in ram {
        if region.start <= run.1 {
            run.1 = run.1.max(region.end());
        } else {
            map_run(run.0, run.1);
            run = (region.start, region.end());
        }
    }
    map_run(run.0, run.1);
}

fn with_kernel_table<R>(f: impl FnOnce(&mut PageTable) -> R) -> R {
    f(KERNEL_PAGE_TABLE.lock().as_mut().expect("kernel page table used before memory::init"))
}

/// How the CPU may cache a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheType {
    /// Writes are batched and reads are uncached: right for framebuffers.
    WriteCombining,
    /// Every access goes straight to the device: right for registers.
    Uncached,
}

impl CacheType {
    /// Page table bits selecting this type (see `arch::init_page_attributes`).
    pub fn flags(self) -> u64 {
        match self {
            CacheType::WriteCombining => paging::WRITE_THROUGH,
            CacheType::Uncached => paging::WRITE_THROUGH | paging::CACHE_DISABLE,
        }
    }
}

/// Takes `len` bytes of virtual address space from a bump-allocated region.
fn reserve(next: &AtomicU64, len: u64, align: u64, region_end: u64) -> u64 {
    let mut start = 0;
    next.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        start = current.next_multiple_of(align);
        Some(start + len)
    })
    .unwrap();
    assert!(start + len <= region_end, "kernel address space region exhausted");
    start
}

/// Maps device memory (registers, framebuffers) into kernel space and
/// returns its virtual address.
pub fn map_device(phys: u64, len: u64, cache: CacheType) -> *mut u8 {
    let offset = phys % PAGE_SIZE;
    let base = phys - offset;
    let len = (offset + len).next_multiple_of(PAGE_SIZE);
    // Big mappings get matching virtual alignment so large pages can be used.
    let large = PageSize::Large.bytes();
    let align = if len >= large && base.is_multiple_of(large) { large } else { PAGE_SIZE };
    let virt = reserve(&NEXT_DEVICE, len, align, DEVICE_END);
    with_kernel_table(|table| table.map_range(virt, base, len, WRITABLE | NO_EXECUTE | GLOBAL | cache.flags()))
        .expect("mapping device memory failed");
    (virt + offset) as *mut u8
}

/// A kernel stack with an unmapped guard page below it, so overflowing it
/// faults instead of corrupting whatever lies below.
pub struct KernelStack {
    pub top: u64,
    pages: u64,
}

pub fn allocate_kernel_stack(pages: u64) -> KernelStack {
    let slot = reserve(&NEXT_STACK, (pages + 1) * PAGE_SIZE, PAGE_SIZE, STACKS_END);
    let bottom = slot + PAGE_SIZE;
    with_kernel_table(|table| {
        for page in 0..pages {
            let frame = frames::allocate().expect("out of memory for a kernel stack");
            table
                .map(bottom + page * PAGE_SIZE, frame, PageSize::Small, WRITABLE | NO_EXECUTE | GLOBAL)
                .expect("mapping a kernel stack failed");
        }
    });
    KernelStack { top: bottom + pages * PAGE_SIZE, pages }
}

/// Unmaps a stack and frees its memory. Its address range isn't reused, so
/// stray pointers into it fault rather than hitting a newer stack.
pub fn free_kernel_stack(stack: KernelStack) {
    let bottom = stack.top - stack.pages * PAGE_SIZE;
    // Allocated first: the heap may need the page table lock to grow.
    let mut freed = alloc::vec::Vec::with_capacity(stack.pages as usize);
    with_kernel_table(|table| {
        for page in 0..stack.pages {
            freed.push(table.unmap(bottom + page * PAGE_SIZE).expect("kernel stack page not mapped").phys);
        }
    });
    // Other CPUs may still have the pages' translations cached.
    arch::smp::flush_tlb(bottom, stack.top);
    freed.into_iter().for_each(frames::free);
}

/// Frees the memory the bootloader and firmware were using. Call once the
/// kernel no longer uses anything there: its page tables are active, it
/// runs on its own stack, and the firmware tables it needs have been read.
/// Returns the number of bytes reclaimed.
pub fn reclaim_boot_memory(boot_info: &BootInfo) -> u64 {
    boot_info
        .memory_regions()
        .iter()
        .filter(|r| {
            matches!(r.kind, MemoryKind::BootloaderReclaimable | MemoryKind::PageTables | MemoryKind::KernelStack)
        })
        .map(|r| frames::reclaim(r.start, r.end()))
        .sum()
}

/// What a faulting address was, if it's somewhere that explains the fault.
/// Safe to call from exception handlers.
pub fn describe_fault_address(address: u64) -> Option<&'static str> {
    if address < PAGE_SIZE {
        Some("that's the null page: a null pointer was dereferenced")
    } else if address < KERNEL_HALF {
        Some("that's in the lower half, where nothing is mapped yet")
    } else if (STACKS_START..STACKS_END).contains(&address) && is_mapped(address) == Some(false) {
        Some("that's a kernel stack's guard page: the stack overflowed")
    } else {
        None
    }
}

/// Whether `virt` is mapped in kernel space, or `None` if the page table is
/// busy (as it may be when an exception interrupts code editing it).
pub fn is_mapped(virt: u64) -> Option<bool> {
    let guard = KERNEL_PAGE_TABLE.try_lock()?;
    Some(guard.as_ref()?.translate(virt).is_some())
}

/// What the kernel page table maps at `virt`.
#[cfg(test)]
pub fn translate(virt: u64) -> Option<paging::Mapping> {
    with_kernel_table(|table| table.translate(virt))
}

/// Physical address of the kernel's PML4. Its upper half is shared by every
/// address space.
pub fn kernel_page_table_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::boot_info;

    fn address_of(symbol: *const u8) -> u64 {
        symbol as u64
    }

    crate::ktest! {
        fn running_on_our_own_page_tables() {
            assert_eq!(arch::page_table_root(), kernel_page_table_root());
            assert_ne!(arch::page_table_root(), boot_info().page_table_root);
        }

        fn the_identity_map_and_null_page_are_gone() {
            assert_eq!(translate(0), None);
            assert_eq!(translate(0x20_0000), None);
            assert_eq!(translate(boot_info().page_table_root), None);
        }

        fn kernel_sections_have_the_right_permissions() {
            let text = translate(address_of(&raw const __text_start)).unwrap();
            assert!(text.flags & WRITABLE == 0 && text.flags & NO_EXECUTE == 0);
            let rodata = translate(address_of(&raw const __rodata_start)).unwrap();
            assert!(rodata.flags & WRITABLE == 0 && rodata.flags & NO_EXECUTE != 0);
            let data = translate(address_of(&raw const __data_start)).unwrap();
            assert!(data.flags & WRITABLE != 0 && data.flags & NO_EXECUTE != 0);
        }

        fn the_direct_map_covers_free_memory() {
            let frame = frames::allocate().unwrap();
            assert_eq!(translate(DIRECT_MAP_START + frame).map(|m| m.flags & NO_EXECUTE != 0), Some(true));
            frames::free(frame);
        }

        fn running_on_a_guarded_stack() {
            let rsp = arch::stack_pointer();
            assert!((STACKS_START..STACKS_END).contains(&rsp));
            // Walk down to the bottom of the stack: the next page must be the
            // unmapped guard.
            let mut page = rsp & !(PAGE_SIZE - 1);
            while translate(page - PAGE_SIZE).is_some() {
                page -= PAGE_SIZE;
            }
            assert!(rsp - page < 64 * 1024, "stack is bigger than expected");
            assert!(describe_fault_address(page - 8).unwrap().contains("guard page"));
        }

        fn device_mappings_alias_physical_memory() {
            let frame = frames::allocate_zeroed().unwrap();
            let device = map_device(frame + 0x10, 8, CacheType::Uncached);
            // SAFETY: both pointers refer to the frame we own.
            unsafe {
                device.cast::<u64>().write_volatile(0x0123_4567_89AB_CDEF);
                assert_eq!(phys_to_virt(frame + 0x10).cast::<u64>().read(), 0x0123_4567_89AB_CDEF);
            }
            let mapping = translate(device as u64).unwrap();
            assert_ne!(mapping.flags & paging::CACHE_DISABLE, 0);
        }

        fn boot_memory_was_reclaimed() {
            let reclaimable: u64 = boot_info()
                .memory_regions()
                .iter()
                .filter(|r| r.kind == MemoryKind::BootloaderReclaimable)
                .map(|r| r.len)
                .sum();
            let usable: u64 =
                boot_info().memory_regions().iter().filter(|r| r.kind == MemoryKind::Usable).map(|r| r.len).sum();
            // Everything reclaimable is now managed, give or take the first
            // megabyte, which is never handed out.
            assert!(frames::stats().managed_bytes + (1 << 20) >= usable + reclaimable);
        }
    }
}
