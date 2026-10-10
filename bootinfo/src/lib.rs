//! The eliOS boot protocol: the contract between `eliboot` and the kernel.
//!
//! # Machine state at kernel entry
//!
//! The bootloader jumps to the kernel's ELF entry point, which must be
//! `extern "sysv64" fn(&'static BootInfo) -> !`, with:
//!
//! - 64-bit long mode, 4-level paging, interrupts disabled (`RFLAGS.IF = 0`).
//! - `EFER.NXE` set if the CPU supports it.
//! - UEFI boot services exited. Firmware runtime services are not used.
//! - `rsp` pointing at the top of a [`KERNEL_STACK_SIZE`] stack (through the
//!   direct map), with a zero return address pushed, so the entry point sees
//!   a normal SysV call frame.
//! - The GDT and IDT still belong to the firmware. The kernel must load its
//!   own before enabling interrupts or reclaiming bootloader memory.
//!
//! # Virtual address space at kernel entry
//!
//! | Range                                | Contents                                   |
//! |--------------------------------------|--------------------------------------------|
//! | `0` .. end of RAM                    | Identity map (only so the bootloader can make the jump; the kernel should drop it) |
//! | [`HHDM_OFFSET`] ..                   | Direct map of all RAM and the framebuffer, non-executable |
//! | [`KERNEL_BASE`] ..                   | Kernel image, mapped per ELF segment permissions |
//!
//! All pointers in [`BootInfo`] are virtual addresses in the direct map, and
//! all addresses in [`MemoryRegion`] and [`Framebuffer`] are physical.

#![no_std]

/// Identifies a valid [`BootInfo`] block.
pub const MAGIC: u64 = u64::from_le_bytes(*b"eliOSbi\0");

/// Bumped on every incompatible change to the structures in this crate.
pub const VERSION: u32 = 1;

/// Where all of physical memory is mapped (the "higher half direct map").
pub const HHDM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Where the kernel image is linked. Must match `kernel/linker.ld`.
pub const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Size of the stack the kernel starts on.
pub const KERNEL_STACK_SIZE: u64 = 64 * 1024;

/// Everything the kernel learns from the bootloader.
#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _reserved: u32,
    /// Always [`HHDM_OFFSET`]; passed explicitly so the kernel never has to
    /// assume it.
    pub hhdm_offset: u64,
    /// Physical address of the active PML4.
    pub page_table_root: u64,
    pub memory_regions: *const MemoryRegion,
    pub memory_region_count: u64,
    pub framebuffer: Framebuffer,
    /// Physical address of the ACPI RSDP, or 0 if the firmware has none.
    pub rsdp: u64,
}

impl BootInfo {
    pub fn is_valid(&self) -> bool {
        self.magic == MAGIC && self.version == VERSION
    }

    /// The physical memory map, sorted by address, with adjacent regions of
    /// the same kind merged.
    pub fn memory_regions(&self) -> &[MemoryRegion] {
        // SAFETY: the bootloader guarantees the pointer and count describe a
        // live array in memory tagged `MemoryKind::BootInfo`.
        unsafe { core::slice::from_raw_parts(self.memory_regions, self.memory_region_count as usize) }
    }

    pub fn framebuffer(&self) -> Option<&Framebuffer> {
        (self.framebuffer.format != PixelFormat::Absent).then_some(&self.framebuffer)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start: u64,
    pub len: u64,
    pub kind: MemoryKind,
    pub _reserved: u32,
}

impl MemoryRegion {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM.
    Usable = 1,
    /// Firmware and bootloader memory. Free once the kernel has its own
    /// GDT, IDT and page tables.
    BootloaderReclaimable = 2,
    /// The loaded kernel image.
    Kernel = 3,
    /// The stack the kernel was entered on.
    KernelStack = 4,
    /// The page tables active at entry. Free once the kernel has its own.
    PageTables = 5,
    /// This [`BootInfo`] block and the memory map array.
    BootInfo = 6,
    /// ACPI tables. Free once the kernel has parsed them.
    AcpiReclaimable = 7,
    /// Must be preserved for the firmware's ACPI use.
    AcpiNvs = 8,
    /// Firmware runtime services. Never touch.
    FirmwareRuntime = 9,
    Mmio = 10,
    Reserved = 11,
    /// RAM the firmware found to be faulty.
    Defective = 12,
    /// Non-volatile RAM.
    Persistent = 13,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Framebuffer {
    /// Physical address of the first pixel.
    pub base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels (not bytes) per row; may be larger than `width`.
    pub stride: u32,
    pub format: PixelFormat,
}

impl Framebuffer {
    pub const ABSENT: Self = Self { base: 0, size: 0, width: 0, height: 0, stride: 0, format: PixelFormat::Absent };
}

/// Layout of a 32-bit pixel in memory, from the lowest byte up.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// There is no linear framebuffer.
    Absent = 0,
    /// Red, green, blue, reserved.
    Rgb = 1,
    /// Blue, green, red, reserved.
    Bgr = 2,
}
