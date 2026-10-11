//! eliboot: the eliOS UEFI bootloader.
//!
//! Loads the kernel ELF from the boot volume, builds its page tables, gathers
//! what the kernel needs to know (memory map, framebuffer, ACPI), leaves UEFI
//! boot services and jumps into the kernel. See the `bootinfo` crate for the
//! exact handoff contract.

#![no_std]
#![no_main]

mod console;
mod cpu;
mod elf;
mod fs;
mod memory;
mod menu;
mod paging;
mod platform;
mod uefi;

use core::convert::Infallible;
use core::panic::PanicInfo;
use core::ptr;

use bootinfo::{BootInfo, Framebuffer, HHDM_OFFSET, KERNEL_BASE, KERNEL_STACK_SIZE, MemoryRegion};

use crate::elf::{Elf, PF_R, PF_W, PF_X, PT_LOAD};
use crate::memory::{ELIOS_BOOT_INFO, ELIOS_INITRD, ELIOS_KERNEL, ELIOS_KERNEL_STACK, MemoryMap, PAGE_SIZE};
use crate::paging::{NO_EXECUTE, PageTables, WRITABLE};
use crate::uefi::{Error, Handle, MemoryType, Status, SystemTable};

const KERNEL_PATH: &str = "\\EFI\\eliOS\\kernel.elf";
const INITRD_PATH: &str = "\\EFI\\eliOS\\initrd.cpio";
/// Options for the kernel (like `root=PARTUUID=...` on an installed system),
/// one line of text; optional.
const COMMAND_LINE_PATH: &str = "\\EFI\\eliOS\\cmdline.txt";

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: Handle, system_table: *mut SystemTable) -> Status {
    // SAFETY: this is the system table the firmware gave us.
    unsafe { uefi::init(system_table) };

    let error = match boot(image) {
        Ok(never) => match never {},
        Err(error) => error,
    };
    console::set_color(console::LIGHT_RED);
    println!("\neliboot: boot failed: {error}");
    cpu::halt_forever()
}

fn boot(image: Handle) -> Result<Infallible, Error> {
    let bs = uefi::boot_services();
    // The firmware reboots the machine if we take more than five minutes,
    // which is easy to hit while sitting in a debugger.
    // SAFETY: plain firmware call.
    unsafe { (bs.set_watchdog_timer)(0, 0, 0, ptr::null()) };

    console::clear();
    console::set_color(console::LIGHT_CYAN);
    println!("eliboot {}", env!("CARGO_PKG_VERSION"));
    console::set_color(console::LIGHT_GRAY);

    if cpu::five_level_paging_active() {
        return Err(Error::new("firmware enabled 5-level paging, which eliOS doesn't support yet"));
    }
    let no_execute = cpu::has_no_execute();

    let framebuffer = platform::framebuffer();
    match &framebuffer {
        Some(fb) => println!("framebuffer: {}x{} {:?} at {:#x}", fb.width, fb.height, fb.format, fb.base),
        None => println!("framebuffer: none"),
    }
    let rsdp = platform::find_rsdp();
    match rsdp {
        Some(address) => println!("ACPI RSDP:   {address:#x}"),
        None => println!("ACPI RSDP:   none"),
    }

    let mut page_tables = PageTables::new(no_execute)?;

    let kernel_file = fs::read_file(image, KERNEL_PATH, MemoryType::LOADER_DATA)?;
    println!("kernel:      {KERNEL_PATH} ({} KiB)", kernel_file.len() / 1024);
    let entry = load_kernel(kernel_file, &mut page_tables)?;

    // The initrd is optional: without one the kernel boots to a prompt-less
    // console, which is still useful for debugging.
    let initrd = match fs::read_file(image, INITRD_PATH, ELIOS_INITRD) {
        Ok(initrd) => {
            println!("initrd:      {INITRD_PATH} ({} KiB)", initrd.len() / 1024);
            Some(initrd)
        }
        Err(error) => {
            println!("initrd:      none ({error})");
            None
        }
    };

    let mut command_line = [0u8; bootinfo::COMMAND_LINE_SIZE];
    let mut command_line_len = 0;
    if let Ok(text) = fs::read_file(image, COMMAND_LINE_PATH, MemoryType::LOADER_DATA) {
        let text = core::str::from_utf8(text).unwrap_or("").lines().next().unwrap_or("").trim();
        command_line_len = text.len().min(command_line.len());
        command_line[..command_line_len].copy_from_slice(&text.as_bytes()[..command_line_len]);
        println!("options:     {}", &text[..command_line_len]);
    }
    if let Some(generation) = menu::choose_generation(image) {
        // Appended to the command line, for `eli` to find.
        let mut option = [0u8; 24];
        let mut writer = Buffer { bytes: &mut option, len: 0 };
        let _ = core::fmt::Write::write_fmt(&mut writer, format_args!(" generation={generation}"));
        let len = writer.len;
        let start = command_line_len.min(command_line.len() - len);
        command_line[start..start + len].copy_from_slice(&option[..len]);
        command_line_len = start + len;
        println!("starting generation {generation}");
    }
    let boot_partition = fs::boot_partition(image).unwrap_or([0; 16]);
    let runtime_services = uefi::system_table().map_or(0, |table| table.runtime_services as u64);

    let stack_pages = (KERNEL_STACK_SIZE / PAGE_SIZE) as usize;
    let stack = memory::allocate_pages(ELIOS_KERNEL_STACK, stack_pages)?;
    let stack_top = HHDM_OFFSET + stack + KERNEL_STACK_SIZE;

    let mut memory_map = MemoryMap::new()?;
    let mut direct_map_end = memory_map.end_of_ram().max(4 << 30);
    if let Some(fb) = &framebuffer {
        direct_map_end = direct_map_end.max(fb.base + fb.size);
    }
    page_tables.map_physical_memory(direct_map_end)?;

    // The boot info block, followed by the memory map array. Allocated now,
    // filled in after ExitBootServices when the memory map is final.
    let region_capacity = memory_map.max_descriptors();
    let boot_info_bytes = size_of::<BootInfo>() + region_capacity * size_of::<MemoryRegion>();
    let boot_info_phys = memory::allocate_pages(ELIOS_BOOT_INFO, boot_info_bytes.div_ceil(PAGE_SIZE as usize))?;
    let regions_phys = boot_info_phys + size_of::<BootInfo>() as u64;

    println!("entering kernel at {entry:#x}");

    // Nothing may touch the firmware between this snapshot and
    // ExitBootServices, or the map key goes stale.
    memory_map.refresh()?;
    // SAFETY: plain firmware call.
    let mut status = unsafe { (bs.exit_boot_services)(image, memory_map.key) };
    if status.is_error() {
        // The map changed under us (firmware events can allocate); the spec
        // says to fetch it again and retry.
        memory_map.refresh()?;
        // SAFETY: as above.
        status = unsafe { (bs.exit_boot_services)(image, memory_map.key) };
    }
    status.check("ExitBootServices failed")?;

    // From here on there is no firmware: no console, no allocation, and no
    // way to report errors.
    uefi::forget_system_table();
    cpu::disable_interrupts();

    // SAFETY: we allocated room for `region_capacity` regions after the
    // BootInfo, and the firmware still identity-maps it until we switch CR3.
    unsafe {
        let regions = core::slice::from_raw_parts_mut(regions_phys as *mut MemoryRegion, region_capacity);
        let region_count = memory_map.to_regions(regions);

        (boot_info_phys as *mut BootInfo).write(BootInfo {
            magic: bootinfo::MAGIC,
            version: bootinfo::VERSION,
            _reserved: 0,
            hhdm_offset: HHDM_OFFSET,
            page_table_root: page_tables.root(),
            memory_regions: (HHDM_OFFSET + regions_phys) as *const MemoryRegion,
            memory_region_count: region_count as u64,
            framebuffer: framebuffer.unwrap_or(Framebuffer::ABSENT),
            rsdp: rsdp.unwrap_or(0),
            initrd: initrd.map_or(ptr::null(), |initrd| (HHDM_OFFSET + initrd.as_ptr() as u64) as *const u8),
            initrd_size: initrd.map_or(0, |initrd| initrd.len() as u64),
            command_line,
            command_line_len: command_line_len as u64,
            boot_partition,
            runtime_services,
        });

        if no_execute {
            cpu::enable_no_execute();
        }
        cpu::enter_kernel(page_tables.root(), stack_top, entry, HHDM_OFFSET + boot_info_phys)
    }
}

/// Copies each loadable segment of the kernel into its own pages and maps them
/// at the segment's virtual address. Returns the entry point.
fn load_kernel(file: &[u8], page_tables: &mut PageTables) -> Result<u64, Error> {
    let elf = Elf::parse(file).map_err(Error::new)?;
    if elf.entry < KERNEL_BASE {
        return Err(Error::new("kernel entry point is outside the kernel's address range"));
    }

    let mut segments = 0;
    for segment in elf.program_headers().filter(|s| s.kind == PT_LOAD && s.mem_size > 0) {
        if segment.vaddr < KERNEL_BASE
            || segment.file_size > segment.mem_size
            || segment.vaddr.checked_add(segment.mem_size).is_none()
        {
            return Err(Error::new("kernel has a malformed or misplaced segment"));
        }
        let data = elf.segment_data(&segment).ok_or(Error::new("kernel segment extends past end of file"))?;

        let offset_in_page = segment.vaddr % PAGE_SIZE;
        let first_page = segment.vaddr - offset_in_page;
        let pages = (offset_in_page + segment.mem_size).div_ceil(PAGE_SIZE);
        // Zeroed, so the part of the segment not stored in the file (.bss)
        // starts out as zeros.
        let phys = memory::allocate_zeroed_pages(ELIOS_KERNEL, pages as usize)?;
        // SAFETY: the destination is inside the pages we just allocated.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), (phys + offset_in_page) as *mut u8, data.len()) };

        let mut flags = 0;
        if segment.flags & PF_W != 0 {
            flags |= WRITABLE;
        }
        if segment.flags & PF_X == 0 {
            flags |= NO_EXECUTE;
        }
        for page in 0..pages {
            page_tables.map_page(first_page + page * PAGE_SIZE, phys + page * PAGE_SIZE, flags)?;
        }

        let permission = |flag, c| if segment.flags & flag != 0 { c } else { '-' };
        println!(
            "  segment {:#x} {}{}{} {:>4} KiB",
            segment.vaddr,
            permission(PF_R, 'r'),
            permission(PF_W, 'w'),
            permission(PF_X, 'x'),
            segment.mem_size.div_ceil(1024),
        );
        segments += 1;
    }

    if segments == 0 {
        return Err(Error::new("kernel has nothing to load"));
    }
    Ok(elf.entry)
}

/// Formats into a fixed buffer (eliboot has no allocator).
struct Buffer<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for Buffer<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::set_color(console::LIGHT_RED);
    println!("\neliboot panicked: {info}");
    cpu::halt_forever()
}
