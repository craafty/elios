//! The eliOS kernel.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod log;
#[cfg(test)]
#[macro_use]
mod test;

mod acpi;
mod arch;
mod collections;
mod console;
mod drivers;
mod errno;
mod font;
mod framebuffer;
mod fs;
mod keyboard;
mod logo;
mod memory;
mod power;
mod process;
mod random;
mod rtc;
mod serial;
mod sync;
mod syscall;
mod task;
mod time;
mod tty;

use core::fmt::{self, Write as _};
use core::panic::PanicInfo;

use bootinfo::BootInfo;

use crate::framebuffer::Framebuffer;
use crate::memory::{CacheType, KernelStack};
use crate::sync::SpinLock;

/// The kernel's main stack, in pages.
const KERNEL_STACK_PAGES: u64 = 16;

/// Hands the stack `_start` switches to over to `kernel_main`, whose thread
/// owns it.
static BOOT_STACK: SpinLock<Option<KernelStack>> = SpinLock::new(None);

/// Kernel entry point, jumped to by eliboot. See the `bootinfo` crate for the
/// machine state at this point.
///
/// This stage still runs on the bootloader's stack and page tables. It sets
/// up exception handling, reads what it needs from the firmware, and builds
/// the kernel's own address space, then moves onto a kernel stack in it.
#[unsafe(no_mangle)]
pub extern "sysv64" fn _start(boot_info: &'static BootInfo) -> ! {
    time::mark_boot();
    serial::init();
    // Separate our output from the firmware's on the serial line.
    serial::write_fmt(format_args!("\n"));

    if !boot_info.is_valid() {
        panic!("boot info has bad magic or version; was this kernel booted by eliboot?");
    }
    info!("eliOS {} starting", env!("CARGO_PKG_VERSION"));

    // The boot CPU's per-CPU block; its APIC ID is filled in once the APIC
    // is up.
    let cpu = arch::cpu::allocate(0).expect("no per-CPU block for the boot CPU");
    arch::cpu::install(cpu);
    arch::gdt::init(cpu);
    arch::interrupts::init();
    arch::syscall::init();
    arch::fpu::init();
    debug!("GDT, TSS, IDT, system calls and FPU set up");

    match acpi::init(boot_info.rsdp) {
        Ok(madt) => info!(
            "acpi: {} CPUs, {} I/O APICs, {} IRQ overrides",
            madt.processors.len(),
            madt.io_apics.len(),
            madt.overrides.len()
        ),
        Err(error) => panic!("can't set up interrupts: {error}"),
    }
    match acpi::power() {
        Some(power) => debug!("acpi: power off via port {:#x}, S5 type {}", power.pm1a_control, power.sleep_type_a),
        None => warn!("acpi: no power-off information"),
    }

    memory::init(boot_info);
    let stack = memory::allocate_kernel_stack(KERNEL_STACK_PAGES);
    let top = stack.top;
    *BOOT_STACK.lock() = Some(stack);
    // SAFETY: the stack was just mapped, and nothing on the current stack is
    // used again.
    unsafe { arch::switch_stack(top, kernel_main, core::ptr::from_ref(boot_info) as u64) }
}

/// The rest of startup, on the kernel's own stack and page tables.
extern "sysv64" fn kernel_main(boot_info: u64) -> ! {
    // SAFETY: `_start` passes its `&'static BootInfo`.
    let boot_info = unsafe { &*(boot_info as *const BootInfo) };
    let reclaimed = memory::reclaim_boot_memory(boot_info);
    task::init("kernel", BOOT_STACK.lock().take().expect("boot stack missing"));

    if let Some(info) = boot_info.framebuffer() {
        let pixels = memory::map_device(info.base, info.size, CacheType::WriteCombining);
        // SAFETY: just mapped, and only the console draws to it.
        console::init(unsafe { Framebuffer::new(info, pixels.cast()) });
        console::with(|console| {
            logo::draw(console);
            // Show what was logged before the console existed.
            log::replay(console);
            console.flush();
        });
    }

    match boot_info.framebuffer() {
        Some(fb) => info!("display: {}x{}", fb.width, fb.height),
        None => warn!("no framebuffer; output is on the serial port only"),
    }
    log_memory(boot_info, reclaimed);

    let madt = acpi::madt().expect("ACPI tables were read at startup");
    if madt.legacy_pics {
        arch::pic::disable();
    }
    arch::apic::init_local(madt);
    arch::cpu::current().apic_id.store(u32::from(arch::apic::local_id()), core::sync::atomic::Ordering::Relaxed);
    arch::apic::init_io(madt);
    time::init();
    match arch::smp::start_others(boot_info, madt) {
        1 => info!("smp: running on 1 CPU"),
        count => info!("smp: running on {count} CPUs"),
    }
    match keyboard::init() {
        Ok(()) => info!("keyboard: PS/2 ready"),
        Err(error) => warn!("keyboard: {error}"),
    }

    let initrd = boot_info.initrd();
    fs::init(initrd);
    if let Some(initrd) = initrd {
        // Unpacked; the original isn't needed any more.
        let start = initrd.as_ptr() as u64 - boot_info.hhdm_offset;
        memory::frames::reclaim(start, start + initrd.len() as u64);
    }
    if let Ok(name) = fs::lookup("/", "/etc/hostname", true) {
        let mut buffer = [0; 64];
        let len = name.read_at(0, &mut buffer).unwrap_or(0);
        if let Ok(text) = core::str::from_utf8(&buffer[..len]) {
            *syscall::HOSTNAME.lock() = alloc::string::String::from(text.trim());
        }
    }
    mount_disk(drivers::init());
    if let Err(error) = serial::enable_input() {
        warn!("serial: no input: {error}");
    }
    keyboard::start_terminal_input();
    arch::enable_interrupts();

    #[cfg(test)]
    test::run(boot_info);

    #[cfg(not(test))]
    {
        match process::start_init("/sbin/init") {
            Ok(_) => info!("boot complete, starting /sbin/init"),
            Err(error) => error!("can't start /sbin/init: {error:?}; is the initrd missing?"),
        }
        task::exit()
    }
}

/// Mounts the first FAT32 partition found (the boot disk's EFI partition,
/// normally) at /disk.
fn mount_disk(disks: alloc::vec::Vec<alloc::sync::Arc<dyn drivers::block::Disk>>) {
    use drivers::block::{PartitionKind, partitions};
    let candidates = disks.iter().flat_map(partitions).filter(|p| p.kind != PartitionKind::Other);
    for partition in candidates {
        let name = alloc::string::String::from(partition.disk.name());
        match fs::fatfs::mount(partition) {
            Ok((root, total, free)) => {
                if fs::root().link("disk", root).is_ok() {
                    info!("disk: {name} mounted at /disk ({} free of {})", Size(free), Size(total));
                }
                return;
            }
            Err(error) => warn!("disk: {name}: can't mount: {error:?}"),
        }
    }
    if !disks.is_empty() {
        warn!("disk: no FAT32 partition to mount");
    }
}

/// Logs a summary of memory, and the boot memory map at debug level.
fn log_memory(boot_info: &BootInfo, reclaimed: u64) {
    for region in boot_info.memory_regions() {
        debug!("{:#014x}-{:#014x} {:?}", region.start, region.end(), region.kind);
    }
    let frames = memory::frames::stats();
    let heap = memory::heap::stats();
    info!(
        "memory: {} free of {} ({} reclaimed from the bootloader), heap {}",
        Size(frames.free_bytes),
        Size(frames.managed_bytes),
        Size(reclaimed),
        Size(heap.bytes_mapped),
    );
}

/// A byte count, displayed in the largest unit that keeps it at least 1,
/// with one decimal place below 10.
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
        let mut unit = 0;
        while self.0 >> (10 * (unit + 1)) > 0 && unit < UNITS.len() - 1 {
            unit += 1;
        }
        let tenths = (self.0 * 10) >> (10 * unit);
        if unit > 0 && tenths < 100 && !tenths.is_multiple_of(10) {
            write!(f, "{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
        } else {
            write!(f, "{} {}", tenths / 10, UNITS[unit])
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
    arch::smp::stop_others();
    // SAFETY: we never return, so interleaving with an interrupted writer is
    // the worst that can happen.
    let mut port = unsafe { serial::panic_port() };
    let _ = writeln!(port, "\n\x1B[91m*** KERNEL PANIC ***\x1B[0m\n{info}");
    console::try_with(|console| {
        let _ = writeln!(console, "\n\x1B[91m*** KERNEL PANIC ***\x1B[0m\n{info}");
        console.flush();
    });

    #[cfg(test)]
    test::on_panic();
    #[cfg(not(test))]
    arch::halt_forever()
}

#[cfg(test)]
mod tests {
    use bootinfo::MemoryKind;

    use super::*;
    use crate::test::boot_info;

    crate::ktest! {
        fn memory_map_is_sorted_and_disjoint() {
            let regions = boot_info().memory_regions();
            assert!(!regions.is_empty());
            for pair in regions.windows(2) {
                assert!(pair[0].end() <= pair[1].start, "{:x?} overlaps {:x?}", pair[0], pair[1]);
            }
            for region in regions {
                assert!(region.len > 0 && region.start % 4096 == 0 && region.len % 4096 == 0, "{region:x?}");
            }
        }

        fn memory_map_has_the_kernel_and_free_memory() {
            let has = |kind| boot_info().memory_regions().iter().any(|r| r.kind == kind);
            for kind in [MemoryKind::Usable, MemoryKind::Kernel, MemoryKind::KernelStack, MemoryKind::PageTables] {
                assert!(has(kind), "no {kind:?} region");
            }
        }

        fn sizes_display_in_sensible_units() {
            let show = |bytes| {
                let mut text = crate::test::StrBuf::<32>::new();
                write!(text, "{}", Size(bytes)).unwrap();
                text
            };
            assert_eq!(show(512).as_str(), "512 bytes");
            assert_eq!(show(4096).as_str(), "4 KiB");
            assert_eq!(show(206 << 20).as_str(), "206 MiB");
            assert_eq!(show(4_000_000).as_str(), "3.8 MiB");
            assert_eq!(show(1 << 30).as_str(), "1 GiB");
        }
    }
}
