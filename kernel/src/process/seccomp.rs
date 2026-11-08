//! seccomp: a thread limits the system calls it (and everything it
//! starts) may make. Sandboxes depend on it: Firefox's content, media and
//! network processes install filters before touching web content, and
//! refuse to run plugins on systems without it.
//!
//! A filter is a classic BPF program (the packet filter language) run on
//! every system call over a [`seccomp_data`]-shaped record of the call: its
//! number, the architecture, the instruction pointer and the six arguments.
//! It returns an action: allow the call, fail it with an errno, raise
//! `SIGSYS` (sandboxes catch that to handle a call some other way), or end
//! the thread or process. Filters stack: each new one is added in front of
//! those already there, they all run, and the strictest answer wins. Once
//! installed, a filter can't be taken off; threads and children inherit it,
//! and it stays across `execve`.
//!
//! Strict mode, the original seccomp, allows just `read`, `write`, `exit`
//! and `rt_sigreturn`.
//!
//! [`seccomp_data`]: https://man7.org/linux/man-pages/man2/seccomp.2.html

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::UserThread;
use crate::errno::{Errno, Result};

/// `AUDIT_ARCH_X86_64`, the `arch` every call reports.
pub const ARCH_X86_64: u32 = 0xC000_003E;

// Actions, in the top 16 bits of a filter's result; the low 16 are data.
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_USER_NOTIF: u32 = 0x7FC0_0000;
const RET_TRACE: u32 = 0x7FF0_0000;
const RET_LOG: u32 = 0x7FFC_0000;
const RET_ALLOW: u32 = 0x7FFF_0000;
const RET_ACTION_FULL: u32 = 0xFFFF_0000;
const RET_DATA: u32 = 0x0000_FFFF;

// `seccomp` operations and flags.
const SET_MODE_STRICT: u64 = 0;
const SET_MODE_FILTER: u64 = 1;
const GET_ACTION_AVAIL: u64 = 2;
const FLAG_TSYNC: u64 = 1;
const FLAG_LOG: u64 = 2;
const FLAG_SPEC_ALLOW: u64 = 4;
const FLAG_TSYNC_ESRCH: u64 = 16;

/// Linux's limits: instructions in one program, and in all of a thread's
/// filters together (each counting 4 extra).
const MAX_INSTRUCTIONS: usize = 4096;
const MAX_PATH_INSTRUCTIONS: usize = 32768;

/// The size of the record a filter reads.
const DATA_SIZE: u32 = 64;

/// One classic BPF instruction (`struct sock_filter`).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Instruction {
    code: u16,
    jump_true: u8,
    jump_false: u8,
    k: u32,
}

/// An installed filter and the ones installed before it.
pub struct Filter {
    program: Vec<Instruction>,
    /// Calls this filter allows with `SECCOMP_RET_LOG` are logged
    /// (`SECCOMP_FILTER_FLAG_LOG`).
    log: bool,
    previous: Option<Arc<Filter>>,
    /// Instructions in this filter and all before it, for the limit.
    path_length: usize,
}

/// A thread's seccomp state.
#[derive(Clone, Default)]
pub enum Mode {
    #[default]
    Disabled,
    Strict,
    Filter(Arc<Filter>),
}

impl Mode {
    /// The number `PR_GET_SECCOMP` and /proc/PID/status report.
    pub fn number(&self) -> u64 {
        match self {
            Mode::Disabled => 0,
            Mode::Strict => 1,
            Mode::Filter(_) => 2,
        }
    }
}

/// What to do with a system call.
pub enum Verdict {
    Allow,
    /// Skip the call and return this (an `-errno`) instead.
    Return(u64),
    /// Skip the call and raise `SIGSYS` with this as `si_errno`.
    Trap(u16),
    KillThread,
    /// End the process as if by this signal (strict mode's is `SIGKILL`).
    KillProcess(u32),
}

/// Runs the running thread's filters over a system call.
pub fn check(thread: &UserThread, number: u64, arguments: &[u64; 6], rip: u64) -> Verdict {
    let mode = thread.seccomp.lock().clone();
    let filter = match mode {
        Mode::Disabled => return Verdict::Allow,
        Mode::Strict => {
            return match number {
                0 | 1 | 15 | 60 => Verdict::Allow,
                _ => Verdict::KillProcess(super::signal::SIGKILL),
            };
        }
        Mode::Filter(filter) => filter,
    };
    let mut data = [0u8; DATA_SIZE as usize];
    data[0..4].copy_from_slice(&(number as u32).to_le_bytes());
    data[4..8].copy_from_slice(&ARCH_X86_64.to_le_bytes());
    data[8..16].copy_from_slice(&rip.to_le_bytes());
    for (index, argument) in arguments.iter().enumerate() {
        data[16 + index * 8..24 + index * 8].copy_from_slice(&argument.to_le_bytes());
    }
    // The strictest action wins: actions compare as signed numbers, so
    // KILL_PROCESS (negative) beats everything, ALLOW nothing. On a tie the
    // newest filter's data counts.
    let action = |value: u32| (value & RET_ACTION_FULL) as i32;
    let mut result = run(&filter.program, &data);
    let mut log = filter.log;
    let mut next = filter.previous.as_ref();
    while let Some(filter) = next {
        let value = run(&filter.program, &data);
        if action(value) < action(result) {
            result = value;
            log = filter.log;
        }
        next = filter.previous.as_ref();
    }
    let data = (result & RET_DATA) as u16;
    match result & RET_ACTION_FULL {
        RET_ALLOW => Verdict::Allow,
        RET_LOG => {
            if log {
                crate::println!("seccomp: pid {} allowed system call {number} (logged)", thread.process.pid);
            }
            Verdict::Allow
        }
        RET_ERRNO => Verdict::Return((-i64::from(data.min(4095))) as u64),
        RET_TRAP => Verdict::Trap(data),
        // Nothing traces or listens to calls here, so these calls fail as
        // they do on Linux without a tracer or listener.
        RET_TRACE | RET_USER_NOTIF => Verdict::Return((-(Errno::ENOSYS as i64)) as u64),
        RET_KILL_THREAD => Verdict::KillThread,
        _ => Verdict::KillProcess(super::signal::SIGSYS),
    }
}

// Classic BPF instruction fields.
const CLASS_LD: u16 = 0x00;
const CLASS_LDX: u16 = 0x01;
const CLASS_ST: u16 = 0x02;
const CLASS_STX: u16 = 0x03;
const CLASS_ALU: u16 = 0x04;
const CLASS_JMP: u16 = 0x05;
const CLASS_RET: u16 = 0x06;
const CLASS_MISC: u16 = 0x07;
const SIZE_W: u16 = 0x00;
const MODE_IMM: u16 = 0x00;
const MODE_ABS: u16 = 0x20;
const MODE_MEM: u16 = 0x60;
const MODE_LEN: u16 = 0x80;
const SOURCE_X: u16 = 0x08;
const ALU_ADD: u16 = 0x00;
const ALU_SUB: u16 = 0x10;
const ALU_MUL: u16 = 0x20;
const ALU_DIV: u16 = 0x30;
const ALU_OR: u16 = 0x40;
const ALU_AND: u16 = 0x50;
const ALU_LSH: u16 = 0x60;
const ALU_RSH: u16 = 0x70;
const ALU_NEG: u16 = 0x80;
const ALU_MOD: u16 = 0x90;
const ALU_XOR: u16 = 0xA0;
const JMP_JA: u16 = 0x00;
const JMP_JEQ: u16 = 0x10;
const JMP_JGT: u16 = 0x20;
const JMP_JGE: u16 = 0x30;
const JMP_JSET: u16 = 0x40;
const RET_SOURCE_A: u16 = 0x10;
const MISC_TAX: u16 = 0x00;
const MISC_TXA: u16 = 0x80;
/// Scratch memory words (`M[]`).
const MEMORY_WORDS: u32 = 16;

/// Checks a program the way Linux does before accepting it: only the
/// instructions seccomp allows, loads aligned and inside the record, jumps
/// inside the program (they only go forward, so it always ends), no
/// division by a constant 0, and a return at the end.
fn validate(program: &[Instruction]) -> Result<()> {
    if program.is_empty() || program.len() > MAX_INSTRUCTIONS {
        return Err(Errno::EINVAL);
    }
    for (pc, instruction) in program.iter().enumerate() {
        let code = instruction.code;
        let k = instruction.k;
        let remaining = (program.len() - pc - 1) as u32;
        let valid = match code & 0x07 {
            CLASS_LD | CLASS_LDX => match code & !0x07 {
                c if c == SIZE_W | MODE_ABS => code & 0x07 == CLASS_LD && k % 4 == 0 && k < DATA_SIZE,
                c if c == SIZE_W | MODE_LEN || c == SIZE_W | MODE_IMM => true,
                c if c == SIZE_W | MODE_MEM => k < MEMORY_WORDS,
                _ => false,
            },
            CLASS_ST | CLASS_STX => code & !0x07 == 0 && k < MEMORY_WORDS,
            CLASS_ALU => match code & 0xF0 {
                ALU_NEG => code & SOURCE_X == 0,
                ALU_DIV | ALU_MOD if code & SOURCE_X == 0 => k != 0,
                ALU_LSH | ALU_RSH if code & SOURCE_X == 0 => k < 32,
                ALU_ADD | ALU_SUB | ALU_MUL | ALU_DIV | ALU_MOD | ALU_OR | ALU_AND | ALU_LSH | ALU_RSH | ALU_XOR => {
                    true
                }
                _ => false,
            },
            CLASS_JMP => match code & 0xF0 {
                JMP_JA => code & SOURCE_X == 0 && k < remaining,
                JMP_JEQ | JMP_JGT | JMP_JGE | JMP_JSET => {
                    u32::from(instruction.jump_true) < remaining && u32::from(instruction.jump_false) < remaining
                }
                _ => false,
            },
            CLASS_RET => matches!(code & 0x18, 0 | RET_SOURCE_A) && code & !0x1F == 0,
            CLASS_MISC => matches!(code & 0xF8, MISC_TAX | MISC_TXA),
            _ => false,
        };
        if !valid {
            return Err(Errno::EINVAL);
        }
    }
    if program.last().unwrap().code & 0x07 != CLASS_RET {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// Runs a validated program over a system call record.
fn run(program: &[Instruction], data: &[u8; DATA_SIZE as usize]) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut memory = [0u32; MEMORY_WORDS as usize];
    let mut pc = 0;
    loop {
        let Instruction { code, jump_true, jump_false, k } = program[pc];
        pc += 1;
        match code & 0x07 {
            CLASS_LD | CLASS_LDX => {
                let value = match code & 0xE0 {
                    MODE_ABS => {
                        let at = k as usize;
                        u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
                    }
                    MODE_LEN => DATA_SIZE,
                    MODE_MEM => memory[k as usize],
                    _ => k,
                };
                if code & 0x07 == CLASS_LD {
                    a = value;
                } else {
                    x = value;
                }
            }
            CLASS_ST => memory[k as usize] = a,
            CLASS_STX => memory[k as usize] = x,
            CLASS_ALU => {
                let operand = if code & SOURCE_X != 0 { x } else { k };
                a = match code & 0xF0 {
                    ALU_ADD => a.wrapping_add(operand),
                    ALU_SUB => a.wrapping_sub(operand),
                    ALU_MUL => a.wrapping_mul(operand),
                    // Dividing by an X of 0 ends the program with 0, as in
                    // Linux.
                    ALU_DIV | ALU_MOD if operand == 0 => return 0,
                    ALU_DIV => a / operand,
                    ALU_MOD => a % operand,
                    ALU_OR => a | operand,
                    ALU_AND => a & operand,
                    ALU_LSH => a.wrapping_shl(operand),
                    ALU_RSH => a.wrapping_shr(operand),
                    ALU_NEG => a.wrapping_neg(),
                    _ => a ^ operand,
                };
            }
            CLASS_JMP => {
                let operand = if code & SOURCE_X != 0 { x } else { k };
                let taken = match code & 0xF0 {
                    JMP_JA => {
                        pc += k as usize;
                        continue;
                    }
                    JMP_JEQ => a == operand,
                    JMP_JGT => a > operand,
                    JMP_JGE => a >= operand,
                    _ => a & operand != 0,
                };
                pc += usize::from(if taken { jump_true } else { jump_false });
            }
            CLASS_RET => return if code & 0x18 == RET_SOURCE_A { a } else { k },
            _ => {
                if code & 0xF8 == MISC_TAX {
                    x = a;
                } else {
                    a = x;
                }
            }
        }
    }
}

/// Reads a `struct sock_fprog` and the program it points to.
fn read_program(address: u64) -> Result<Vec<Instruction>> {
    let header: [u8; 16] = super::user::read_value(address)?;
    let length = usize::from(u16::from_le_bytes([header[0], header[1]]));
    let pointer = u64::from_le_bytes(header[8..16].try_into().unwrap());
    if length == 0 || length > MAX_INSTRUCTIONS {
        return Err(Errno::EINVAL);
    }
    let bytes = super::user::read_bytes(pointer, length * 8)?;
    let program: Vec<Instruction> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| Instruction {
            code: u16::from_le_bytes([b[0], b[1]]),
            jump_true: b[2],
            jump_false: b[3],
            k: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
        .collect();
    Ok(program)
}

/// The `seccomp` system call (and `prctl(PR_SET_SECCOMP)`).
pub fn seccomp(operation: u64, flags: u64, arguments: u64) -> Result<u64> {
    let thread = super::current_thread();
    match operation {
        SET_MODE_STRICT => {
            if flags != 0 || arguments != 0 {
                return Err(Errno::EINVAL);
            }
            let mut mode = thread.seccomp.lock();
            if matches!(*mode, Mode::Filter(_)) {
                return Err(Errno::EINVAL);
            }
            *mode = Mode::Strict;
            Ok(0)
        }
        SET_MODE_FILTER => {
            if flags & !(FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW | FLAG_TSYNC_ESRCH) != 0 {
                return Err(Errno::EINVAL);
            }
            let program = read_program(arguments)?;
            // Without no_new_privs, a filter could make a setuid program
            // misbehave in its owner's favor; only root may do that.
            let allowed = thread.process.no_new_privileges.load(core::sync::atomic::Ordering::Relaxed)
                || super::credentials::current().euid == 0;
            if !allowed {
                return Err(Errno::EACCES);
            }
            validate(&program)?;
            install(&thread, program, flags)
        }
        GET_ACTION_AVAIL => {
            if flags != 0 {
                return Err(Errno::EINVAL);
            }
            let action: u32 = super::user::read_value(arguments)?;
            match action {
                RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_TRACE | RET_LOG | RET_ALLOW => Ok(0),
                _ => Err(Errno::EOPNOTSUPP),
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

/// Puts a new filter in front of the running thread's (with
/// `SECCOMP_FILTER_FLAG_TSYNC`, every thread's of the process: allowed
/// only if each thread's filters are among the caller's, so none loses a
/// filter it had).
fn install(thread: &Arc<UserThread>, program: Vec<Instruction>, flags: u64) -> Result<u64> {
    let threads = thread.process.threads.lock().clone();
    let current = thread.seccomp.lock().clone();
    let previous = match current {
        Mode::Strict => return Err(Errno::EINVAL),
        Mode::Disabled => None,
        Mode::Filter(filter) => Some(filter),
    };
    let path_length = previous.as_ref().map_or(0, |p| p.path_length) + program.len() + 4;
    if path_length > MAX_PATH_INSTRUCTIONS {
        return Err(Errno::ENOMEM);
    }
    let filter = Arc::new(Filter { program, log: flags & FLAG_LOG != 0, previous: previous.clone(), path_length });
    if flags & FLAG_TSYNC == 0 {
        *thread.seccomp.lock() = Mode::Filter(filter);
        return Ok(0);
    }
    // Every thread's filter must be the caller's or one of its ancestors.
    let ancestor = |mode: &Mode| match mode {
        Mode::Disabled => true,
        Mode::Strict => false,
        Mode::Filter(other) => {
            let mut next = previous.as_ref();
            while let Some(filter) = next {
                if Arc::ptr_eq(filter, other) {
                    return true;
                }
                next = filter.previous.as_ref();
            }
            false
        }
    };
    let mut locks: Vec<_> = threads.iter().map(|other| other.seccomp.lock()).collect();
    for (other, mode) in threads.iter().zip(&locks) {
        if !Arc::ptr_eq(other, thread) && !ancestor(mode) {
            return if flags & FLAG_TSYNC_ESRCH != 0 { Err(Errno::ESRCH) } else { Ok(u64::from(other.tid())) };
        }
    }
    for mode in &mut locks {
        **mode = Mode::Filter(filter.clone());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statement(code: u16, k: u32) -> Instruction {
        Instruction { code, jump_true: 0, jump_false: 0, k }
    }

    fn jump(code: u16, k: u32, jump_true: u8, jump_false: u8) -> Instruction {
        Instruction { code, jump_true, jump_false, k }
    }

    fn record(number: u32, argument: u64) -> [u8; 64] {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&number.to_le_bytes());
        data[4..8].copy_from_slice(&ARCH_X86_64.to_le_bytes());
        data[16..24].copy_from_slice(&argument.to_le_bytes());
        data
    }

    crate::ktest! {
        fn seccomp_filters_run_and_validate() {
            // Load nr; if 39 (getpid) return ERRNO|1, else ALLOW.
            let program = [
                statement(CLASS_LD | SIZE_W | MODE_ABS, 0),
                jump(CLASS_JMP | JMP_JEQ, 39, 0, 1),
                statement(CLASS_RET, RET_ERRNO | 1),
                statement(CLASS_RET, RET_ALLOW),
            ];
            assert!(validate(&program).is_ok());
            assert_eq!(run(&program, &record(39, 0)), RET_ERRNO | 1);
            assert_eq!(run(&program, &record(40, 0)), RET_ALLOW);

            // Arithmetic, scratch memory and X: (arg0 & 0xff) * 2 via M[3].
            let program = [
                statement(CLASS_LD | SIZE_W | MODE_ABS, 16),
                statement(CLASS_ALU | ALU_AND, 0xFF),
                statement(CLASS_ST, 3),
                statement(CLASS_LDX | SIZE_W | MODE_MEM, 3),
                statement(CLASS_ALU | ALU_ADD | SOURCE_X, 0),
                statement(CLASS_RET | RET_SOURCE_A, 0),
            ];
            assert!(validate(&program).is_ok());
            assert_eq!(run(&program, &record(0, 0x1234)), 0x68);

            // Rejected: misaligned or out-of-range loads, jumps past the
            // end, division by constant 0, no return at the end.
            assert!(validate(&[statement(CLASS_LD | SIZE_W | MODE_ABS, 2), statement(CLASS_RET, 0)]).is_err());
            assert!(validate(&[statement(CLASS_LD | SIZE_W | MODE_ABS, 64), statement(CLASS_RET, 0)]).is_err());
            assert!(validate(&[jump(CLASS_JMP | JMP_JEQ, 0, 1, 0), statement(CLASS_RET, 0)]).is_err());
            assert!(validate(&[statement(CLASS_ALU | ALU_DIV, 0), statement(CLASS_RET, 0)]).is_err());
            assert!(validate(&[statement(CLASS_LD | SIZE_W | MODE_IMM, 0)]).is_err());
            assert!(validate(&[]).is_err());
        }
    }
}
