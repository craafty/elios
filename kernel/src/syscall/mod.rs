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
mod mounts;
mod net;
mod poll;
mod processes;
mod system;

pub use self::net::net_ioctl;
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
        36 => "getitimer",
        37 => "alarm",
        38 => "setitimer",
        39 => "getpid",
        40 => "sendfile",
        41 => "socket",
        42 => "connect",
        43 => "accept",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        48 => "shutdown",
        49 => "bind",
        50 => "listen",
        51 => "getsockname",
        52 => "getpeername",
        53 => "socketpair",
        54 => "setsockopt",
        55 => "getsockopt",
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
        81 => "fchdir",
        82 => "rename",
        83 => "mkdir",
        84 => "rmdir",
        86 => "link",
        87 => "unlink",
        89 => "readlink",
        90 => "chmod",
        91 => "fchmod",
        92 => "chown",
        93 => "fchown",
        94 => "lchown",
        95 => "umask",
        96 => "gettimeofday",
        99 => "sysinfo",
        102 => "getuid",
        105 => "setuid",
        106 => "setgid",
        104 => "getgid",
        107 => "geteuid",
        108 => "getegid",
        109 => "setpgid",
        110 => "getppid",
        113 => "setreuid",
        114 => "setregid",
        115 => "getgroups",
        116 => "setgroups",
        117 => "setresuid",
        118 => "getresuid",
        119 => "setresgid",
        120 => "getresgid",
        122 => "setfsuid",
        123 => "setfsgid",
        128 => "rt_sigtimedwait",
        130 => "rt_sigsuspend",
        131 => "sigaltstack",
        137 => "statfs",
        138 => "fstatfs",
        157 => "prctl",
        158 => "arch_prctl",
        162 => "sync",
        165 => "mount",
        166 => "umount2",
        170 => "sethostname",
        186 => "gettid",
        201 => "time",
        200 => "tkill",
        202 => "futex",
        222 => "timer_create",
        223 => "timer_settime",
        224 => "timer_gettime",
        225 => "timer_getoverrun",
        226 => "timer_delete",
        203 => "sched_setaffinity",
        204 => "sched_getaffinity",
        217 => "getdents64",
        218 => "set_tid_address",
        228 => "clock_gettime",
        230 => "clock_nanosleep",
        231 => "exit_group",
        234 => "tgkill",
        235 => "utimes",
        257 => "openat",
        260 => "fchownat",
        262 => "newfstatat",
        263 => "unlinkat",
        265 => "linkat",
        267 => "readlinkat",
        268 => "fchmodat",
        269 => "faccessat",
        270 => "pselect6",
        271 => "ppoll",
        280 => "utimensat",
        273 => "set_robust_list",
        288 => "accept4",
        293 => "pipe2",
        302 => "prlimit64",
        318 => "getrandom",
        332 => "statx",
        334 => "rseq",
        435 => "clone3",
        439 => "faccessat2",
        452 => "fchmodat2",
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
        21 => files::access_at(AT_FDCWD, a[0], a[1], 0),
        22 => files::pipe(a[0], 0),
        23 => poll::select(a[0], a[1], a[2], a[3], a[4], false),
        24 => processes::sched_yield(),
        // madvise: advice can be ignored.
        28 => Ok(0),
        32 => files::dup(a[0]),
        33 => files::dup2(a[0], a[1]),
        34 => signal::pause(),
        35 => processes::nanosleep(a[0]),
        36 => processes::getitimer(a[0], a[1]),
        37 => processes::alarm(a[0]),
        38 => processes::setitimer(a[0], a[1], a[2]),
        39 => processes::getpid(),
        40 => files::sendfile(a[0], a[1], a[2], a[3]),
        41 => net::socket(a[0], a[1], a[2]),
        42 => net::connect(a[0], a[1], a[2]),
        43 => net::accept(a[0], a[1], a[2], 0),
        44 => net::send_to(a[0], a[1], a[2], a[3], a[4], a[5]),
        45 => net::receive_from(a[0], a[1], a[2], a[3], a[4], a[5]),
        46 => net::send_message(a[0], a[1], a[2]),
        47 => net::receive_message(a[0], a[1], a[2]),
        48 => net::shutdown(a[0], a[1]),
        49 => net::bind(a[0], a[1], a[2]),
        50 => net::listen(a[0], a[1]),
        51 => net::get_socket_name(a[0], a[1], a[2]),
        52 => net::get_peer_name(a[0], a[1], a[2]),
        53 => Err(Errno::EAFNOSUPPORT),
        54 => net::set_option(a[0], a[1], a[2], a[3], a[4]),
        55 => net::get_option(a[0], a[1], a[2], a[3], a[4]),
        56 => processes::clone(frame, a[0], a[1], a[2], a[3], a[4]),
        57 | 58 => processes::fork(frame),
        59 => processes::exec(frame, a[0], a[1], a[2]),
        60 => processes::exit(a[0]),
        61 => processes::wait(a[0], a[1], a[2]),
        62 => processes::kill(a[0], a[1]),
        63 => system::uname(a[0]),
        72 => files::fcntl(a[0], a[1], a[2]),
        // fsync, fdatasync: everything is written back.
        74 | 75 => mounts::sync(),
        76 => files::truncate(a[0], a[1]),
        77 => files::ftruncate(a[0], a[1]),
        79 => files::getcwd(a[0], a[1]),
        80 => files::chdir(a[0]),
        81 => files::fchdir(a[0]),
        82 => files::rename_at(AT_FDCWD, a[0], AT_FDCWD, a[1]),
        83 => files::mkdir_at(AT_FDCWD, a[0], a[1]),
        84 => files::unlink_at(AT_FDCWD, a[0], 0x200),
        86 => files::link_at(AT_FDCWD, a[0], AT_FDCWD, a[1], 0),
        87 => files::unlink_at(AT_FDCWD, a[0], 0),
        88 => files::symlink_at(a[0], AT_FDCWD, a[1]),
        89 => files::readlink_at(AT_FDCWD, a[0], a[1], a[2]),
        90 => files::chmod_at(AT_FDCWD, a[0], a[1], 0),
        91 => files::fchmod(a[0], a[1]),
        92 => files::chown_at(AT_FDCWD, a[0], a[1], a[2], 0),
        93 => files::fchown(a[0], a[1], a[2]),
        94 => files::chown_at(AT_FDCWD, a[0], a[1], a[2], 0x100),
        95 => processes::umask(a[0]),
        96 => system::gettimeofday(a[0]),
        97 => system::prlimit(a[0], a[1]),
        99 => system::sysinfo(a[0]),
        102 | 104 | 107 | 108 => processes::get_id(number),
        105 => processes::setuid(a[0]),
        106 => processes::setgid(a[0]),
        109 => processes::setpgid(a[0], a[1]),
        110 => processes::getppid(),
        111 => processes::getpgid(0),
        112 => processes::setsid(),
        113 => processes::setreuid(a[0], a[1]),
        114 => processes::setregid(a[0], a[1]),
        115 => processes::getgroups(a[0], a[1]),
        116 => processes::setgroups(a[0], a[1]),
        117 => processes::setresuid(a[0], a[1], a[2]),
        118 => processes::get_three_ids(a[0], a[1], a[2], false),
        119 => processes::setresgid(a[0], a[1], a[2]),
        120 => processes::get_three_ids(a[0], a[1], a[2], true),
        122 => processes::fs_id(false),
        123 => processes::fs_id(true),
        121 | 124 => processes::getpgid(a[0]),
        128 => processes::sigtimedwait(a[0], a[1], a[2], a[3]),
        130 => processes::sigsuspend(a[0], a[1]),
        131 => processes::sigaltstack(a[0], a[1]),
        137 => files::statfs(a[0], a[1]),
        138 => files::fstatfs(a[0], a[1]),
        157 => processes::prctl(a[0], a[1]),
        158 => processes::arch_prctl(a[0], a[1]),
        162 | 306 => mounts::sync(),
        165 => mounts::mount(a[0], a[1], a[2], a[3], a[4]),
        166 => mounts::umount(a[0], a[1]),
        169 => system::reboot(a[0], a[2]),
        170 => system::sethostname(a[0], a[1]),
        186 => processes::gettid(),
        200 => processes::tkill(a[0], a[1]),
        201 => system::time(a[0]),
        202 => processes::futex(a[0], a[1], a[2], a[3], a[4], a[5]),
        // sched_setaffinity: there's nothing to choose between yet.
        203 => Ok(0),
        204 => processes::sched_getaffinity(a[1], a[2]),
        217 => files::get_directory_entries(a[0], a[1], a[2]),
        222 => processes::timer_create(a[0], a[1], a[2]),
        223 => processes::timer_settime(a[0], a[1], a[2], a[3]),
        224 => processes::timer_gettime(a[0], a[1]),
        225 => processes::timer_getoverrun(a[0]),
        226 => processes::timer_delete(a[0]),
        218 => processes::set_tid_address(a[0]),
        228 => system::clock_gettime(a[0], a[1]),
        229 => system::clock_getres(a[0], a[1]),
        230 => processes::clock_nanosleep(a[0], a[1], a[2]),
        231 => processes::exit_group(a[0]),
        234 => processes::tgkill(a[0], a[1], a[2]),
        235 => files::utimes(a[0], a[1]),
        257 => files::open_at(a[0], a[1], a[2], a[3]),
        258 => files::mkdir_at(a[0], a[1], a[2]),
        260 => files::chown_at(a[0], a[1], a[2], a[3], a[4]),
        262 => files::stat_at(a[0], a[1], a[2], a[3]),
        263 => files::unlink_at(a[0], a[1], a[2]),
        264 | 316 => files::rename_at(a[0], a[1], a[2], a[3]),
        265 => files::link_at(a[0], a[1], a[2], a[3], a[4]),
        266 => files::symlink_at(a[0], a[1], a[2]),
        267 => files::readlink_at(a[0], a[1], a[2], a[3]),
        268 => files::chmod_at(a[0], a[1], a[2], 0),
        269 => files::access_at(a[0], a[1], a[2], 0),
        270 => poll::select(a[0], a[1], a[2], a[3], a[4], true),
        271 => poll::ppoll(a[0], a[1], a[2]),
        // set_robust_list: nothing to do.
        273 => Ok(0),
        280 => files::utimensat(a[0], a[1], a[2], a[3]),
        288 => net::accept(a[0], a[1], a[2], a[3]),
        292 => files::dup3(a[0], a[1], a[2]),
        293 => files::pipe(a[0], a[1]),
        302 => system::prlimit(a[1], a[3]),
        318 => system::getrandom(a[0], a[1]),
        435 => processes::clone3(frame, a[0], a[1]),
        439 => files::access_at(a[0], a[1], a[2], a[3]),
        452 => files::chmod_at(a[0], a[1], a[2], a[3]),
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
