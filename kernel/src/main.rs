//! The eliOS kernel.

#![no_std]
#![no_main]

#[macro_use]
mod log;
#[cfg(test)]
#[macro_use]
mod test;

mod arch;
mod console;
mod font;
mod framebuffer;
mod logo;
mod serial;
mod sync;

use core::fmt::{self, Write as _};
use core::panic::PanicInfo;

use bootinfo::{BootInfo, MemoryKind};

use crate::framebuffer::Framebuffer;

/// Kernel entry point, jumped to by eliboot. See the `bootinfo` crate for the
/// machine state at this point.
#[unsafe(no_mangle)]
pub extern "sysv64" fn _start(boot_info: &'static BootInfo) -> ! {
    serial::init();
    // Separate our output from the firmware's on the serial line.
    serial::write_fmt(format_args!("\n"));

    if !boot_info.is_valid() {
        panic!("boot info has bad magic or version; was this kernel booted by eliboot?");
    }

    if let Some(info) = boot_info.framebuffer() {
        // SAFETY: the bootloader maps the framebuffer in the direct map, and
        // the console is the only thing that draws to it.
        console::init(unsafe { Framebuffer::new(info, boot_info.hhdm_offset) });
        console::with(|console| {
            logo::draw(console);
            console.flush();
        });
    }

    info!("eliOS {} starting", env!("CARGO_PKG_VERSION"));
    match boot_info.framebuffer() {
        Some(fb) => info!("display: {}x{}", fb.width, fb.height),
        None => warn!("no framebuffer; output is on the serial port only"),
    }
    log_memory(boot_info);

    #[cfg(test)]
    test::run(boot_info);

    #[cfg(not(test))]
    {
        info!("boot complete; nothing more to do yet, halting");
        arch::halt_forever()
    }
}

/// Logs a summary of physical memory, and the full map at debug level.
fn log_memory(boot_info: &BootInfo) {
    let regions = boot_info.memory_regions();
    for region in regions {
        debug!("{:#014x}-{:#014x} {:?}", region.start, region.end(), region.kind);
    }

    let total = |kind| regions.iter().filter(|r| r.kind == kind).map(|r| r.len).sum::<u64>();
    let usable = total(MemoryKind::Usable);
    let reclaimable = total(MemoryKind::BootloaderReclaimable) + total(MemoryKind::AcpiReclaimable);
    let kernel = total(MemoryKind::Kernel) + total(MemoryKind::KernelStack);
    info!("memory: {} usable, {} reclaimable later, kernel uses {}", Size(usable), Size(reclaimable), Size(kernel));
}

/// A byte count, displayed in the largest unit that keeps it at least 1.
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
        let mut value = self.0;
        let mut unit = 0;
        while value >= 1024 && unit < UNITS.len() - 1 {
            value /= 1024;
            unit += 1;
        }
        write!(f, "{value} {}", UNITS[unit])
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
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

        fn running_on_the_bootloaders_page_tables() {
            assert_eq!(arch::page_table_root(), boot_info().page_table_root);
        }

        fn running_on_the_kernel_stack() {
            let physical = arch::stack_pointer() - boot_info().hhdm_offset;
            let on_stack = boot_info()
                .memory_regions()
                .iter()
                .any(|r| r.kind == MemoryKind::KernelStack && (r.start..r.end()).contains(&physical));
            assert!(on_stack, "stack pointer is outside the KernelStack region");
        }

        fn sizes_display_in_sensible_units() {
            use core::fmt::Write as _;
            struct Buf([u8; 32], usize);
            impl fmt::Write for Buf {
                fn write_str(&mut self, s: &str) -> fmt::Result {
                    self.0[self.1..self.1 + s.len()].copy_from_slice(s.as_bytes());
                    self.1 += s.len();
                    Ok(())
                }
            }
            let show = |bytes| {
                let mut buf = Buf([0; 32], 0);
                write!(buf, "{}", Size(bytes)).unwrap();
                buf
            };
            assert_eq!(&show(512).0[..9], b"512 bytes");
            assert_eq!(&show(4096).0[..5], b"4 KiB");
            assert_eq!(&show(206 << 20).0[..7], b"206 MiB");
        }
    }
}
