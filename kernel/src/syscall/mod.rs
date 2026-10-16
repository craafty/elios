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
pub use self::system::{HOSTNAME, KERNEL_RELEASE};

use crate::arch::interrupts::InterruptFrame;
use crate::errno::{Errno, Result};
use crate::process::signal;
use crate::{arch, process, task};

/// eliOS-specific calls.
pub const ELI_PROCESS_LIST: u64 = 0x1000;
pub const ELI_KERNEL_LOG: u64 = 0x1001;
pub const ELI_TRACE: u64 = 0x1002;

/// System call names, for tracing.
/// Linux's x86_64 system call names, by number (from its `unistd_64.h`),
/// for tracing and for reporting calls eliOS doesn't implement.
#[rustfmt::skip]
const NAMES: [&str; 472] = [
    "read", "write", "open", "close", "stat", "fstat", "lstat", "poll", "lseek", "mmap", "mprotect", "munmap", "brk",
    "rt_sigaction", "rt_sigprocmask", "rt_sigreturn", "ioctl", "pread64", "pwrite64", "readv", "writev", "access",
    "pipe", "select", "sched_yield", "mremap", "msync", "mincore", "madvise", "shmget", "shmat", "shmctl", "dup",
    "dup2", "pause", "nanosleep", "getitimer", "alarm", "setitimer", "getpid", "sendfile", "socket", "connect",
    "accept", "sendto", "recvfrom", "sendmsg", "recvmsg", "shutdown", "bind", "listen", "getsockname", "getpeername",
    "socketpair", "setsockopt", "getsockopt", "clone", "fork", "vfork", "execve", "exit", "wait4", "kill", "uname",
    "semget", "semop", "semctl", "shmdt", "msgget", "msgsnd", "msgrcv", "msgctl", "fcntl", "flock", "fsync",
    "fdatasync", "truncate", "ftruncate", "getdents", "getcwd", "chdir", "fchdir", "rename", "mkdir", "rmdir",
    "creat", "link", "unlink", "symlink", "readlink", "chmod", "fchmod", "chown", "fchown", "lchown", "umask",
    "gettimeofday", "getrlimit", "getrusage", "sysinfo", "times", "ptrace", "getuid", "syslog", "getgid", "setuid",
    "setgid", "geteuid", "getegid", "setpgid", "getppid", "getpgrp", "setsid", "setreuid", "setregid", "getgroups",
    "setgroups", "setresuid", "getresuid", "setresgid", "getresgid", "getpgid", "setfsuid", "setfsgid", "getsid",
    "capget", "capset", "rt_sigpending", "rt_sigtimedwait", "rt_sigqueueinfo", "rt_sigsuspend", "sigaltstack",
    "utime", "mknod", "uselib", "personality", "ustat", "statfs", "fstatfs", "sysfs", "getpriority", "setpriority",
    "sched_setparam", "sched_getparam", "sched_setscheduler", "sched_getscheduler", "sched_get_priority_max",
    "sched_get_priority_min", "sched_rr_get_interval", "mlock", "munlock", "mlockall", "munlockall", "vhangup",
    "modify_ldt", "pivot_root", "_sysctl", "prctl", "arch_prctl", "adjtimex", "setrlimit", "chroot", "sync", "acct",
    "settimeofday", "mount", "umount2", "swapon", "swapoff", "reboot", "sethostname", "setdomainname", "iopl",
    "ioperm", "create_module", "init_module", "delete_module", "get_kernel_syms", "query_module", "quotactl",
    "nfsservctl", "getpmsg", "putpmsg", "afs_syscall", "tuxcall", "security", "gettid", "readahead", "setxattr",
    "lsetxattr", "fsetxattr", "getxattr", "lgetxattr", "fgetxattr", "listxattr", "llistxattr", "flistxattr",
    "removexattr", "lremovexattr", "fremovexattr", "tkill", "time", "futex", "sched_setaffinity", "sched_getaffinity",
    "set_thread_area", "io_setup", "io_destroy", "io_getevents", "io_submit", "io_cancel", "get_thread_area",
    "lookup_dcookie", "epoll_create", "epoll_ctl_old", "epoll_wait_old", "remap_file_pages", "getdents64",
    "set_tid_address", "restart_syscall", "semtimedop", "fadvise64", "timer_create", "timer_settime", "timer_gettime",
    "timer_getoverrun", "timer_delete", "clock_settime", "clock_gettime", "clock_getres", "clock_nanosleep",
    "exit_group", "epoll_wait", "epoll_ctl", "tgkill", "utimes", "vserver", "mbind", "set_mempolicy", "get_mempolicy",
    "mq_open", "mq_unlink", "mq_timedsend", "mq_timedreceive", "mq_notify", "mq_getsetattr", "kexec_load", "waitid",
    "add_key", "request_key", "keyctl", "ioprio_set", "ioprio_get", "inotify_init", "inotify_add_watch",
    "inotify_rm_watch", "migrate_pages", "openat", "mkdirat", "mknodat", "fchownat", "futimesat", "newfstatat",
    "unlinkat", "renameat", "linkat", "symlinkat", "readlinkat", "fchmodat", "faccessat", "pselect6", "ppoll",
    "unshare", "set_robust_list", "get_robust_list", "splice", "tee", "sync_file_range", "vmsplice", "move_pages",
    "utimensat", "epoll_pwait", "signalfd", "timerfd_create", "eventfd", "fallocate", "timerfd_settime",
    "timerfd_gettime", "accept4", "signalfd4", "eventfd2", "epoll_create1", "dup3", "pipe2", "inotify_init1",
    "preadv", "pwritev", "rt_tgsigqueueinfo", "perf_event_open", "recvmmsg", "fanotify_init", "fanotify_mark",
    "prlimit64", "name_to_handle_at", "open_by_handle_at", "clock_adjtime", "syncfs", "sendmmsg", "setns", "getcpu",
    "process_vm_readv", "process_vm_writev", "kcmp", "finit_module", "sched_setattr", "sched_getattr", "renameat2",
    "seccomp", "getrandom", "memfd_create", "kexec_file_load", "bpf", "execveat", "userfaultfd", "membarrier",
    "mlock2", "copy_file_range", "preadv2", "pwritev2", "pkey_mprotect", "pkey_alloc", "pkey_free", "statx",
    "io_pgetevents", "rseq", "uretprobe", "uprobe", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "pidfd_send_signal", "io_uring_setup",
    "io_uring_enter", "io_uring_register", "open_tree", "move_mount", "fsopen", "fsconfig", "fsmount", "fspick",
    "pidfd_open", "clone3", "close_range", "openat2", "pidfd_getfd", "faccessat2", "process_madvise", "epoll_pwait2",
    "mount_setattr", "quotactl_fd", "landlock_create_ruleset", "landlock_add_rule", "landlock_restrict_self",
    "memfd_secret", "process_mrelease", "futex_waitv", "set_mempolicy_home_node", "cachestat", "fchmodat2",
    "map_shadow_stack", "futex_wake", "futex_wait", "futex_requeue", "statmount", "listmount", "lsm_get_self_attr",
    "lsm_set_self_attr", "lsm_list_modules", "mseal", "setxattrat", "getxattrat", "listxattrat", "removexattrat",
    "open_tree_attr", "file_getattr", "file_setattr", "listns", "rseq_slice_yield",
];

fn name(number: u64) -> Option<&'static str> {
    match number {
        ELI_PROCESS_LIST => Some("eli_process_list"),
        ELI_KERNEL_LOG => Some("eli_kernel_log"),
        ELI_TRACE => Some("eli_trace"),
        _ => NAMES.get(number as usize).copied().filter(|name| !name.is_empty()),
    }
}

/// A system call's name (or its number), for /proc and tracing.
pub fn call_name(number: u64) -> alloc::string::String {
    name(number).map_or_else(|| alloc::format!("syscall_{number}"), alloc::string::ToString::to_string)
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
    // Paths are shown as strings: what a program looks for is usually the
    // interesting part.
    let path_argument = match number {
        2 | 4 | 6 | 21 | 59 | 76 | 80 | 82 | 83 | 84 | 86 | 87 | 88 | 89 | 90 | 92 | 94 | 137 | 161 | 165 => Some(0),
        257 | 258 | 260 | 262 | 263 | 264 | 265 | 266 | 267 | 268 | 269 | 280 | 316 | 332 | 437 | 439 | 452 => Some(1),
        _ => None,
    };
    let argument = |index: usize| match path_argument {
        Some(path) if path == index => process::user::read_string(a[index])
            .map_or_else(|_| alloc::format!("{:#x}", a[index]), |s| alloc::format!("{s:?}")),
        _ => alloc::format!("{:#x}", a[index]),
    };
    crate::log::write_terminal(&alloc::format!(
        "\x1b[2m[{pid}] {name}({}, {}, {}) = {outcome}\x1b[0m\n",
        argument(0),
        argument(1),
        argument(2)
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
    let thread = task::current_user_thread();
    if let Some(thread) = &thread {
        for (slot, value) in thread.syscall_arguments.iter().zip(a) {
            slot.store(value, core::sync::atomic::Ordering::Relaxed);
        }
        thread.syscall.store(number + 1, core::sync::atomic::Ordering::Relaxed);
        thread.syscall_stack.store(frame.rsp, core::sync::atomic::Ordering::Relaxed);
        thread.syscall_rip.store(frame.rip, core::sync::atomic::Ordering::Relaxed);
    }
    let result = dispatch(number, a, frame);
    if let Some(thread) = &thread {
        thread.syscall.store(0, core::sync::atomic::Ordering::Relaxed);
    }
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
        25 => processes::mremap(a[0], a[1], a[2], a[3]),
        28 => processes::madvise(a[0], a[1], a[2]),
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
        53 => net::socketpair(a[0], a[1], a[2], a[3]),
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
        125 => system::capget(a[0], a[1]),
        126 => system::capset(a[0], a[1]),
        128 => processes::sigtimedwait(a[0], a[1], a[2], a[3]),
        130 => processes::sigsuspend(a[0], a[1]),
        131 => processes::sigaltstack(a[0], a[1]),
        137 => files::statfs(a[0], a[1]),
        // getpriority and setpriority: everything runs at the normal
        // priority (reported, as the system call does, as 20 - nice).
        140 => Ok(20),
        141 => Ok(0),
        // The scheduler: one policy (SCHED_OTHER, 0) at priority 0.
        142 | 144 => Ok(0),
        143 => process::user::write_value(a[1], &0i32).map(|()| 0),
        145..=147 => Ok(0),
        // readahead: a hint.
        187 => Ok(0),
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
        // fadvise64: a hint.
        221 => Ok(0),
        217 => files::get_directory_entries(a[0], a[1], a[2]),
        222 => processes::timer_create(a[0], a[1], a[2]),
        223 => processes::timer_settime(a[0], a[1], a[2], a[3]),
        224 => processes::timer_gettime(a[0], a[1]),
        225 => processes::timer_getoverrun(a[0]),
        226 => processes::timer_delete(a[0]),
        213 => poll::epoll_create(0),
        218 => processes::set_tid_address(a[0]),
        228 => system::clock_gettime(a[0], a[1]),
        229 => system::clock_getres(a[0], a[1]),
        230 => processes::clock_nanosleep(a[0], a[1], a[2]),
        231 => processes::exit_group(a[0]),
        232 => poll::epoll_wait(a[0], a[1], a[2], a[3], false),
        233 => poll::epoll_ctl(a[0], a[1], a[2], a[3]),
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
        // unshare: there's nothing shared to unshare, and no namespaces.
        272 if a[0] & process::CLONE_NEW_NAMESPACES != 0 => Err(Errno::EPERM),
        272 => Ok(0),
        // set_robust_list: nothing to do.
        273 => Ok(0),
        280 => files::utimensat(a[0], a[1], a[2], a[3]),
        281 => poll::epoll_wait(a[0], a[1], a[2], a[3], false),
        283 => poll::timerfd_create(a[0], a[1]),
        285 => files::fallocate(a[0], a[1], a[2], a[3]),
        284 => poll::eventfd(a[0], 0),
        286 => poll::timerfd_settime(a[0], a[1], a[2], a[3]),
        287 => poll::timerfd_gettime(a[0], a[1]),
        288 => net::accept(a[0], a[1], a[2], a[3]),
        290 => poll::eventfd(a[0], a[1]),
        291 => poll::epoll_create(a[0]),
        292 => files::dup3(a[0], a[1], a[2]),
        293 => files::pipe(a[0], a[1]),
        295 | 327 => files::vectored_at(a[0], a[1], a[2], a[3], false),
        296 | 328 => files::vectored_at(a[0], a[1], a[2], a[3], true),
        302 => system::prlimit(a[1], a[3]),
        318 => system::getrandom(a[0], a[1]),
        319 => files::memfd_create(a[0], a[1]),
        324 => system::membarrier(a[0]),
        332 => files::statx(a[0], a[1], a[2], a[3], a[4]),
        435 => processes::clone3(frame, a[0], a[1]),
        436 => files::close_range(a[0], a[1], a[2]),
        439 => files::access_at(a[0], a[1], a[2], a[3]),
        441 => poll::epoll_wait(a[0], a[1], a[2], a[3], true),
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
