//! Process, memory and signal system calls.

use crate::arch::interrupts::InterruptFrame;
use crate::arch::syscall;
use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind};
use crate::memory::PAGE_SIZE;
use crate::process::{self, PROT_EXEC, PROT_READ, PROT_WRITE, Pid, SIGKILL, SigAction, USER_END, signal, user};
use crate::task;

pub fn fork(frame: &InterruptFrame) -> Result<u64> {
    process::fork(frame).map(u64::from)
}

const CLONE_VM: u64 = 0x100;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// `clone` for the case programs use to fork (glibc's `fork` calls it).
/// Shared-memory clones (threads) aren't supported yet.
pub fn clone(frame: &InterruptFrame, flags: u64, child_tid: u64) -> Result<u64> {
    if flags & (CLONE_VM | CLONE_THREAD) != 0 {
        return Err(Errno::ENOSYS);
    }
    let pid = process::fork(frame)?;
    if flags & CLONE_CHILD_SETTID != 0 && child_tid != 0 {
        // The child's copy of memory holds the variable; write it there.
        if let Some(child) = process::find(pid)
            && let Some(space) = child.space.lock().as_mut()
        {
            space.write(child_tid, &pid.to_le_bytes())?;
        }
    }
    Ok(u64::from(pid))
}

pub fn exec(frame: &mut InterruptFrame, path: u64, arguments: u64, environment: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let arguments = user::read_string_array(arguments)?;
    let environment = user::read_string_array(environment)?;
    process::exec(frame, &path, arguments, environment)?;
    Ok(0)
}

pub fn exit(code: u64) -> ! {
    process::exit(process::exit_status(code as i32))
}

const WNOHANG: u64 = 1;

pub fn wait(pid: u64, status: u64, options: u64) -> Result<u64> {
    match process::wait(pid as i32 as i64, options & WNOHANG != 0)? {
        Some((pid, wait_status)) => {
            if status != 0 {
                user::write_value(status, &wait_status)?;
            }
            Ok(u64::from(pid))
        }
        None => Ok(0),
    }
}

pub fn kill(pid: u64, signal: u64) -> Result<u64> {
    process::kill(pid as i32 as i64, signal as u32).map(|_| 0)
}

pub fn getpid() -> Result<u64> {
    Ok(u64::from(process::current().pid))
}

pub fn getppid() -> Result<u64> {
    Ok(u64::from(process::current().parent()))
}

/// `setpgid`: moves a process (the caller or a child) into a group.
pub fn setpgid(pid: u64, group: u64) -> Result<u64> {
    let me = process::current();
    let pid = if pid == 0 { me.pid } else { pid as Pid };
    let target = process::find(pid).ok_or(Errno::ESRCH)?;
    if target.pid != me.pid && target.parent() != me.pid {
        return Err(Errno::ESRCH);
    }
    target.set_group(if group == 0 { pid } else { group as Pid });
    Ok(0)
}

pub fn getpgid(pid: u64) -> Result<u64> {
    let process = if pid == 0 { process::current() } else { process::find(pid as Pid).ok_or(Errno::ESRCH)? };
    Ok(u64::from(process.group()))
}

pub fn setsid() -> Result<u64> {
    let me = process::current();
    me.set_group(me.pid);
    Ok(u64::from(me.pid))
}

/// `rt_sigaction`: how a signal is handled (ignored, default, or a
/// handler), as a Linux `struct sigaction`.
pub fn sigaction(signal: u64, action: u64, old_action: u64) -> Result<u64> {
    let signal = signal as usize;
    if signal == 0 || signal >= process::SIGNAL_COUNT {
        return Err(Errno::EINVAL);
    }
    let new: Option<SigAction> = if action == 0 { None } else { Some(user::read_value(action)?) };
    if new.is_some() && (signal == SIGKILL as usize || signal == 19) {
        return Err(Errno::EINVAL);
    }
    let process = process::current();
    let previous = process.signals.lock().actions[signal];
    if old_action != 0 {
        user::write_value(old_action, &previous)?;
    }
    if let Some(new) = new {
        process.signals.lock().actions[signal] = new;
    }
    Ok(0)
}

pub fn sigprocmask(how: u64, set: u64, old_set: u64) -> Result<u64> {
    let set = if set == 0 { None } else { Some(user::read_value::<u64>(set)?) };
    let old = signal::set_mask(&process::current(), how, set)?;
    if old_set != 0 {
        user::write_value(old_set, &old)?;
    }
    Ok(0)
}

pub fn sigreturn(frame: &mut InterruptFrame) -> Result<u64> {
    signal::sigreturn(&process::current(), frame)
}

/// `sigaltstack`, with `stack_t` as (sp, flags, size).
pub fn sigaltstack(new: u64, old: u64) -> Result<u64> {
    let new = if new == 0 {
        None
    } else {
        let [base, flags, size]: [u64; 3] = user::read_value(new)?;
        Some((base, flags & 0xFFFF_FFFF, size))
    };
    let (base, flags, size) = signal::set_alternate_stack(&process::current(), new)?;
    if old != 0 {
        user::write_value(old, &[base, flags, size])?;
    }
    Ok(0)
}

const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;

/// `arch_prctl`: programs set FS to their thread-local storage.
pub fn arch_prctl(code: u64, address: u64) -> Result<u64> {
    match code {
        ARCH_SET_FS => {
            if address >= USER_END {
                return Err(Errno::EPERM);
            }
            syscall::set_fs_base(address);
            Ok(0)
        }
        ARCH_GET_FS => user::write_value(address, &syscall::fs_base()).map(|_| 0),
        _ => Err(Errno::EINVAL),
    }
}

pub fn brk(address: u64) -> Result<u64> {
    let process = process::current();
    let mut space = process.space.lock();
    let space = space.as_mut().ok_or(Errno::ENOMEM)?;
    Ok(if address == 0 { space.brk } else { space.set_brk(address) })
}

const MAP_SHARED: u64 = 0x01;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

/// `mmap`: anonymous memory, or a private copy of part of a file.
pub fn mmap(address: u64, len: u64, protection: u64, flags: u64, fd: u64, offset: u64) -> Result<u64> {
    if len == 0 || !offset.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let len = len.checked_next_multiple_of(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    let protection = protection as u32 & (PROT_READ | PROT_WRITE | PROT_EXEC);
    let file = if flags & MAP_ANONYMOUS == 0 {
        let file = process::current().files.lock().get(fd as usize)?;
        if file.inode.metadata().kind != FileKind::Regular {
            return Err(Errno::ENODEV);
        }
        Some(file)
    } else {
        None
    };
    if flags & MAP_SHARED != 0 && file.as_ref().is_some_and(|f| f.writable()) && protection & PROT_WRITE != 0 {
        // Writing through to files isn't supported.
        return Err(Errno::ENODEV);
    }

    let process = process::current();
    let mut space = process.space.lock();
    let space = space.as_mut().ok_or(Errno::ENOMEM)?;
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
    let start = if fixed {
        if !address.is_multiple_of(PAGE_SIZE) || address.checked_add(len).is_none_or(|end| end > USER_END) {
            return Err(Errno::EINVAL);
        }
        if flags & MAP_FIXED != 0 {
            space.remove(address, address + len);
        }
        address
    } else {
        let hint_free = address != 0
            && address.is_multiple_of(PAGE_SIZE)
            && address.checked_add(len).is_some_and(|end| end <= USER_END)
            && space.add_area(address, address + len, protection).is_ok();
        if hint_free {
            // add_area already reserved it above.
            return finish_mmap(space, address, len, protection, file, offset, true);
        }
        space.find_free(len).ok_or(Errno::ENOMEM)?
    };
    finish_mmap(space, start, len, protection, file, offset, false)
}

fn finish_mmap(
    space: &mut process::AddressSpace,
    start: u64,
    len: u64,
    protection: u32,
    file: Option<alloc::sync::Arc<fs::OpenFile>>,
    offset: u64,
    area_added: bool,
) -> Result<u64> {
    if !area_added {
        space.add_area(start, start + len, protection)?;
    }
    if let Some(file) = file {
        let mut data = alloc::vec![0; len as usize];
        let count = file.inode.read_at(offset, &mut data)?;
        space.write(start, &data[..count])?;
    }
    Ok(start)
}

pub fn munmap(address: u64, len: u64) -> Result<u64> {
    if !address.is_multiple_of(PAGE_SIZE) || len == 0 {
        return Err(Errno::EINVAL);
    }
    let end = address.checked_add(len.next_multiple_of(PAGE_SIZE)).filter(|&e| e <= USER_END).ok_or(Errno::EINVAL)?;
    let process = process::current();
    if let Some(space) = process.space.lock().as_mut() {
        space.remove(address, end);
    }
    Ok(0)
}

pub fn mprotect(address: u64, len: u64, protection: u64) -> Result<u64> {
    if !address.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let end = address.checked_add(len.next_multiple_of(PAGE_SIZE)).filter(|&e| e <= USER_END).ok_or(Errno::ENOMEM)?;
    let process = process::current();
    let mut space = process.space.lock();
    let space = space.as_mut().ok_or(Errno::ENOMEM)?;
    space.protect(address, end, protection as u32 & (PROT_READ | PROT_WRITE | PROT_EXEC)).map(|_| 0)
}

pub fn sched_yield() -> Result<u64> {
    task::yield_now();
    Ok(0)
}

/// `nanosleep` and `clock_nanosleep`: sleeps for a `struct timespec`.
pub fn nanosleep(request: u64) -> Result<u64> {
    let [seconds, nanoseconds]: [i64; 2] = user::read_value(request)?;
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(Errno::EINVAL);
    }
    let ms = seconds as u64 * 1000 + (nanoseconds as u64).div_ceil(1_000_000);
    task::sleep_ms(ms);
    if process::signal_pending() {
        return Err(Errno::EINTR);
    }
    Ok(0)
}
