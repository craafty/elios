//! Physical memory: page allocation and the firmware memory map.

use core::ptr;

use bootinfo::{MemoryKind, MemoryRegion};

use crate::uefi::{self, AllocateType, Error, MemoryDescriptor, MemoryType, Status};

pub const PAGE_SIZE: u64 = 4096;

// UEFI reserves memory types 0x8000_0000 and up for OS loaders. Everything
// eliboot hands to the kernel is tagged with one of these, so the kernel's
// memory map says exactly what each region holds.
pub const ELIOS_KERNEL: MemoryType = MemoryType(0x8000_0000);
pub const ELIOS_KERNEL_STACK: MemoryType = MemoryType(0x8000_0001);
pub const ELIOS_PAGE_TABLES: MemoryType = MemoryType(0x8000_0002);
pub const ELIOS_BOOT_INFO: MemoryType = MemoryType(0x8000_0003);

/// Allocates `count` contiguous pages and returns their physical address
/// (which is also their address in the firmware's identity mapping).
pub fn allocate_pages(ty: MemoryType, count: usize) -> Result<u64, Error> {
    let mut address = 0;
    // SAFETY: plain firmware call with a valid out-pointer.
    unsafe { (uefi::boot_services().allocate_pages)(AllocateType::ANY_PAGES, ty, count, &mut address) }
        .check("AllocatePages failed")?;
    Ok(address)
}

pub fn allocate_zeroed_pages(ty: MemoryType, count: usize) -> Result<u64, Error> {
    let address = allocate_pages(ty, count)?;
    // SAFETY: we just allocated these pages, and UEFI identity-maps memory.
    unsafe { ptr::write_bytes(address as *mut u8, 0, count * PAGE_SIZE as usize) };
    Ok(address)
}

/// A buffer holding the firmware memory map.
pub struct MemoryMap {
    buf: *mut u8,
    capacity: usize,
    size: usize,
    descriptor_size: usize,
    /// Identifies this snapshot to ExitBootServices.
    pub key: usize,
}

impl MemoryMap {
    /// Extra descriptors to make room for, since allocations made after the
    /// buffer is sized (including the buffer itself) can split regions.
    const SLACK_DESCRIPTORS: usize = 128;

    pub fn new() -> Result<Self, Error> {
        let bs = uefi::boot_services();
        let (mut size, mut key, mut descriptor_size, mut version) = (0, 0, 0, 0);
        // SAFETY: a zero-sized query only writes the out-parameters.
        let status =
            unsafe { (bs.get_memory_map)(&mut size, ptr::null_mut(), &mut key, &mut descriptor_size, &mut version) };
        if status != Status::BUFFER_TOO_SMALL {
            return Err(Error::uefi("GetMemoryMap size query failed", status));
        }
        if descriptor_size < size_of::<MemoryDescriptor>() {
            return Err(Error::new("firmware reported an impossibly small memory descriptor"));
        }

        let pages = (size + Self::SLACK_DESCRIPTORS * descriptor_size).div_ceil(PAGE_SIZE as usize);
        let buf = allocate_pages(MemoryType::LOADER_DATA, pages)? as *mut u8;
        let mut map = Self { buf, capacity: pages * PAGE_SIZE as usize, size: 0, descriptor_size, key: 0 };
        map.refresh()?;
        Ok(map)
    }

    /// Takes a fresh snapshot. Must be the last firmware call before
    /// ExitBootServices, since almost any call can change the map.
    pub fn refresh(&mut self) -> Result<(), Error> {
        let mut size = self.capacity;
        let mut version = 0;
        // SAFETY: `buf` holds `capacity` writable bytes.
        unsafe {
            (uefi::boot_services().get_memory_map)(
                &mut size,
                self.buf.cast(),
                &mut self.key,
                &mut self.descriptor_size,
                &mut version,
            )
        }
        .check("GetMemoryMap failed")?;
        self.size = size;
        Ok(())
    }

    /// The most descriptors this buffer can ever hold.
    pub fn max_descriptors(&self) -> usize {
        self.capacity / self.descriptor_size
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &MemoryDescriptor> {
        (0..self.size / self.descriptor_size).map(move |i| {
            // SAFETY: the firmware filled `size` bytes with descriptors of
            // `descriptor_size` bytes each; the buffer is page-aligned.
            unsafe { &*self.buf.add(i * self.descriptor_size).cast::<MemoryDescriptor>() }
        })
    }

    /// The end of the highest region that holds RAM or firmware data. MMIO and
    /// reserved holes are left out: they can sit far above RAM and would
    /// balloon the direct map.
    pub fn end_of_ram(&self) -> u64 {
        self.descriptors()
            .filter(|d| !matches!(d.ty, MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE | MemoryType::RESERVED))
            .map(|d| d.physical_start + d.number_of_pages * PAGE_SIZE)
            .max()
            .unwrap_or(0)
    }

    /// Converts the map into the eliOS format, sorted and merged. `out` must
    /// have room for `max_descriptors()` entries. Returns the entry count.
    pub fn to_regions(&self, out: &mut [MemoryRegion]) -> usize {
        let mut count = 0;
        for descriptor in self.descriptors() {
            out[count] = MemoryRegion {
                start: descriptor.physical_start,
                len: descriptor.number_of_pages * PAGE_SIZE,
                kind: kind_of(descriptor.ty),
                _reserved: 0,
            };
            count += 1;
        }

        let regions = &mut out[..count];
        regions.sort_unstable_by_key(|r| r.start);

        let mut merged = 0;
        for i in 0..count {
            let region = regions[i];
            if merged > 0 {
                let last = &mut regions[merged - 1];
                if last.kind == region.kind && last.end() == region.start {
                    last.len += region.len;
                    continue;
                }
            }
            regions[merged] = region;
            merged += 1;
        }
        merged
    }
}

fn kind_of(ty: MemoryType) -> MemoryKind {
    match ty {
        MemoryType::CONVENTIONAL => MemoryKind::Usable,
        MemoryType::LOADER_CODE
        | MemoryType::LOADER_DATA
        | MemoryType::BOOT_SERVICES_CODE
        | MemoryType::BOOT_SERVICES_DATA => MemoryKind::BootloaderReclaimable,
        MemoryType::RUNTIME_SERVICES_CODE | MemoryType::RUNTIME_SERVICES_DATA => MemoryKind::FirmwareRuntime,
        MemoryType::ACPI_RECLAIM => MemoryKind::AcpiReclaimable,
        MemoryType::ACPI_NVS => MemoryKind::AcpiNvs,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => MemoryKind::Mmio,
        MemoryType::UNUSABLE => MemoryKind::Defective,
        MemoryType::PERSISTENT => MemoryKind::Persistent,
        ELIOS_KERNEL => MemoryKind::Kernel,
        ELIOS_KERNEL_STACK => MemoryKind::KernelStack,
        ELIOS_PAGE_TABLES => MemoryKind::PageTables,
        ELIOS_BOOT_INFO => MemoryKind::BootInfo,
        _ => MemoryKind::Reserved,
    }
}
