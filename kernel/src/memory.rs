//! Physical memory access. For now this is just the bootloader's direct map;
//! the kernel takes over memory management in Phase 3.

use core::sync::atomic::{AtomicU64, Ordering};

use bootinfo::BootInfo;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn init(boot_info: &BootInfo) {
    HHDM_OFFSET.store(boot_info.hhdm_offset, Ordering::Relaxed);
}

/// The virtual address at which physical address `phys` is mapped.
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    (HHDM_OFFSET.load(Ordering::Relaxed) + phys) as *mut u8
}
