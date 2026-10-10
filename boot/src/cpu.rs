//! Direct CPU access: feature detection, control registers and the jump into
//! the kernel.

use core::arch::asm;

const IA32_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;
const CR4_LA57: u64 = 1 << 12;

/// Returns `[eax, ebx, ecx, edx]` for the given leaf (subleaf 0).
pub fn cpuid(leaf: u32) -> [u32; 4] {
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
            inout("ecx") 0 => ecx,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    [eax, rbx as u32, ecx, edx]
}

pub fn has_no_execute() -> bool {
    let [max_extended_leaf, ..] = cpuid(0x8000_0000);
    max_extended_leaf >= 0x8000_0001 && cpuid(0x8000_0001)[3] & (1 << 20) != 0
}

/// Whether the firmware left 5-level paging on, which our 4-level tables
/// can't work with.
pub fn five_level_paging_active() -> bool {
    let cr4: u64;
    // SAFETY: reading CR4 has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    cr4 & CR4_LA57 != 0
}

/// # Safety
/// The CPU must support NX (see [`has_no_execute`]).
pub unsafe fn enable_no_execute() {
    unsafe {
        let (low, high): (u32, u32);
        asm!("rdmsr", in("ecx") IA32_EFER, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
        let efer = (u64::from(high) << 32 | u64::from(low)) | EFER_NXE;
        asm!(
            "wrmsr",
            in("ecx") IA32_EFER,
            in("eax") efer as u32,
            in("edx") (efer >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

pub fn disable_interrupts() {
    // SAFETY: masking interrupts can't violate memory safety.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

pub fn halt_forever() -> ! {
    loop {
        // SAFETY: stops the CPU until the next interrupt, which never comes.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Switches to the kernel's page tables and stack and jumps to its entry
/// point with `boot_info` as the first argument.
///
/// # Safety
/// `page_tables` must map the currently executing code at its current address,
/// and `stack_top`, `entry` and `boot_info` must be valid under them.
pub unsafe fn enter_kernel(page_tables: u64, stack_top: u64, entry: u64, boot_info: u64) -> ! {
    unsafe {
        asm!(
            "mov cr3, {page_tables}",
            "mov rsp, {stack_top}",
            "xor ebp, ebp",
            // A zero return address: makes the stack look like the kernel
            // was called (keeping SysV alignment) and ends stack traces.
            "push 0",
            "jmp {entry}",
            page_tables = in(reg) page_tables,
            stack_top = in(reg) stack_top,
            entry = in(reg) entry,
            in("rdi") boot_info,
            options(noreturn),
        )
    }
}
