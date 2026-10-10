//! Restarting and powering off.

use crate::acpi;
use crate::arch::{self, inb, inw, outb, outw};

/// Restarts the machine: through the ACPI reset register, the keyboard
/// controller's reset line, or failing those, a deliberate triple fault.
pub fn restart() -> ! {
    info!("restarting");
    arch::disable_interrupts();
    if let Some((port, value)) = acpi::power().and_then(|power| power.reset) {
        // SAFETY: the firmware says writing this value here resets.
        unsafe { outb(port, value) };
        spin(10_000_000);
    }
    // SAFETY: pulsing the reset line is what we want.
    unsafe {
        for _ in 0..100_000 {
            if inb(0x64) & 2 == 0 {
                break;
            }
        }
        outb(0x64, 0xFE);
    }
    spin(10_000_000);
    // An empty IDT turns the next interrupt into a triple fault, which
    // resets the CPU.
    let empty = crate::arch::gdt::DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: intentionally fatal.
    unsafe { core::arch::asm!("lidt [{}]", "int3", in(reg) &raw const empty, options(noreturn)) };
}

/// Powers the machine off through ACPI (entering sleep state S5), falling
/// back to the shutdown ports of common virtual machines.
pub fn power_off() -> ! {
    info!("powering off");
    arch::disable_interrupts();
    if let Some(power) = acpi::power() {
        const SCI_ENABLED: u16 = 1;
        const SLEEP_ENABLE: u16 = 1 << 13;
        // SAFETY: the ports and values come from the firmware's tables.
        unsafe {
            // Hand power management to ACPI if the firmware hasn't.
            if inw(power.pm1a_control) & SCI_ENABLED == 0 && power.smi_command != 0 && power.acpi_enable != 0 {
                outb(power.smi_command, power.acpi_enable);
                for _ in 0..1000 {
                    if inw(power.pm1a_control) & SCI_ENABLED != 0 {
                        break;
                    }
                    spin(100_000);
                }
            }
            outw(power.pm1a_control, power.sleep_type_a << 10 | SLEEP_ENABLE);
            if power.pm1b_control != 0 {
                outw(power.pm1b_control, power.sleep_type_b << 10 | SLEEP_ENABLE);
            }
        }
        spin(10_000_000);
    }
    // SAFETY: the shutdown ports of QEMU's older machines, Bochs and
    // VirtualBox. On other machines nothing is listening there.
    unsafe {
        outw(0xB004, 0x2000);
        outw(0x4004, 0x3400);
    }
    crate::log::write_terminal("\nIt's now safe to turn off your computer.\n");
    arch::halt_forever()
}

fn spin(iterations: u64) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}
