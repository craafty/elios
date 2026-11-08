//! Signals: sending, masking, default actions, and running handlers.
//!
//! Handlers run the way Linux runs them: just before the kernel returns to
//! user mode, it saves the interrupted registers in a signal frame on the
//! user stack (laid out exactly as Linux's `struct rt_sigframe`, so C
//! libraries understand it) and enters the handler. The handler returns to a
//! "restorer" the C library supplies, which calls `rt_sigreturn`; that
//! reloads the saved registers from the frame.
//!
//! As in POSIX, handlers belong to the process, but each thread has its own
//! mask. A signal sent to the process is taken by whichever thread doesn't
//! block it; one sent to a thread (`tgkill`, or a fault) waits for that
//! thread.

use alloc::vec::Vec;

use super::{Process, UserThread, user};
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
pub const SIGALRM: u32 = 14;
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
pub const SIGSYS: u32 = 31;
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

/// What a handler (or `sigwaitinfo`) is told about a signal beyond its
/// number: `si_errno`, `si_code`, and the two words after them in
/// `siginfo_t` (for a timer: its id and overrun count, then the value it
/// was created with; for seccomp's `SIGSYS`: the call's address, then its
/// number and architecture).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalInfo {
    pub errno: i32,
    pub code: i32,
    pub fields: [u64; 2],
}

/// `si_code` for a signal from a POSIX timer.
pub const SI_TIMER: i32 = -2;

/// The details of pending signals that have any (a signal set has room for
/// just one pending instance of each).
#[derive(Clone, Default)]
struct PendingInfo(Vec<(u32, SignalInfo)>);

impl PendingInfo {
    fn set(&mut self, signal: u32, info: SignalInfo) {
        self.0.retain(|(other, _)| *other != signal);
        if info != SignalInfo::default() {
            self.0.push((signal, info));
        }
    }

    fn take(&mut self, signal: u32) -> SignalInfo {
        match self.0.iter().position(|(other, _)| *other == signal) {
            Some(index) => self.0.remove(index).1,
            None => SignalInfo::default(),
        }
    }
}

/// A process's signal state, shared by its threads.
#[derive(Clone)]
pub struct Signals {
    /// Signals sent to the process as a whole.
    pending: u64,
    info: PendingInfo,
    pub actions: [SigAction; SIGNAL_COUNT],
}

/// A thread's own signal state.
#[derive(Clone, Default)]
pub struct ThreadSignals {
    /// Signals sent to this thread in particular.
    pending: u64,
    blocked: u64,
    /// The alternate signal stack (base, size), if one is set.
    alternate_stack: Option<(u64, u64)>,
    /// For a signal raised by a CPU fault: the faulting address.
    fault_address: u64,
    /// The mask to go back to after the next handler, set while
    /// `rt_sigsuspend` waits with a temporary one.
    saved_mask: Option<u64>,
    info: PendingInfo,
}

impl ThreadSignals {
    /// The state of a forked child's thread: the same mask and alternate
    /// stack.
    pub fn for_child(&self) -> Self {
        Self { pending: 0, fault_address: 0, saved_mask: None, info: PendingInfo::default(), ..self.clone() }
    }

    /// A new thread in the same process: only the mask carries over.
    pub fn for_new_thread(&self) -> Self {
        Self { blocked: self.blocked, ..Self::default() }
    }

    /// After `exec`: the alternate stack was in the old program's memory.
    pub fn exec(&mut self) {
        self.alternate_stack = None;
    }
}

impl Signals {
    pub const fn new() -> Self {
        Self {
            pending: 0,
            info: PendingInfo(Vec::new()),
            actions: [SigAction { handler: SIG_DFL, flags: 0, restorer: 0, mask: 0 }; SIGNAL_COUNT],
        }
    }

    /// The state a forked child starts with: everything but pending signals.
    pub fn for_child(&self) -> Self {
        Self { pending: 0, info: PendingInfo::default(), ..self.clone() }
    }

    /// After `exec`: handlers point into the old program, so they go back to
    /// the default (ignored signals stay ignored).
    pub fn exec(&mut self) {
        for action in &mut self.actions {
            if action.handler != SIG_IGN {
                *action = SigAction::default();
            }
        }
    }

    fn ignored(&self, signal: u32) -> bool {
        match self.actions[signal as usize].handler {
            _ if signal == SIGKILL || signal == SIGSTOP => false,
            SIG_IGN => true,
            SIG_DFL => ignored_by_default(signal),
            _ => false,
        }
    }
}

const UNBLOCKABLE: u64 = 1 << (SIGKILL - 1) | 1 << (SIGSTOP - 1);

/// Pending signals that `thread` can act on now.
fn deliverable(process: &Signals, thread: &ThreadSignals) -> u64 {
    (process.pending | thread.pending) & (!thread.blocked | UNBLOCKABLE)
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

fn valid(signal: u32) -> bool {
    signal != 0 && (signal as usize) < SIGNAL_COUNT
}

/// Sends a signal to a process: marks it pending and wakes a thread that
/// will act on it.
pub fn send(process: &Process, signal: u32) {
    send_with_info(process, signal, SignalInfo::default());
}

pub fn send_with_info(process: &Process, signal: u32, info: SignalInfo) {
    if !valid(signal) || process.status.lock().is_some() {
        return;
    }
    {
        let mut signals = process.signals.lock();
        if signals.ignored(signal) {
            return;
        }
        signals.pending |= bit(signal);
        signals.info.set(signal, info);
    }
    SIGNAL_WAIT.wake_all();
    let threads = process.threads.lock().clone();
    let taker = threads.iter().find(|thread| thread.signals.lock().blocked & bit(signal) & !UNBLOCKABLE == 0);
    if let Some(thread) = taker {
        task::interrupt(thread.id);
    }
}

/// Sends a signal to one thread (`tgkill`).
pub fn send_to_thread(thread: &UserThread, signal: u32) {
    send_to_thread_with_info(thread, signal, SignalInfo::default());
}

pub fn send_to_thread_with_info(thread: &UserThread, signal: u32, info: SignalInfo) {
    if !valid(signal) || thread.process.signals.lock().ignored(signal) {
        return;
    }
    let wake = {
        let mut signals = thread.signals.lock();
        signals.pending |= bit(signal);
        signals.info.set(signal, info);
        signals.blocked & bit(signal) & !UNBLOCKABLE == 0
    };
    // A thread waiting in sigwaitinfo waits for blocked signals too.
    SIGNAL_WAIT.wake_all();
    if wake {
        task::interrupt(thread.id);
    }
}

/// Raises a signal for a CPU fault in the running thread. Unlike other
/// signals it can't be ignored or blocked: if it isn't handled, the process
/// ends.
pub fn raise_fault(thread: &UserThread, signal: u32, address: u64) -> bool {
    let action = thread.process.signals.lock().actions[signal as usize];
    let mut signals = thread.signals.lock();
    let handled = action.handler != SIG_DFL && action.handler != SIG_IGN && signals.blocked & bit(signal) == 0;
    if handled {
        signals.pending |= bit(signal);
        signals.fault_address = address;
    }
    handled
}

/// Raises seccomp's `SIGSYS` for a system call a filter trapped. Like a
/// fault it can't be blocked or ignored: if the program doesn't handle it,
/// the process ends.
pub fn raise_seccomp_trap(thread: &UserThread, info: SignalInfo) {
    let action = thread.process.signals.lock().actions[SIGSYS as usize];
    let mut signals = thread.signals.lock();
    let handled = action.handler != SIG_DFL && action.handler != SIG_IGN && signals.blocked & bit(SIGSYS) == 0;
    if !handled {
        drop(signals);
        crate::arch::enable_interrupts();
        super::exit_group(super::signal_status(SIGSYS));
    }
    signals.pending |= bit(SIGSYS);
    signals.info.set(SIGSYS, info);
}

/// Whether the running thread should stop a blocking system call: a signal
/// needs handling, or the thread is being ended.
pub fn pending() -> bool {
    task::current_user_thread().is_some_and(|thread| {
        super::must_end(&thread) || deliverable(&thread.process.signals.lock(), &thread.signals.lock()) != 0
    })
}

/// Acts on pending signals before returning to user mode: ends the process
/// for signals with a fatal default action, or sets up `frame` to run a
/// handler. `syscall` is the number of the system call being returned from,
/// if any, so calls interrupted by a handler with `SA_RESTART` can resume.
/// Also ends the thread if the process is exiting or another thread is
/// running `execve`.
pub fn deliver(thread: &UserThread, frame: &mut InterruptFrame, syscall: Option<u64>) {
    if super::must_end(thread) {
        crate::arch::enable_interrupts();
        super::exit_thread(0);
    }
    let (signal, action, info) = {
        let mut process = thread.process.signals.lock();
        let mut signals = thread.signals.lock();
        // Pending signals that are ignored by now are dropped.
        let ignored = (1..SIGNAL_COUNT as u32).filter(|&s| process.ignored(s)).fold(0, |m, s| m | bit(s));
        process.pending &= !ignored;
        signals.pending &= !ignored;
        let deliverable = deliverable(&process, &signals);
        if deliverable == 0 {
            // rt_sigsuspend woke for a signal another thread took.
            if let Some(mask) = signals.saved_mask.take() {
                signals.blocked = mask;
            }
            return;
        }
        let signal = deliverable.trailing_zeros() + 1;
        let info = if signals.pending & bit(signal) != 0 {
            signals.pending &= !bit(signal);
            signals.info.take(signal)
        } else {
            process.pending &= !bit(signal);
            process.info.take(signal)
        };
        (signal, process.actions[signal as usize], info)
    };

    if action.handler == SIG_DFL || signal == SIGKILL {
        crate::arch::enable_interrupts();
        super::exit_group(super::signal_status(signal));
    }

    if let Some(number) = syscall
        && frame.rax as i64 == -(Errno::EINTR as i64)
        && action.flags & SA_RESTART != 0
        && !NEVER_RESTARTED.contains(&number)
    {
        // Back up over the `syscall` instruction (2 bytes) to run it again
        // once the handler returns.
        frame.rax = number;
        frame.rip -= 2;
    }

    if push_frame(thread, frame, signal, &action, info).is_err() {
        // No usable stack for the handler: nothing to do but end it.
        crate::arch::enable_interrupts();
        super::exit_group(super::signal_status(SIGSEGV));
    }
}

/// Calls Linux never restarts after a handler, `SA_RESTART` or not: waiting
/// for a signal (pause, sigsuspend, sigtimedwait), sleeping, and waiting
/// with a timeout (poll, select, epoll). They return `EINTR`, which is how
/// the program learns the signal came.
const NEVER_RESTARTED: [u64; 11] = [7, 23, 34, 35, 128, 130, 230, 232, 270, 271, 281];

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

fn push_frame(
    thread: &UserThread,
    frame: &mut InterruptFrame,
    signal: u32,
    action: &SigAction,
    info: SignalInfo,
) -> Result<()> {
    let (blocked, alternate, fault_address) = {
        let mut signals = thread.signals.lock();
        // After rt_sigsuspend, the handler returns to the mask from before.
        let restore = signals.saved_mask.take().unwrap_or(signals.blocked);
        (restore, signals.alternate_stack, signals.fault_address)
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
    // siginfo: signo, errno, code; then the faulting address, or what the
    // sender said.
    bytes.extend_from_slice(&siginfo(signal, info, fault_address));
    bytes.resize(frame_size as usize, 0);
    user::write_bytes(frame_address, &bytes)?;

    if action.flags & SA_RESETHAND != 0 {
        thread.process.signals.lock().actions[signal as usize] = SigAction::default();
    }
    {
        let mut signals = thread.signals.lock();
        signals.blocked |= action.mask & !UNBLOCKABLE;
        if action.flags & SA_NODEFER == 0 {
            signals.blocked |= bit(signal);
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
pub fn sigreturn(thread: &UserThread, frame: &mut InterruptFrame) -> Result<u64> {
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
    thread.signals.lock().blocked = mask & !UNBLOCKABLE;

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
pub fn set_mask(thread: &UserThread, how: u64, set: Option<u64>) -> Result<u64> {
    let mut signals = thread.signals.lock();
    let old = signals.blocked;
    if let Some(set) = set {
        let set = set & !UNBLOCKABLE;
        signals.blocked = match how {
            SIG_BLOCK => old | set,
            SIG_UNBLOCK => old & !set,
            SIG_SETMASK => set,
            _ => return Err(Errno::EINVAL),
        };
    }
    Ok(old)
}

/// Where `pause` and `rt_sigsuspend` wait: nothing wakes it but signals.
static SIGNAL_WAIT: task::WaitQueue = task::WaitQueue::new();

/// The first 32 bytes of a `siginfo_t` (the rest is zeros).
fn siginfo(signal: u32, info: SignalInfo, fault_address: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&signal.to_le_bytes());
    if fault_address != 0 {
        // SEGV_MAPERR and the address.
        bytes[8..12].copy_from_slice(&1i32.to_le_bytes());
        bytes[16..24].copy_from_slice(&fault_address.to_le_bytes());
    } else {
        bytes[4..8].copy_from_slice(&info.errno.to_le_bytes());
        bytes[8..12].copy_from_slice(&info.code.to_le_bytes());
        bytes[16..24].copy_from_slice(&info.fields[0].to_le_bytes());
        bytes[24..32].copy_from_slice(&info.fields[1].to_le_bytes());
    }
    bytes
}

/// `rt_sigtimedwait` (`sigwaitinfo`, `sigtimedwait`): waits until one of
/// the signals in `set` is pending, takes it without running a handler, and
/// returns it with its details. Blocked signals count (that's the usual
/// way to use it). Gives up with `EAGAIN` at `deadline` (in timer ticks),
/// or `EINTR` if another signal needs handling.
pub fn wait_for(thread: &UserThread, set: u64, deadline: Option<u64>) -> Result<(u32, [u8; 32])> {
    let set = set & !UNBLOCKABLE;
    let taken = SIGNAL_WAIT.wait_until_deadline(deadline, || {
        let mut process = thread.process.signals.lock();
        let mut signals = thread.signals.lock();
        let waiting = (signals.pending | process.pending) & set;
        if waiting != 0 {
            let signal = waiting.trailing_zeros() + 1;
            let info = if signals.pending & bit(signal) != 0 {
                signals.pending &= !bit(signal);
                signals.info.take(signal)
            } else {
                process.pending &= !bit(signal);
                process.info.take(signal)
            };
            return Some(Ok((signal, siginfo(signal, info, 0))));
        }
        drop(signals);
        drop(process);
        pending().then_some(Err(Errno::EINTR))
    });
    taken.unwrap_or(Err(Errno::EAGAIN))
}

/// `pause`: waits for a signal. Always returns `EINTR`.
pub fn pause() -> Result<u64> {
    SIGNAL_WAIT.wait_until(|| pending().then_some(()));
    Err(Errno::EINTR)
}

/// `rt_sigsuspend`: waits for a signal with `mask` as the thread's mask;
/// the old mask comes back once the handler returns.
pub fn suspend(thread: &UserThread, mask: u64) -> Result<u64> {
    {
        let mut signals = thread.signals.lock();
        let old = signals.blocked;
        signals.saved_mask = Some(old);
        signals.blocked = mask & !UNBLOCKABLE;
    }
    pause()
}

const SS_DISABLE: u64 = 2;

/// `sigaltstack`: returns the old stack as (sp, flags, size).
pub fn set_alternate_stack(thread: &UserThread, new: Option<(u64, u64, u64)>) -> Result<(u64, u64, u64)> {
    let mut signals = thread.signals.lock();
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
pub fn fault(thread: &UserThread, frame: &mut InterruptFrame, signal: u32, address: u64) {
    if raise_fault(thread, signal, address) {
        deliver(thread, frame, None);
    } else {
        crate::arch::enable_interrupts();
        super::exit_group(super::signal_status(signal));
    }
}
