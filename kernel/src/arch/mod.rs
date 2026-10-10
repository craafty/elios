//! x86_64 support: CPU instructions, descriptor tables, interrupts and the
//! interrupt controllers.

pub mod apic;
pub mod gdt;
pub mod interrupts;
pub mod pic;
pub mod pit;

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

/// A short delay for old devices that need time between port accesses.
pub fn io_wait() {
    // SAFETY: port 0x80 is the POST code port; writing it is harmless.
    unsafe { outb(0x80, 0) };
}

/// # Safety
/// Reading a model-specific register that doesn't exist faults, and some
/// have side effects.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    u64::from(high) << 32 | u64::from(low)
}

/// # Safety
/// Model-specific registers control fundamental CPU behavior.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        )
    };
}

/// Returns `[eax, ebx, ecx, edx]` for a CPUID leaf and subleaf.
pub fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    let (eax, ecx, edx): (u32, u32, u32);
    let rbx: u64;
    // SAFETY: cpuid is available on every x86_64 CPU. LLVM reserves rbx, so
    // it is saved and restored around the instruction.
    unsafe {
        asm!(
            "mov {rbx}, rbx",
            "cpuid",
            "xchg {rbx}, rbx",
            rbx = out(reg) rbx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    [eax, rbx as u32, ecx, edx]
}

/// The CPU's timestamp counter.
pub fn rdtsc() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: rdtsc only reads a counter.
    unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    u64::from(high) << 32 | u64::from(low)
}

/// The address that caused the most recent page fault.
pub fn read_cr2() -> u64 {
    let cr2: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
    cr2
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

pub fn interrupts_enabled() -> bool {
    let rflags: u64;
    // SAFETY: reads RFLAGS through the stack.
    unsafe { asm!("pushfq", "pop {}", out(reg) rflags, options(nomem, preserves_flags)) };
    rflags & (1 << 9) != 0
}

// The interrupt flag instructions deliberately don't use `nomem`: they double
// as compiler barriers, so memory accesses can't be moved across them into or
// out of a critical section.

pub fn disable_interrupts() {
    // SAFETY: masking interrupts can't violate memory safety.
    unsafe { asm!("cli", options(nostack)) };
}

pub fn enable_interrupts() {
    // SAFETY: the IDT is set up before anything enables interrupts.
    unsafe { asm!("sti", options(nostack)) };
}

/// Enables interrupts and sleeps until one arrives. The two instructions
/// execute as a unit, so an interrupt can't slip in between and be missed.
pub fn wait_for_interrupt() {
    // SAFETY: as for `enable_interrupts`.
    unsafe { asm!("sti", "hlt", options(nostack)) };
}

pub fn halt_forever() -> ! {
    loop {
        // SAFETY: stops the CPU; with interrupts masked it never wakes.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
