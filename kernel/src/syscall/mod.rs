//! System calls.
//!
//! eliOS implements the Linux x86_64 system call ABI: the same numbers, the
//! same registers (number in rax, arguments in rdi, rsi, rdx, r10, r8, r9),
//! and errors returned as negated errno values. Programs built for Linux
//! that stick to the calls implemented here run unmodified.
//!
//! eliOS's own calls (for things Linux does through /proc) are numbered
//! from 0x1000 up, far from Linux's.

mod files;
mod poll;
mod processes;
mod system;

pub use self::system::HOSTNAME;

use crate::arch::interrupts::InterruptFrame;
use crate::errno::{Errno, Result};
use crate::process::signal;
use crate::{arch, process};

/// eliOS-specific calls.
pub const ELI_PROCESS_LIST: u64 = 0x1000;
pub const ELI_KERNEL_LOG: u64 = 0x1001;
pub const ELI_TRACE: u64 = 0x1002;

/// System call names, for tracing.
fn name(number: u64) -> Option<&'static str> {
    Some(match number {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        4 => "stat",
        5 => "fstat",
        6 => "lstat",
        7 => "poll",
        8 => "lseek",
        9 => "mmap",
        10 => "mprotect",
        11 => "munmap",
        12 => "brk",
        13 => "rt_sigaction",
        14 => "rt_sigprocmask",
        15 => "rt_sigreturn",
        16 => "ioctl",
        17 => "pread64",
        18 => "pwrite64",
        19 => "readv",
        20 => "writev",
        21 => "access",
        22 => "pipe",
        23 => "select",
        24 => "sched_yield",
        25 => "mremap",
        28 => "madvise",
        32 => "dup",
        33 => "dup2",
        34 => "pause",
        35 => "nanosleep",
        39 => "getpid",
        40 => "sendfile",
        56 => "clone",
        57 => "fork",
        58 => "vfork",
        59 => "execve",
        60 => "exit",
        61 => "wait4",
        62 => "kill",
        63 => "uname",
        72 => "fcntl",
        79 => "getcwd",
        80 => "chdir",
        83 => "mkdir",
        87 => "unlink",
        89 => "readlink",
        96 => "gettimeofday",
        99 => "sysinfo",
        102 => "getuid",
        104 => "getgid",
        107 => "geteuid",
        108 => "getegid",
        109 => "setpgid",
        110 => "getppid",
        137 => "statfs",
        138 => "fstatfs",
        157 => "prctl",
        158 => "arch_prctl",
        186 => "gettid",
        201 => "time",
        200 => "tkill",
        202 => "futex",
        203 => "sched_setaffinity",
        204 => "sched_getaffinity",
        217 => "getdents64",
        218 => "set_tid_address",
        228 => "clock_gettime",
        230 => "clock_nanosleep",
        231 => "exit_group",
        234 => "tgkill",
        257 => "openat",
        262 => "newfstatat",
        263 => "unlinkat",
        267 => "readlinkat",
        270 => "pselect6",
        271 => "ppoll",
        273 => "set_robust_list",
        293 => "pipe2",
        302 => "prlimit64",
        318 => "getrandom",
        332 => "statx",
        334 => "rseq",
        435 => "clone3",
        ELI_PROCESS_LIST => "eli_process_list",
        ELI_KERNEL_LOG => "eli_kernel_log",
        ELI_TRACE => "eli_trace",
        _ => return None,
    })
}

/// Prints a traced call (dimmed, to tell it apart from program output).
fn trace(pid: process::Pid, number: u64, a: &[u64; 6], result: Option<&Result<u64>>) {
    let name = name(number).map_or_else(|| alloc::format!("syscall_{number}"), alloc::string::ToString::to_string);
    let outcome = match result {
        None => alloc::string::String::from("..."),
        Some(Ok(value)) if *value > 0xFFFF => alloc::format!("{value:#x}"),
        Some(Ok(value)) => alloc::format!("{value}"),
        Some(Err(error)) => alloc::format!("-1 {}", crate::errno::name(*error as i64)),
    };
    crate::log::write_terminal(&alloc::format!(
        "\x1b[2m[{pid}] {name}({:#x}, {:#x}, {:#x}) = {outcome}\x1b[0m\n",
        a[0],
        a[1],
        a[2]
    ));
}

/// Entered from `syscall_entry` with the caller's registers in `frame`.
#[unsafe(no_mangle)]
extern "sysv64" fn syscall_dispatch(frame: &mut InterruptFrame) {
    arch::enable_interrupts();
    let number = frame.rax;
    let a = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];
    let traced = process::current().trace.load(core::sync::atomic::Ordering::Relaxed).then(|| process::current().pid);
    // Calls that don't return (or return into a new program) are shown
    // before they run.
    let no_return = matches!(number, 59 | 60 | 231);
    if let Some(pid) = traced
        && no_return
    {
        trace(pid, number, &a, None);
    }
    let result = dispatch(number, a, frame);
    if let Some(pid) = traced
        && !no_return
    {
        trace(pid, number, &a, Some(&result));
    }
    frame.rax = match result {
        Ok(value) => value,
        Err(error) => (-(error as i64)) as u64,
    };
    arch::disable_interrupts();
    // rt_sigreturn restores rax from the signal frame; that's no system call
    // result to restart.
    let restartable = (number != 15).then_some(number);
    process::before_return_to_user(frame, restartable);
}

fn dispatch(number: u64, a: [u64; 6], frame: &mut InterruptFrame) -> Result<u64> {
    const AT_FDCWD: u64 = -100i64 as u64;
    match number {
        0 => files::read(a[0], a[1], a[2]),
        1 => files::write(a[0], a[1], a[2]),
        2 => files::open_at(AT_FDCWD, a[0], a[1], a[2]),
        3 => files::close(a[0]),
        4 => files::stat_at(AT_FDCWD, a[0], a[1], 0),
        5 => files::fstat(a[0], a[1]),
        6 => files::stat_at(AT_FDCWD, a[0], a[1], 0x100),
        7 => poll::poll(a[0], a[1], a[2]),
        8 => files::seek(a[0], a[1], a[2]),
        9 => processes::mmap(a[0], a[1], a[2], a[3], a[4], a[5]),
        10 => processes::mprotect(a[0], a[1], a[2]),
        11 => processes::munmap(a[0], a[1]),
        12 => processes::brk(a[0]),
        13 => processes::sigaction(a[0], a[1], a[2]),
        14 => processes::sigprocmask(a[0], a[1], a[2]),
        15 => processes::sigreturn(frame),
        16 => files::ioctl(a[0], a[1], a[2]),
        17 => files::pread(a[0], a[1], a[2], a[3]),
        18 => files::pwrite(a[0], a[1], a[2], a[3]),
        19 => files::vectored(a[0], a[1], a[2], false),
        20 => files::vectored(a[0], a[1], a[2], true),
        21 => files::access_at(AT_FDCWD, a[0]),
        22 => files::pipe(a[0], 0),
        23 => poll::select(a[0], a[1], a[2], a[3], a[4], false),
        24 => processes::sched_yield(),
        // madvise: advice can be ignored.
        28 => Ok(0),
        32 => files::dup(a[0]),
        33 => files::dup2(a[0], a[1]),
        34 => signal::pause(),
        35 => processes::nanosleep(a[0]),
        39 => processes::getpid(),
        40 => files::sendfile(a[0], a[1], a[2], a[3]),
        56 => processes::clone(frame, a[0], a[1], a[2], a[3], a[4]),
        57 | 58 => processes::fork(frame),
        59 => processes::exec(frame, a[0], a[1], a[2]),
        60 => processes::exit(a[0]),
        61 => processes::wait(a[0], a[1], a[2]),
        62 => processes::kill(a[0], a[1]),
        63 => system::uname(a[0]),
        72 => files::fcntl(a[0], a[1], a[2]),
        // fsync, fdatasync: everything is in memory already.
        74 | 75 => Ok(0),
        76 => files::truncate(a[0], a[1]),
        77 => files::ftruncate(a[0], a[1]),
        79 => files::getcwd(a[0], a[1]),
        80 => files::chdir(a[0]),
        81 => files::fchdir(a[0]),
        82 => files::rename_at(AT_FDCWD, a[0], AT_FDCWD, a[1]),
        83 => files::mkdir_at(AT_FDCWD, a[0]),
        84 => files::unlink_at(AT_FDCWD, a[0], 0x200),
        87 => files::unlink_at(AT_FDCWD, a[0], 0),
        88 => files::symlink_at(a[0], AT_FDCWD, a[1]),
        89 => files::readlink_at(AT_FDCWD, a[0], a[1], a[2]),
        // chmod, fchmod, chown, fchown, lchown: no permissions to change.
        90..=94 => Ok(0),
        95 => Ok(0o022),
        96 => system::gettimeofday(a[0]),
        97 => system::prlimit(a[0], a[1]),
        99 => system::sysinfo(a[0]),
        // getuid, getgid, setuid, setgid, geteuid, getegid: everyone is root.
        102 | 104..=108 => Ok(0),
        109 => processes::setpgid(a[0], a[1]),
        110 => processes::getppid(),
        111 => processes::getpgid(0),
        112 => processes::setsid(),
        121 | 124 => processes::getpgid(a[0]),
        130 => processes::sigsuspend(a[0], a[1]),
        131 => processes::sigaltstack(a[0], a[1]),
        137 => files::statfs(a[0], a[1]),
        138 => files::fstatfs(a[0], a[1]),
        157 => processes::prctl(a[0], a[1]),
        158 => processes::arch_prctl(a[0], a[1]),
        // sync
        162 => Ok(0),
        169 => system::reboot(a[0], a[2]),
        186 => processes::gettid(),
        200 => processes::tkill(a[0], a[1]),
        201 => system::time(a[0]),
        202 => processes::futex(a[0], a[1], a[2], a[3], a[4], a[5]),
        // sched_setaffinity: there's nothing to choose between yet.
        203 => Ok(0),
        204 => processes::sched_getaffinity(a[1], a[2]),
        217 => files::get_directory_entries(a[0], a[1], a[2]),
        218 => processes::set_tid_address(a[0]),
        228 => system::clock_gettime(a[0], a[1]),
        229 => system::clock_getres(a[0], a[1]),
        230 => processes::clock_nanosleep(a[0], a[1], a[2]),
        231 => processes::exit_group(a[0]),
        234 => processes::tgkill(a[0], a[1], a[2]),
        257 => files::open_at(a[0], a[1], a[2], a[3]),
        258 => files::mkdir_at(a[0], a[1]),
        262 => files::stat_at(a[0], a[1], a[2], a[3]),
        263 => files::unlink_at(a[0], a[1], a[2]),
        264 | 316 => files::rename_at(a[0], a[1], a[2], a[3]),
        266 => files::symlink_at(a[0], a[1], a[2]),
        267 => files::readlink_at(a[0], a[1], a[2], a[3]),
        269 | 439 => files::access_at(a[0], a[1]),
        270 => poll::select(a[0], a[1], a[2], a[3], a[4], true),
        271 => poll::ppoll(a[0], a[1], a[2]),
        // set_robust_list, utimensat: nothing to do.
        273 | 280 => Ok(0),
        292 => files::dup3(a[0], a[1], a[2]),
        293 => files::pipe(a[0], a[1]),
        302 => system::prlimit(a[1], a[3]),
        318 => system::getrandom(a[0], a[1]),
        435 => processes::clone3(frame, a[0], a[1]),
        ELI_PROCESS_LIST => system::process_list(a[0], a[1]),
        ELI_KERNEL_LOG => system::kernel_log(a[0], a[1]),
        ELI_TRACE => {
            process::current().trace.store(a[0] != 0, core::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        _ => {
            debug!(
                "{}: system call {} is not implemented",
                process::current().name.lock(),
                name(number).map_or_else(|| alloc::format!("{number}"), alloc::string::ToString::to_string)
            );
            Err(Errno::ENOSYS)
        }
    }
}
