//! Processes: creating, running, waiting for, and signaling them.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sys::{self, Error, Result};

pub fn exit(code: i32) -> ! {
    // SAFETY: exit_group never returns.
    unsafe { sys::syscall(sys::EXIT_GROUP, [code as usize, 0, 0, 0, 0, 0]) };
    unreachable!()
}

pub fn id() -> u32 {
    // SAFETY: takes no arguments.
    unsafe { sys::syscall(sys::GETPID, [0; 6]) as u32 }
}

pub fn parent_id() -> u32 {
    // SAFETY: takes no arguments.
    unsafe { sys::syscall(sys::GETPPID, [0; 6]) as u32 }
}

/// Splits the process in two. Returns the child's pid in the parent and 0 in
/// the child.
pub fn fork() -> Result<u32> {
    // SAFETY: takes no arguments.
    unsafe { sys::call(sys::FORK, [0; 6]) }.map(|pid| pid as u32)
}

fn c_strings(strings: &[String]) -> (Vec<Vec<u8>>, Vec<usize>) {
    let owned: Vec<Vec<u8>> = strings
        .iter()
        .map(|s| {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            bytes
        })
        .collect();
    let mut pointers: Vec<usize> = owned.iter().map(|s| s.as_ptr() as usize).collect();
    pointers.push(0);
    (owned, pointers)
}

/// Replaces this program with another. Only returns if that fails.
pub fn exec(path: &str, arguments: &[String], environment: &[String]) -> Error {
    let mut c_path = path.as_bytes().to_vec();
    c_path.push(0);
    let (_arguments, argument_pointers) = c_strings(arguments);
    let (_environment, environment_pointers) = c_strings(environment);
    // SAFETY: all strings are NUL-terminated and the arrays NULL-terminated,
    // and they live until the call.
    let result = unsafe {
        sys::call(
            sys::EXECVE,
            [
                c_path.as_ptr() as usize,
                argument_pointers.as_ptr() as usize,
                environment_pointers.as_ptr() as usize,
                0,
                0,
                0,
            ],
        )
    };
    result.err().unwrap_or(Error::INVALID)
}

/// How a process ended, as reported by `wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub fn code(self) -> Option<i32> {
        (self.0 & 0x7F == 0).then_some((self.0 >> 8) & 0xFF)
    }

    pub fn signal(self) -> Option<i32> {
        (self.0 & 0x7F != 0).then_some(self.0 & 0x7F)
    }

    pub fn success(self) -> bool {
        self.code() == Some(0)
    }

    /// The value a shell reports in `$?`.
    pub fn shell_code(self) -> i32 {
        self.code().unwrap_or_else(|| 128 + self.signal().unwrap_or(0))
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match (self.code(), self.signal()) {
            (Some(code), _) => write!(f, "exit code {code}"),
            (_, Some(signal)) => write!(f, "{}", signal_description(signal)),
            _ => write!(f, "status {:#x}", self.0),
        }
    }
}

/// Waits for a child (`pid`, or any child for -1). With `no_hang`, returns
/// `None` if none has exited yet.
pub fn wait(pid: i32, no_hang: bool) -> Result<Option<(u32, ExitStatus)>> {
    let mut status = 0i32;
    // SAFETY: room for the status.
    let result =
        unsafe { sys::call(sys::WAIT4, [pid as usize, &raw mut status as usize, usize::from(no_hang), 0, 0, 0]) }?;
    Ok((result != 0).then_some((result as u32, ExitStatus(status))))
}

pub fn kill(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: takes no pointers.
    unsafe { sys::call(sys::KILL, [pid as usize, signal as usize, 0, 0, 0, 0]) }.map(|_| ())
}

/// Moves a process (0 for this one) into a process group (0 for its own).
pub fn set_group(pid: u32, group: u32) -> Result<()> {
    // SAFETY: takes no pointers.
    unsafe { sys::call(sys::SETPGID, [pid as usize, group as usize, 0, 0, 0, 0]) }.map(|_| ())
}

fn set_signal_action(signal: i32, action: usize) {
    let action = [action, 0, 0, 0];
    // SAFETY: the action is a valid `struct sigaction`.
    let _ = unsafe { sys::call(sys::RT_SIGACTION, [signal as usize, action.as_ptr() as usize, 0, 8, 0, 0]) };
}

pub fn ignore_signal(signal: i32) {
    set_signal_action(signal, 1);
}

pub fn default_signal(signal: i32) {
    set_signal_action(signal, 0);
}

pub const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;
pub const SIGQUIT: i32 = 3;

pub fn signal_number(name: &str) -> Option<i32> {
    let name = name.trim_start_matches("SIG");
    SIGNALS.iter().find(|(_, n)| *n == name).map(|(number, _)| *number).or_else(|| name.parse().ok())
}

const SIGNALS: [(i32, &str); 15] = [
    (1, "HUP"),
    (2, "INT"),
    (3, "QUIT"),
    (4, "ILL"),
    (5, "TRAP"),
    (6, "ABRT"),
    (7, "BUS"),
    (8, "FPE"),
    (9, "KILL"),
    (11, "SEGV"),
    (13, "PIPE"),
    (14, "ALRM"),
    (15, "TERM"),
    (10, "USR1"),
    (12, "USR2"),
];

pub fn signal_description(signal: i32) -> String {
    match signal {
        2 => "interrupted".into(),
        9 => "killed".into(),
        11 => "segmentation fault".into(),
        15 => "terminated".into(),
        _ => match SIGNALS.iter().find(|(number, _)| *number == signal) {
            Some((_, name)) => alloc::format!("killed by SIG{name}"),
            None => alloc::format!("killed by signal {signal}"),
        },
    }
}

/// A process, as `ps` shows it.
pub struct ProcessInfo {
    pub pid: u32,
    pub parent: u32,
    pub state: char,
    pub cpu_ms: u64,
    pub memory: u64,
    pub name: String,
}

/// Every process on the system (an eliOS system call).
pub fn list() -> Result<Vec<ProcessInfo>> {
    const ENTRY: usize = 64;
    let mut buffer = alloc::vec![0u8; ENTRY * 256];
    // SAFETY: room for 256 entries.
    let count = unsafe { sys::call(sys::ELI_PROCESS_LIST, [buffer.as_mut_ptr() as usize, 256, 0, 0, 0, 0]) }?;
    Ok(buffer
        .chunks(ENTRY)
        .take(count)
        .map(|entry| {
            let u32_at = |at: usize| u32::from_le_bytes(entry[at..at + 4].try_into().unwrap());
            let u64_at = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
            let name = &entry[32..64];
            let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(32)];
            ProcessInfo {
                pid: u32_at(0),
                parent: u32_at(4),
                state: entry[8] as char,
                cpu_ms: u64_at(16),
                memory: u64_at(24),
                name: String::from_utf8_lossy(name).into_owned(),
            }
        })
        .collect())
}

/// The kernel's log (an eliOS system call).
pub fn kernel_log() -> Result<String> {
    let mut buffer = alloc::vec![0u8; 64 * 1024];
    // SAFETY: the buffer is valid for its length.
    let len = unsafe { sys::call(sys::ELI_KERNEL_LOG, [buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0, 0]) }?;
    Ok(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

const REBOOT_MAGIC1: usize = 0xFEE1_DEAD;
const REBOOT_MAGIC2: usize = 0x2812_1969;

pub fn reboot() -> Error {
    // SAFETY: takes no pointers.
    let result = unsafe { sys::call(sys::REBOOT, [REBOOT_MAGIC1, REBOOT_MAGIC2, 0x0123_4567, 0, 0, 0]) };
    result.err().unwrap_or(Error::INVALID)
}

pub fn power_off() -> Error {
    // SAFETY: takes no pointers.
    let result = unsafe { sys::call(sys::REBOOT, [REBOOT_MAGIC1, REBOOT_MAGIC2, 0x4321_FEDC, 0, 0, 0]) };
    result.err().unwrap_or(Error::INVALID)
}
