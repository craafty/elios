//! x86_64 support: CPU instructions, descriptor tables, interrupts and the
//! interrupt controllers.

pub mod apic;
pub mod fpu;
pub mod gdt;
pub mod interrupts;
pub mod pic;
pub mod pit;
pub mod syscall;

use core::arch::asm;

/// # Safety
/// Writing to an I/O port can reconfigure hardware arbitrarily.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Writing to an I/O port can reconfigure hardware arbitrarily.
pub unsafe fn outw(port: u16, value: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Reading some I/O ports has side effects on the device.
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe { asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
/// Reading some I/O ports has side effects on the device.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
/// Writing to an I/O port can reconfigure hardware arbitrarily.
pub unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Reading some I/O ports has side effects on the device.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags)) };
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

/// A hardware random number, if the CPU has a generator (RDRAND).
pub fn hardware_random() -> Option<u64> {
    if cpuid(1, 0)[2] & (1 << 30) == 0 {
        return None;
    }
    for _ in 0..10 {
        let (value, ok): (u64, u8);
        // SAFETY: the CPU supports rdrand (checked above).
        unsafe { asm!("rdrand {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack)) };
        if ok == 1 {
            return Some(value);
        }
    }
    None
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
pub fn page_table_root() -> u64 {
    let cr3: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    cr3 & 0x000F_FFFF_FFFF_F000
}

/// Switches to another set of page tables, flushing all non-global TLB
/// entries.
///
/// # Safety
/// The new tables must map everything currently in use: this code, the
/// stack, and anything referenced through pointers.
pub unsafe fn set_page_table_root(root: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
}

/// Removes any cached translation for the page containing `address`.
pub fn flush_tlb_page(address: u64) {
    // SAFETY: invalidating a TLB entry only costs a page walk later.
    unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

/// Makes the CPU honor the global bit in page tables: global translations
/// survive address space switches, which suits kernel mappings shared by
/// every address space.
pub fn enable_global_pages() {
    // SAFETY: setting CR4.PGE only changes how long TLB entries live.
    unsafe {
        asm!(
            "mov {tmp}, cr4",
            "or {tmp}, {pge}",
            "mov cr4, {tmp}",
            tmp = out(reg) _,
            pge = const 1 << 7,
            options(nostack, preserves_flags),
        )
    };
}

const IA32_PAT: u32 = 0x277;

/// Programs the page attribute table so page table entries can select
/// write-combining memory (PWT alone), which makes framebuffer writes much
/// faster on real hardware. The other entries keep their power-on meaning.
pub fn init_page_attributes() {
    // Entries 0-7: write-back, write-combining, uncached-minus, uncached,
    // write-back, write-through, uncached-minus, uncached.
    const PAT: u64 = u64::from_le_bytes([6, 1, 7, 0, 6, 4, 7, 0]);
    // SAFETY: no existing mapping uses entry 1, the only one that changes,
    // and caches are flushed so no stale line has the old type.
    unsafe {
        asm!("wbinvd", options(nostack, preserves_flags));
        wrmsr(IA32_PAT, PAT);
    }
}

/// Moves onto a new stack and calls `entry(argument)` there. The current
/// stack is abandoned.
///
/// # Safety
/// `stack_top` must be the 16-byte-aligned top of a mapped, unused stack.
pub unsafe fn switch_stack(stack_top: u64, entry: extern "sysv64" fn(u64) -> !, argument: u64) -> ! {
    unsafe {
        asm!(
            "mov rsp, {stack_top}",
            "xor ebp, ebp",
            // A zero return address, as at kernel entry: keeps SysV stack
            // alignment and ends stack traces.
            "push 0",
            "jmp {entry}",
            stack_top = in(reg) stack_top,
            entry = in(reg) entry,
            in("rdi") argument,
            options(noreturn),
        )
    }
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
