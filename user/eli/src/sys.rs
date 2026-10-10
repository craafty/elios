//! Raw system calls (the Linux x86_64 ABI) and error numbers.

use core::arch::asm;
use core::fmt;

pub const READ: usize = 0;
pub const WRITE: usize = 1;
pub const CLOSE: usize = 3;
pub const FSTAT: usize = 5;
pub const LSEEK: usize = 8;
pub const MMAP: usize = 9;
pub const MUNMAP: usize = 11;
pub const RT_SIGACTION: usize = 13;
pub const IOCTL: usize = 16;
pub const PIPE2: usize = 293;
pub const DUP2: usize = 33;
pub const NANOSLEEP: usize = 35;
pub const GETPID: usize = 39;
pub const FORK: usize = 57;
pub const EXECVE: usize = 59;
pub const EXIT_GROUP: usize = 231;
pub const WAIT4: usize = 61;
pub const KILL: usize = 62;
pub const UNAME: usize = 63;
pub const GETCWD: usize = 79;
pub const CHDIR: usize = 80;
pub const SYSINFO: usize = 99;
pub const GETPPID: usize = 110;
pub const SETPGID: usize = 109;
pub const REBOOT: usize = 169;
pub const GETDENTS64: usize = 217;
pub const CLOCK_GETTIME: usize = 228;
pub const OPENAT: usize = 257;
pub const MKDIRAT: usize = 258;
pub const NEWFSTATAT: usize = 262;
pub const UNLINKAT: usize = 263;
pub const RENAMEAT: usize = 264;
pub const SYMLINKAT: usize = 266;
pub const READLINKAT: usize = 267;
pub const GETRANDOM: usize = 318;
/// eliOS's own calls.
pub const ELI_PROCESS_LIST: usize = 0x1000;
pub const ELI_KERNEL_LOG: usize = 0x1001;
pub const ELI_TRACE: usize = 0x1002;

pub const AT_FDCWD: usize = -100isize as usize;

/// Makes a system call with up to six arguments.
///
/// # Safety
/// The arguments must be valid for the call (pointers to suitable memory
/// and so on).
#[inline]
pub unsafe fn syscall(number: usize, args: [usize; 6]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Turns a raw result into `Ok(value)` or `Err(errno)`.
pub fn check(result: isize) -> Result<usize> {
    if (-4095..0).contains(&result) { Err(Error(-result as i32)) } else { Ok(result as usize) }
}

/// Shorthand: a system call checked for errors.
///
/// # Safety
/// As for [`syscall`].
pub unsafe fn call(number: usize, args: [usize; 6]) -> Result<usize> {
    check(unsafe { syscall(number, args) })
}

/// An error number from a failed system call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub i32);

impl Error {
    pub const NOT_FOUND: Error = Error(2);
    pub const INTERRUPTED: Error = Error(4);
    pub const BAD_FILE: Error = Error(9);
    pub const CHILD: Error = Error(10);
    pub const EXISTS: Error = Error(17);
    pub const NOT_DIRECTORY: Error = Error(20);
    pub const IS_DIRECTORY: Error = Error(21);
    pub const INVALID: Error = Error(22);

    pub fn description(self) -> &'static str {
        match self.0 {
            1 => "operation not permitted",
            2 => "no such file or directory",
            3 => "no such process",
            4 => "interrupted",
            5 => "input/output error",
            7 => "argument list too long",
            8 => "not an executable",
            9 => "bad file descriptor",
            10 => "no child processes",
            11 => "try again",
            12 => "out of memory",
            13 => "permission denied",
            14 => "bad address",
            17 => "file exists",
            20 => "not a directory",
            21 => "is a directory",
            22 => "invalid argument",
            24 => "too many open files",
            25 => "not a terminal",
            27 => "file too large",
            28 => "no space left",
            29 => "illegal seek",
            32 => "broken pipe",
            36 => "file name too long",
            38 => "function not implemented",
            39 => "directory not empty",
            40 => "too many symbolic links",
            _ => "unknown error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

pub type Result<T> = core::result::Result<T, Error>;
