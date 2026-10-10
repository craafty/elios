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
mod processes;
mod system;

pub use self::system::HOSTNAME;

use crate::arch::interrupts::InterruptFrame;
use crate::errno::{Errno, Result};
use crate::{arch, process};

/// eliOS-specific calls.
pub const ELI_PROCESS_LIST: u64 = 0x1000;
pub const ELI_KERNEL_LOG: u64 = 0x1001;

/// Entered from `syscall_entry` with the caller's registers in `frame`.
#[unsafe(no_mangle)]
extern "sysv64" fn syscall_dispatch(frame: &mut InterruptFrame) {
    arch::enable_interrupts();
    let number = frame.rax;
    let a = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];
    let result = dispatch(number, a, frame);
    frame.rax = match result {
        Ok(value) => value,
        Err(error) => (-(error as i64)) as u64,
    };
    arch::disable_interrupts();
    process::before_return_to_user();
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
        8 => files::seek(a[0], a[1], a[2]),
        9 => processes::mmap(a[0], a[1], a[2], a[3], a[4], a[5]),
        10 => processes::mprotect(a[0], a[1], a[2]),
        11 => processes::munmap(a[0], a[1]),
        12 => processes::brk(a[0]),
        13 => processes::sigaction(a[0], a[1], a[2]),
        14 => processes::sigprocmask(a[2]),
        16 => files::ioctl(a[0], a[1], a[2]),
        17 => files::pread(a[0], a[1], a[2], a[3]),
        18 => files::pwrite(a[0], a[1], a[2], a[3]),
        19 => files::vectored(a[0], a[1], a[2], false),
        20 => files::vectored(a[0], a[1], a[2], true),
        21 => files::access_at(AT_FDCWD, a[0]),
        22 => files::pipe(a[0], 0),
        24 => processes::sched_yield(),
        // madvise: advice can be ignored.
        28 => Ok(0),
        32 => files::dup(a[0]),
        33 => files::dup2(a[0], a[1]),
        35 => processes::nanosleep(a[0]),
        39 | 186 => processes::getpid(),
        56 => processes::clone(frame, a[0], a[3]),
        57 | 58 => processes::fork(frame),
        59 => processes::exec(frame, a[0], a[1], a[2]),
        60 | 231 => processes::exit(a[0]),
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
        // sigaltstack: no handlers run yet, so no stack is needed.
        131 => Ok(0),
        158 => processes::arch_prctl(a[0], a[1]),
        // sync
        162 => Ok(0),
        169 => system::reboot(a[0], a[2]),
        200 => processes::kill(a[0], a[1]),
        201 => system::time(a[0]),
        // futex: processes have one thread, so there's never anyone to
        // wait for or wake.
        202 => Ok(0),
        217 => files::get_directory_entries(a[0], a[1], a[2]),
        218 => processes::getpid(),
        228 => system::clock_gettime(a[0], a[1]),
        229 => system::clock_getres(a[0], a[1]),
        230 => processes::nanosleep(a[2]),
        234 => processes::kill(a[1], a[2]),
        257 => files::open_at(a[0], a[1], a[2], a[3]),
        258 => files::mkdir_at(a[0], a[1]),
        262 => files::stat_at(a[0], a[1], a[2], a[3]),
        263 => files::unlink_at(a[0], a[1], a[2]),
        264 | 316 => files::rename_at(a[0], a[1], a[2], a[3]),
        266 => files::symlink_at(a[0], a[1], a[2]),
        267 => files::readlink_at(a[0], a[1], a[2], a[3]),
        269 | 439 => files::access_at(a[0], a[1]),
        // set_robust_list, utimensat: nothing to do.
        273 | 280 => Ok(0),
        292 => files::dup3(a[0], a[1], a[2]),
        293 => files::pipe(a[0], a[1]),
        302 => system::prlimit(a[1], a[3]),
        318 => system::getrandom(a[0], a[1]),
        ELI_PROCESS_LIST => system::process_list(a[0], a[1]),
        ELI_KERNEL_LOG => system::kernel_log(a[0], a[1]),
        _ => {
            debug!("{}: system call {number} is not implemented", process::current().name.lock());
            Err(Errno::ENOSYS)
        }
    }
}
