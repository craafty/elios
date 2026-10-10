//! Signals: sending, masking, default actions, and running handlers.
//!
//! Handlers run the way Linux runs them: just before the kernel returns to
//! user mode, it saves the interrupted registers in a signal frame on the
//! user stack (laid out exactly as Linux's `struct rt_sigframe`, so C
//! libraries understand it) and enters the handler. The handler returns to a
//! "restorer" the C library supplies, which calls `rt_sigreturn`; that
//! reloads the saved registers from the frame.

use alloc::vec::Vec;

use super::{Process, user};
use crate::arch::fpu::{self, FpuState};
use crate::arch::interrupts::InterruptFrame;
use crate::errno::{Errno, Result};
use crate::task;

// Signal numbers (Linux values).
pub const SIGINT: u32 = 2;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;
pub const SIGPIPE: u32 = 13;
#[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGURG: u32 = 23;
pub const SIGWINCH: u32 = 28;
pub const SIGNAL_COUNT: usize = 65;

/// Handler values with special meanings.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

// sa_flags bits.
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;

/// `struct sigaction` as the kernel ABI has it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// A process's signal state.
#[derive(Clone)]
pub struct Signals {
    pending: u64,
    blocked: u64,
    pub actions: [SigAction; SIGNAL_COUNT],
    /// The alternate signal stack (base, size), if one is set.
    alternate_stack: Option<(u64, u64)>,
    /// For a signal raised by a CPU fault: the faulting address.
    fault_address: u64,
}

impl Signals {
    pub const fn new() -> Self {
        Self {
            pending: 0,
            blocked: 0,
            actions: [SigAction { handler: SIG_DFL, flags: 0, restorer: 0, mask: 0 }; SIGNAL_COUNT],
            alternate_stack: None,
            fault_address: 0,
        }
    }

    /// The state a forked child starts with: everything but pending signals.
    pub fn for_child(&self) -> Self {
        Self { pending: 0, ..self.clone() }
    }

    /// After `exec`: handlers point into the old program, so they go back to
    /// the default (ignored signals stay ignored). The mask is kept.
    pub fn exec(&mut self) {
        for action in &mut self.actions {
            if action.handler != SIG_IGN {
                *action = SigAction::default();
            }
        }
        self.alternate_stack = None;
    }

    fn ignored(&self, signal: u32) -> bool {
        match self.actions[signal as usize].handler {
            _ if signal == SIGKILL || signal == SIGSTOP => false,
            SIG_IGN => true,
            SIG_DFL => ignored_by_default(signal),
            _ => false,
        }
    }

    /// Pending signals that can be acted on now.
    fn deliverable(&self) -> u64 {
        let unblockable = bit(SIGKILL) | bit(SIGSTOP);
        self.pending & (!self.blocked | unblockable)
    }
}

/// A signal's bit in a signal set: as in Linux, signal n is bit n - 1.
pub fn bit(signal: u32) -> u64 {
    1 << (signal - 1)
}

/// Signals whose default action is to do nothing.
fn ignored_by_default(signal: u32) -> bool {
    // Stopping isn't supported, so the stop signals are ignored too.
    matches!(signal, SIGCHLD | SIGCONT | SIGURG | SIGWINCH | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

/// Marks a signal pending and, if it will do something, wakes the process
/// so it notices.
pub fn send(process: &Process, signal: u32) {
    if signal == 0 || signal as usize >= SIGNAL_COUNT || process.status.lock().is_some() {
        return;
    }
    let wake = {
        let mut signals = process.signals.lock();
        if signals.ignored(signal) {
            return;
        }
        signals.pending |= bit(signal);
        signals.deliverable() & bit(signal) != 0
    };
    if wake && let Some(thread) = *process.thread.lock() {
        task::interrupt(thread);
    }
}

/// Raises a signal for a CPU fault in the running process. Unlike other
/// signals it can't be ignored or blocked: if it isn't handled, the process
/// ends.
pub fn raise_fault(process: &Process, signal: u32, address: u64) -> bool {
    let mut signals = process.signals.lock();
    let action = signals.actions[signal as usize];
    let handled = action.handler != SIG_DFL && action.handler != SIG_IGN && signals.blocked & bit(signal) == 0;
    if handled {
        signals.pending |= bit(signal);
        signals.fault_address = address;
    }
    handled
}

/// Whether the running process has a signal that should interrupt a
/// blocking system call.
pub fn pending() -> bool {
    task::current_process().is_some_and(|process| process.signals.lock().deliverable() != 0)
}

/// Acts on pending signals before returning to user mode: ends the process
/// for signals with a fatal default action, or sets up `frame` to run a
/// handler. `syscall` is the number of the system call being returned from,
/// if any, so calls interrupted by a handler with `SA_RESTART` can resume.
pub fn deliver(process: &Process, frame: &mut InterruptFrame, syscall: Option<u64>) {
    let (signal, action) = {
        let mut signals = process.signals.lock();
        // Pending signals that are ignored by now are dropped.
        let ignored = (1..SIGNAL_COUNT as u32).filter(|&s| signals.ignored(s)).fold(0, |m, s| m | bit(s));
        signals.pending &= !ignored;
        let deliverable = signals.deliverable();
        if deliverable == 0 {
            return;
        }
        let signal = deliverable.trailing_zeros() + 1;
        signals.pending &= !bit(signal);
        (signal, signals.actions[signal as usize])
    };

    if action.handler == SIG_DFL || signal == SIGKILL {
        crate::arch::enable_interrupts();
        super::exit(super::signal_status(signal));
    }

    if let Some(number) = syscall
        && frame.rax as i64 == -(Errno::EINTR as i64)
        && action.flags & SA_RESTART != 0
    {
        // Back up over the `syscall` instruction (2 bytes) to run it again
        // once the handler returns.
        frame.rax = number;
        frame.rip -= 2;
    }

    if push_frame(process, frame, signal, &action).is_err() {
        // No usable stack for the handler: nothing to do but end it.
        crate::arch::enable_interrupts();
        super::exit(super::signal_status(SIGSEGV));
    }
}

const UCONTEXT_SIZE: u64 = 304;
const SIGINFO_SIZE: u64 = 128;
/// Bytes below the stack pointer that leaf functions may use (the red zone).
const RED_ZONE: u64 = 128;

/// The registers in `struct sigcontext` order.
fn context_registers(frame: &InterruptFrame) -> [u64; 18] {
    [
        frame.r8,
        frame.r9,
        frame.r10,
        frame.r11,
        frame.r12,
        frame.r13,
        frame.r14,
        frame.r15,
        frame.rdi,
        frame.rsi,
        frame.rbp,
        frame.rbx,
        frame.rdx,
        frame.rax,
        frame.rcx,
        frame.rsp,
        frame.rip,
        frame.rflags,
    ]
}

fn push_frame(process: &Process, frame: &mut InterruptFrame, signal: u32, action: &SigAction) -> Result<()> {
    let (blocked, alternate, fault_address) = {
        let signals = process.signals.lock();
        (signals.blocked, signals.alternate_stack, signals.fault_address)
    };
    let on_alternate = alternate.is_some_and(|(base, size)| (base..base + size).contains(&frame.rsp));
    let mut top = match alternate {
        Some((base, size)) if action.flags & SA_ONSTACK != 0 && !on_alternate => base + size,
        _ => frame.rsp - RED_ZONE,
    };

    // The FPU registers go first, 64-byte aligned.
    top = (top - size_of::<FpuState>() as u64) & !63;
    let fpu_state = top;
    user::check(fpu_state, size_of::<FpuState>(), true)?;
    // SAFETY: checked writable and aligned; fxsave stores the live (user)
    // FPU state.
    unsafe { fpu::save_to(fpu_state as *mut u8) };

    // Then pretcode, the ucontext and the siginfo, placed so that the stack
    // pointer at the handler's entry looks like it came from a `call`.
    let frame_size = 8 + UCONTEXT_SIZE + SIGINFO_SIZE;
    let frame_address = ((top - frame_size) & !15) - 8;
    let context_address = frame_address + 8;
    let info_address = context_address + UCONTEXT_SIZE;

    let mut bytes: Vec<u8> = Vec::with_capacity(frame_size as usize);
    let mut push = |value: u64| bytes.extend_from_slice(&value.to_le_bytes());
    push(action.restorer);
    // ucontext: flags, link, stack_t (sp, flags + padding, size).
    push(0);
    push(0);
    let (stack_base, stack_size) = alternate.unwrap_or((0, 0));
    push(stack_base);
    push(if alternate.is_some() { 0 } else { 2 });
    push(stack_size);
    // sigcontext.
    for register in context_registers(frame) {
        push(register);
    }
    push(frame.cs | frame.ss << 48);
    push(frame.error_code);
    push(if frame.vector == u64::MAX { 0 } else { frame.vector });
    push(blocked);
    push(fault_address);
    push(fpu_state);
    for _ in 0..8 {
        push(0);
    }
    // uc_sigmask: the mask to restore.
    push(blocked);
    // siginfo: signo, errno, code; then the faulting address or sender.
    push(u64::from(signal));
    let code: u64 = if fault_address != 0 { 1 } else { 0 };
    push(code);
    push(fault_address);
    bytes.resize(frame_size as usize, 0);
    user::write_bytes(frame_address, &bytes)?;

    {
        let mut signals = process.signals.lock();
        signals.blocked |= action.mask;
        if action.flags & SA_NODEFER == 0 {
            signals.blocked |= bit(signal);
        }
        if action.flags & SA_RESETHAND != 0 {
            signals.actions[signal as usize] = SigAction::default();
        }
        signals.fault_address = 0;
    }

    frame.rip = action.handler;
    frame.rsp = frame_address;
    frame.rdi = u64::from(signal);
    frame.rsi = info_address;
    frame.rdx = context_address;
    frame.rax = 0;
    // Clear the trap and direction flags for the handler.
    frame.rflags &= !(0x100 | 0x400);
    // The handler starts with clean FPU registers.
    fpu::restore(&FpuState::initial());
    Ok(())
}

/// User-changeable RFLAGS bits: carry, parity, adjust, zero, sign, trap,
/// direction, overflow, alignment check.
const USER_FLAGS: u64 = 0x0001 | 0x0004 | 0x0010 | 0x0040 | 0x0080 | 0x0100 | 0x0400 | 0x0800 | 0x4_0000;

/// `rt_sigreturn`: restores the registers saved when a handler was entered.
/// The handler's `ret` popped the restorer address, so the stack pointer is
/// at the ucontext.
pub fn sigreturn(process: &Process, frame: &mut InterruptFrame) -> Result<u64> {
    let context = frame.rsp;
    let registers = user::read_bytes(context + 40, 256)?;
    let word = |index: usize| u64::from_le_bytes(registers[index * 8..index * 8 + 8].try_into().unwrap());
    frame.r8 = word(0);
    frame.r9 = word(1);
    frame.r10 = word(2);
    frame.r11 = word(3);
    frame.r12 = word(4);
    frame.r13 = word(5);
    frame.r14 = word(6);
    frame.r15 = word(7);
    frame.rdi = word(8);
    frame.rsi = word(9);
    frame.rbp = word(10);
    frame.rbx = word(11);
    frame.rdx = word(12);
    frame.rax = word(13);
    frame.rcx = word(14);
    frame.rsp = word(15);
    frame.rip = word(16);
    // Only the flags a program may change; interrupts stay enabled.
    frame.rflags = (word(17) & USER_FLAGS) | 0x202;
    frame.cs = u64::from(crate::arch::gdt::USER_CODE);
    frame.ss = u64::from(crate::arch::gdt::USER_DATA);
    let fpu_state = word(23);

    let mask: u64 = user::read_value(context + 40 + 256)?;
    process.signals.lock().blocked = mask & !(bit(SIGKILL) | bit(SIGSTOP));

    if fpu_state != 0 {
        let saved = user::read_bytes(fpu_state, size_of::<FpuState>())?;
        let mut state = FpuState::initial();
        state.load(&saved);
        fpu::restore(&state);
    }
    // The system call returns the restored rax, leaving it unchanged.
    Ok(frame.rax)
}

const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

/// `rt_sigprocmask`.
pub fn set_mask(process: &Process, how: u64, set: Option<u64>) -> Result<u64> {
    let mut signals = process.signals.lock();
    let old = signals.blocked;
    if let Some(set) = set {
        let set = set & !(bit(SIGKILL) | bit(SIGSTOP));
        signals.blocked = match how {
            SIG_BLOCK => old | set,
            SIG_UNBLOCK => old & !set,
            SIG_SETMASK => set,
            _ => return Err(Errno::EINVAL),
        };
    }
    Ok(old)
}

const SS_DISABLE: u64 = 2;

/// `sigaltstack`: returns the old stack as (sp, flags, size).
pub fn set_alternate_stack(process: &Process, new: Option<(u64, u64, u64)>) -> Result<(u64, u64, u64)> {
    let mut signals = process.signals.lock();
    let old = match signals.alternate_stack {
        Some((base, size)) => (base, 0, size),
        None => (0, SS_DISABLE, 0),
    };
    if let Some((base, flags, size)) = new {
        signals.alternate_stack = if flags & SS_DISABLE != 0 {
            None
        } else if size < 2048 {
            return Err(Errno::ENOMEM);
        } else {
            Some((base, size))
        };
    }
    Ok(old)
}

/// Starts the handler for a fault signal raised by user code, or ends the
/// process if it has none.
pub fn fault(process: &Process, frame: &mut InterruptFrame, signal: u32, address: u64) {
    if raise_fault(process, signal, address) {
        deliver(process, frame, None);
    } else {
        crate::arch::enable_interrupts();
        super::exit(super::signal_status(signal));
    }
}
