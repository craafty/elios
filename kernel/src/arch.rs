//! x86_64 instructions the rest of the kernel needs.

use core::arch::asm;

/// # Safety
/// Writing to an I/O port can reconfigure hardware arbitrarily.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Reading some I/O ports has side effects on the device.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Physical address of the active top-level page table.
#[cfg(test)]
pub fn page_table_root() -> u64 {
    let cr3: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    cr3 & 0x000F_FFFF_FFFF_F000
}

#[cfg(test)]
pub fn stack_pointer() -> u64 {
    let rsp: u64;
    // SAFETY: reading RSP has no side effects.
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    rsp
}

pub fn disable_interrupts() {
    // SAFETY: masking interrupts can't violate memory safety.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

pub fn halt_forever() -> ! {
    loop {
        // SAFETY: stops the CPU; with interrupts masked it never wakes.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
