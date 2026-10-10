//! Restarting and powering off.

use crate::arch::{self, inb, outb, outw};

/// Restarts the machine: through the keyboard controller's reset line, or
/// failing that, a deliberate triple fault.
pub fn restart() -> ! {
    info!("restarting");
    arch::disable_interrupts();
    // SAFETY: pulsing the reset line is what we want.
    unsafe {
        for _ in 0..100_000 {
            if inb(0x64) & 2 == 0 {
                break;
            }
        }
        outb(0x64, 0xFE);
    }
    // An empty IDT turns the next interrupt into a triple fault, which
    // resets the CPU.
    let empty = crate::arch::gdt::DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: intentionally fatal.
    unsafe { core::arch::asm!("lidt [{}]", "int3", in(reg) &raw const empty, options(noreturn)) };
}

/// Powers the machine off where we know how (virtual machines), otherwise
/// halts.
pub fn power_off() -> ! {
    info!("powering off");
    arch::disable_interrupts();
    // SAFETY: the shutdown ports of QEMU (q35 and i440fx), Bochs and
    // VirtualBox. On other machines nothing is listening there.
    unsafe {
        outw(0x604, 0x2000);
        outw(0xB004, 0x2000);
        outw(0x4004, 0x3400);
    }
    // Real hardware needs ACPI's sleep machinery, which comes later.
    crate::log::write_terminal("\nIt's now safe to turn off your computer.\n");
    arch::halt_forever()
}
