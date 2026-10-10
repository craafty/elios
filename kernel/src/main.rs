//! The eliOS kernel.

#![no_std]
#![no_main]

mod arch;
mod framebuffer;
mod serial;
mod sync;

use core::fmt::Write as _;
use core::panic::PanicInfo;

use bootinfo::{BootInfo, MemoryKind};

use crate::framebuffer::Framebuffer;

/// Kernel entry point, jumped to by eliboot. See the `bootinfo` crate for the
/// machine state at this point.
#[unsafe(no_mangle)]
pub extern "sysv64" fn _start(boot_info: &'static BootInfo) -> ! {
    serial::init();
    println!();
    println!("eliOS kernel {}", env!("CARGO_PKG_VERSION"));

    if !boot_info.is_valid() {
        panic!("boot info has bad magic or version; was this kernel booted by eliboot?");
    }

    print_memory_map(boot_info);

    match boot_info.framebuffer() {
        Some(info) => {
            println!("framebuffer: {}x{} {:?}", info.width, info.height, info.format);
            // SAFETY: the bootloader maps the framebuffer in the direct map,
            // and nothing else draws to it.
            let mut fb = unsafe { Framebuffer::new(info, boot_info.hhdm_offset) };
            framebuffer::draw_splash(&mut fb);
        }
        None => println!("framebuffer: none"),
    }

    println!("eliOS: boot complete, halting");
    arch::halt_forever()
}

fn print_memory_map(boot_info: &BootInfo) {
    println!("memory map:");
    let mut usable = 0;
    for region in boot_info.memory_regions() {
        println!("  {:#014x}-{:#014x} {:>9} KiB  {:?}", region.start, region.end(), region.len / 1024, region.kind);
        if region.kind == MemoryKind::Usable {
            usable += region.len;
        }
    }
    println!("usable memory: {} MiB", usable / (1024 * 1024));
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
    // SAFETY: we never return, so interleaving with an interrupted writer is
    // the worst that can happen.
    let mut port = unsafe { serial::panic_port() };
    let _ = writeln!(port, "\n*** KERNEL PANIC ***\n{info}");
    arch::halt_forever()
}
