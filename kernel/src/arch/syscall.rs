//! The `syscall` instruction's entry point, per-CPU data, and entering user
//! mode.
//!
//! `syscall` doesn't switch stacks, so the entry code first swaps GS to the
//! kernel's per-CPU block, which holds the current thread's kernel stack. It
//! then builds an [`InterruptFrame`] exactly like an interrupt would, so
//! system calls, interrupts and exceptions all describe user state the same
//! way, and all return through `interrupt_return`.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

use super::gdt::{self, KERNEL_CODE, USER_CODE, USER_DATA};
use super::interrupts::InterruptFrame;
use super::{rdmsr, wrmsr};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const IA32_FS_BASE: u32 = 0xC000_0100;
const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

const EFER_SYSCALL_ENABLE: u64 = 1 << 0;
/// RFLAGS bits cleared on entry: interrupts, direction, trap, alignment check.
const SYSCALL_FLAG_MASK: u64 = 0x200 | 0x400 | 0x100 | 0x4_0000;

/// Per-CPU data, reached through GS while in the kernel. The entry code
/// relies on the field offsets.
#[repr(C)]
struct CpuLocal {
    /// Top of the running thread's kernel stack (offset 0).
    kernel_stack: AtomicU64,
    /// The user stack pointer during system call entry (offset 8).
    user_stack: AtomicU64,
}

static CPU_LOCAL: CpuLocal = CpuLocal { kernel_stack: AtomicU64::new(0), user_stack: AtomicU64::new(0) };

global_asm!(
    r#"
    .section .text.syscall_entry, "ax"
    .global syscall_entry
syscall_entry:
    swapgs
    mov gs:[8], rsp
    mov rsp, gs:[0]
    // The frame an interrupt from user mode would have pushed: ss, rsp,
    // rflags (in r11), cs, rip (in rcx), error code, vector.
    push {user_data}
    push qword ptr gs:[8]
    push r11
    push {user_code}
    push rcx
    push 0
    // The vector field: -1 marks a system call rather than an interrupt.
    push -1
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    call syscall_dispatch
    jmp interrupt_return
    "#,
    user_data = const USER_DATA,
    user_code = const USER_CODE,
);

unsafe extern "C" {
    fn syscall_entry();
    fn interrupt_return();
}

/// Enables the `syscall` instruction and sets up per-CPU data.
pub fn init() {
    // SAFETY: the standard configuration for 64-bit system calls. The
    // selector bases match the GDT layout (see gdt.rs).
    unsafe {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | EFER_SYSCALL_ENABLE);
        // Bits 32-47: kernel CS (SS is +8). Bits 48-63: the base sysret
        // derives user selectors from (SS = base + 8, CS = base + 16).
        let star = u64::from(KERNEL_CODE) << 32 | u64::from(gdt::KERNEL_DATA) << 48;
        wrmsr(IA32_STAR, star);
        wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);
        wrmsr(IA32_FMASK, SYSCALL_FLAG_MASK);
        // In the kernel, GS points at the per-CPU block; `swapgs` trades it
        // for the user's value on the way in and out.
        wrmsr(IA32_GS_BASE, &raw const CPU_LOCAL as u64);
        wrmsr(IA32_KERNEL_GS_BASE, 0);
    }
}

/// Records the stack to use when the current thread enters the kernel from
/// user mode (by system call or interrupt).
pub fn set_kernel_stack(top: u64) {
    CPU_LOCAL.kernel_stack.store(top, Ordering::Relaxed);
    gdt::set_kernel_stack(top);
}

/// The base address of the FS segment, which user programs use for
/// thread-local storage.
pub fn fs_base() -> u64 {
    // SAFETY: reading an architectural MSR.
    unsafe { rdmsr(IA32_FS_BASE) }
}

pub fn set_fs_base(base: u64) {
    // SAFETY: FS is only used by user programs; the kernel never relies on it.
    unsafe { wrmsr(IA32_FS_BASE, base) };
}

/// A frame that starts user code at `entry` with stack pointer `stack`.
pub fn user_frame(entry: u64, stack: u64) -> InterruptFrame {
    InterruptFrame {
        rip: entry,
        rsp: stack,
        cs: u64::from(USER_CODE),
        ss: u64::from(USER_DATA),
        // Interrupts enabled; bit 1 is always set.
        rflags: 0x202,
        ..Default::default()
    }
}

/// Switches to user mode with the register state in `frame`.
///
/// # Safety
/// The current address space must map the code and stack `frame` refers
/// to, with user access.
pub unsafe fn enter_user(frame: &InterruptFrame) -> ! {
    super::disable_interrupts();
    unsafe {
        asm!(
            "mov rsp, {frame}",
            "jmp {interrupt_return}",
            frame = in(reg) core::ptr::from_ref(frame),
            interrupt_return = sym interrupt_return,
            options(noreturn),
        )
    }
}
