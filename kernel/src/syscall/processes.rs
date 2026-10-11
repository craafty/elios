//! Process, memory and signal system calls.

use crate::arch::interrupts::InterruptFrame;
use crate::arch::syscall;
use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind};
use crate::memory::PAGE_SIZE;
use crate::process::{self, CloneArgs, PROT_EXEC, PROT_READ, PROT_WRITE, Pid, SIGKILL, SigAction, USER_END};
use crate::process::{credentials, futex, signal, timers, user};
use crate::{task, time};

pub fn fork(frame: &InterruptFrame) -> Result<u64> {
    process::fork(frame).map(u64::from)
}

/// `clone(flags, stack, parent_tid, child_tid, tls)`. The low byte of the
/// flags is the signal to send the parent when the child exits.
pub fn clone(frame: &InterruptFrame, flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> Result<u64> {
    let args =
        CloneArgs { flags: flags & !0xFF, exit_signal: (flags & 0xFF) as u32, stack, parent_tid, child_tid, tls };
    process::clone(frame, &args).map(u64::from)
}

const CLONE_PIDFD: u64 = 0x1000;

/// `clone3(args, size)`, where `args` is a `struct clone_args`.
pub fn clone3(frame: &InterruptFrame, address: u64, size: u64) -> Result<u64> {
    if size < 64 {
        return Err(Errno::EINVAL);
    }
    let [flags, _pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls]: [u64; 8] =
        user::read_value(address)?;
    if flags & CLONE_PIDFD != 0 {
        return Err(Errno::EINVAL);
    }
    // clone3 gives the stack as a range; the stack pointer starts at its top.
    let stack = if stack == 0 { 0 } else { stack + stack_size };
    if exit_signal >= process::SIGNAL_COUNT as u64 {
        return Err(Errno::EINVAL);
    }
    let args = CloneArgs { flags, exit_signal: exit_signal as u32, stack, parent_tid, child_tid, tls };
    process::clone(frame, &args).map(u64::from)
}

pub fn exec(frame: &mut InterruptFrame, path: u64, arguments: u64, environment: u64) -> Result<u64> {
    let path = user::read_string(path)?;
    let arguments = user::read_string_array(arguments)?;
    let environment = user::read_string_array(environment)?;
    process::exec(frame, &path, arguments, environment)?;
    Ok(0)
}

/// `exit`: ends the calling thread.
pub fn exit(code: u64) -> ! {
    process::exit_thread(process::exit_status(code as i32))
}

/// `exit_group`: ends the whole process.
pub fn exit_group(code: u64) -> ! {
    process::exit_group(process::exit_status(code as i32))
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

pub fn gettid() -> Result<u64> {
    Ok(u64::from(process::current_thread().tid()))
}

/// `set_tid_address`: where to clear (and wake) when the thread exits.
pub fn set_tid_address(address: u64) -> Result<u64> {
    let thread = process::current_thread();
    thread.set_clear_child_tid(address);
    Ok(u64::from(thread.tid()))
}

pub fn tgkill(process: u64, tid: u64, signal: u64) -> Result<u64> {
    process::kill_thread(Some(process as Pid), tid as Pid, signal as u32).map(|_| 0)
}

pub fn tkill(tid: u64, signal: u64) -> Result<u64> {
    process::kill_thread(None, tid as Pid, signal as u32).map(|_| 0)
}

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;

/// `futex(address, op, value, timeout, address2, value3)`. Every futex is
/// private to its process, so the private flag changes nothing.
pub fn futex(address: u64, op: u64, value: u64, timeout: u64, address2: u64, value3: u64) -> Result<u64> {
    let pid = process::current().pid;
    let value = value as u32;
    match op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) {
        FUTEX_WAIT => {
            let deadline = deadline(timeout, None)?;
            futex::wait(pid, address, value, u32::MAX, deadline).map(|_| 0)
        }
        FUTEX_WAIT_BITSET => {
            let clock = if op & FUTEX_CLOCK_REALTIME != 0 { CLOCK_REALTIME } else { CLOCK_MONOTONIC };
            let deadline = deadline(timeout, Some(clock))?;
            futex::wait(pid, address, value, value3 as u32, deadline).map(|_| 0)
        }
        FUTEX_WAKE => futex::wake(pid, address, value, u32::MAX),
        FUTEX_WAKE_BITSET => futex::wake(pid, address, value, value3 as u32),
        // For the requeue operations, the timeout argument is a count.
        FUTEX_REQUEUE => futex::requeue(pid, address, address2, value, timeout as u32, None),
        FUTEX_CMP_REQUEUE => futex::requeue(pid, address, address2, value, timeout as u32, Some(value3 as u32)),
        _ => Err(Errno::ENOSYS),
    }
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// The timer tick at which a timeout given as a `struct timespec` at
/// `address` (none if 0) expires: relative to now, or a time on `clock`.
fn deadline(address: u64, clock: Option<u64>) -> Result<Option<u64>> {
    if address == 0 {
        return Ok(None);
    }
    let [seconds, nanoseconds]: [i64; 2] = user::read_value(address)?;
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(Errno::EINVAL);
    }
    let requested = seconds as u64 * 1_000_000_000 + nanoseconds as u64;
    let remaining = match clock {
        None => requested,
        Some(CLOCK_REALTIME) => requested.saturating_sub(time::unix_nanos()),
        Some(_) => requested.saturating_sub(time::uptime_nanos()),
    };
    Ok(Some(time::ticks() + remaining.div_ceil(1_000_000)))
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
    let old = signal::set_mask(&process::current_thread(), how, set)?;
    if old_set != 0 {
        user::write_value(old_set, &old)?;
    }
    Ok(0)
}

pub fn sigsuspend(mask: u64, size: u64) -> Result<u64> {
    if size != 8 {
        return Err(Errno::EINVAL);
    }
    let mask = user::read_value(mask)?;
    signal::suspend(&process::current_thread(), mask)
}

pub fn sigreturn(frame: &mut InterruptFrame) -> Result<u64> {
    signal::sigreturn(&process::current_thread(), frame)
}

/// `sigaltstack`, with `stack_t` as (sp, flags, size).
pub fn sigaltstack(new: u64, old: u64) -> Result<u64> {
    let new = if new == 0 {
        None
    } else {
        let [base, flags, size]: [u64; 3] = user::read_value(new)?;
        Some((base, flags & 0xFFFF_FFFF, size))
    };
    let (base, flags, size) = signal::set_alternate_stack(&process::current_thread(), new)?;
    if old != 0 {
        user::write_value(old, &[base, flags, size])?;
    }
    Ok(0)
}

const PR_SET_NAME: u64 = 15;
const PR_GET_NAME: u64 = 16;

/// `prctl`: only naming the process is supported.
pub fn prctl(option: u64, argument: u64) -> Result<u64> {
    let process = process::current();
    match option {
        PR_SET_NAME => {
            let mut name = user::read_bytes(argument, 16)?;
            name.truncate(name.iter().position(|&b| b == 0).unwrap_or(15));
            let name = alloc::string::String::from_utf8_lossy(&name).into_owned();
            task::set_name(&name);
            *process.name.lock() = name;
            Ok(0)
        }
        PR_GET_NAME => {
            let mut name = [0u8; 16];
            let current = process.name.lock().clone();
            let len = current.len().min(15);
            name[..len].copy_from_slice(&current.as_bytes()[..len]);
            user::write_value(argument, &name)?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
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

/// `sched_getaffinity`: the CPUs the process may run on (all of them), as
/// a bit mask.
pub fn sched_getaffinity(len: u64, mask: u64) -> Result<u64> {
    if len < 8 {
        return Err(Errno::EINVAL);
    }
    let bits = crate::arch::cpu::online().fold(0u64, |bits, cpu| bits | 1 << cpu.index());
    user::write_value(mask, &bits)?;
    Ok(8)
}

/// `nanosleep`: sleeps for a `struct timespec`.
pub fn nanosleep(request: u64) -> Result<u64> {
    sleep_until(deadline(request, None)?.ok_or(Errno::EFAULT)?)
}

const TIMER_ABSTIME: u64 = 1;

/// `clock_nanosleep`: sleeps for a time, or until a time on `clock`.
pub fn clock_nanosleep(clock: u64, flags: u64, request: u64) -> Result<u64> {
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | 7) {
        return Err(Errno::EINVAL);
    }
    let clock = (flags & TIMER_ABSTIME != 0).then_some(clock);
    sleep_until(deadline(request, clock)?.ok_or(Errno::EFAULT)?)
}

fn sleep_until(deadline: u64) -> Result<u64> {
    let now = time::ticks();
    if deadline > now {
        task::sleep_ms(deadline - now);
    }
    if process::signal_pending() {
        return Err(Errno::EINTR);
    }
    Ok(0)
}

/// A user or group id argument: -1 (as a 32-bit value) means "leave it".
fn id_argument(value: u64) -> Option<u32> {
    let id = value as u32;
    (id != u32::MAX).then_some(id)
}

/// `getuid`, `geteuid`, `getgid`, `getegid`.
pub fn get_id(number: u64) -> Result<u64> {
    let credentials = credentials::current();
    Ok(u64::from(match number {
        102 => credentials.uid,
        104 => credentials.gid,
        107 => credentials.euid,
        _ => credentials.egid,
    }))
}

pub fn setuid(uid: u64) -> Result<u64> {
    credentials::update(|current| current.set_uid(uid as u32)).map(|_| 0)
}

pub fn setgid(gid: u64) -> Result<u64> {
    credentials::update(|current| current.set_gid(gid as u32)).map(|_| 0)
}

pub fn setreuid(real: u64, effective: u64) -> Result<u64> {
    credentials::update(|current| current.set_real_effective_uid(id_argument(real), id_argument(effective))).map(|_| 0)
}

pub fn setregid(real: u64, effective: u64) -> Result<u64> {
    credentials::update(|current| current.set_real_effective_gid(id_argument(real), id_argument(effective))).map(|_| 0)
}

pub fn setresuid(real: u64, effective: u64, saved: u64) -> Result<u64> {
    credentials::update(|current| current.set_user_ids(id_argument(real), id_argument(effective), id_argument(saved)))
        .map(|_| 0)
}

pub fn setresgid(real: u64, effective: u64, saved: u64) -> Result<u64> {
    credentials::update(|current| current.set_group_ids(id_argument(real), id_argument(effective), id_argument(saved)))
        .map(|_| 0)
}

/// `getresuid` (`groups` false) or `getresgid`: three ids, written to three
/// addresses.
pub fn get_three_ids(real: u64, effective: u64, saved: u64, groups: bool) -> Result<u64> {
    let credentials = credentials::current();
    let ids = if groups {
        [credentials.gid, credentials.egid, credentials.sgid]
    } else {
        [credentials.uid, credentials.euid, credentials.suid]
    };
    for (address, id) in [real, effective, saved].into_iter().zip(ids) {
        user::write_value(address, &id)?;
    }
    Ok(0)
}

/// `setfsuid` and `setfsgid`: eliOS checks file access with the effective
/// ids, so these just report them (which is all most callers want).
pub fn fs_id(groups: bool) -> Result<u64> {
    let credentials = credentials::current();
    Ok(u64::from(if groups { credentials.egid } else { credentials.euid }))
}

/// `getgroups(size, list)`: with size 0, just the count.
pub fn getgroups(size: u64, list: u64) -> Result<u64> {
    let credentials = credentials::current();
    let count = credentials.groups.len();
    let size = size as u32 as usize;
    if size == 0 {
        return Ok(count as u64);
    }
    if size < count {
        return Err(Errno::EINVAL);
    }
    let bytes: alloc::vec::Vec<u8> = credentials.groups.iter().flat_map(|group| group.to_le_bytes()).collect();
    user::write_bytes(list, &bytes)?;
    Ok(count as u64)
}

pub fn setgroups(size: u64, list: u64) -> Result<u64> {
    let size = size as u32 as usize;
    if size > credentials::MAX_GROUPS {
        return Err(Errno::EINVAL);
    }
    let mut groups = alloc::vec::Vec::with_capacity(size);
    for index in 0..size {
        groups.push(user::read_value::<u32>(list + index as u64 * 4)?);
    }
    credentials::update(|current| current.set_groups(groups)).map(|_| 0)
}

/// `umask`: sets the mask, returning the old one.
pub fn umask(mask: u64) -> Result<u64> {
    Ok(u64::from(process::current().set_umask(mask as u32)))
}

const SIGEV_SIGNAL: u32 = 0;
const SIGEV_NONE: u32 = 1;
const SIGEV_THREAD_ID: u32 = 4;
const NANOS: u64 = 1_000_000_000;

/// The clocks timers can use; all count the same way here.
fn timer_clock(clock: u64) -> Result<()> {
    match clock as i32 {
        0 | 1 | 4..=9 => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

fn read_timespec(address: u64) -> Result<u64> {
    let [seconds, nanoseconds]: [i64; 2] = user::read_value(address)?;
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(Errno::EINVAL);
    }
    Ok((seconds as u64).saturating_mul(NANOS).saturating_add(nanoseconds as u64))
}

fn timespec(nanoseconds: u64) -> [u64; 2] {
    [nanoseconds / NANOS, nanoseconds % NANOS]
}

/// `timer_create(clock, event, id)`. Without an event, the timer sends
/// SIGALRM.
pub fn timer_create(clock: u64, event: u64, id_address: u64) -> Result<u64> {
    timer_clock(clock)?;
    let pid = process::current().pid;
    let (signal, notify, value) = if event == 0 {
        (signal::SIGALRM, timers::Notify::Signal, None)
    } else {
        // struct sigevent: value, signal, notify, then the thread id.
        let [value, words, tid]: [u64; 3] = user::read_value(event)?;
        let (signal, kind) = (words as u32, (words >> 32) as u32);
        let notify = match kind {
            SIGEV_SIGNAL => timers::Notify::Signal,
            SIGEV_NONE => timers::Notify::None,
            SIGEV_THREAD_ID => timers::Notify::Thread(tid as u32),
            _ => return Err(Errno::EINVAL),
        };
        (signal, notify, Some(value))
    };
    let id = timers::create(pid, signal, notify, value)?;
    user::write_value(id_address, &id)?;
    Ok(0)
}

/// `timer_settime(id, flags, new, old)`: itimerspecs (interval, value).
pub fn timer_settime(id: u64, flags: u64, new: u64, old: u64) -> Result<u64> {
    let pid = process::current().pid;
    let interval = read_timespec(new)?;
    let mut value = read_timespec(new + 16)?;
    if flags & TIMER_ABSTIME != 0 && value != 0 {
        // An absolute time on the clock: how long from now.
        value = value.saturating_sub(time::unix_nanos()).max(1);
    }
    let (left, old_interval) = timers::set(pid, id as i32, value, interval)?;
    if old != 0 {
        user::write_value(old, &[timespec(old_interval), timespec(left)])?;
    }
    Ok(0)
}

pub fn timer_gettime(id: u64, current: u64) -> Result<u64> {
    let (left, interval) = timers::get(process::current().pid, id as i32)?;
    user::write_value(current, &[timespec(interval), timespec(left)])?;
    Ok(0)
}

pub fn timer_getoverrun(id: u64) -> Result<u64> {
    timers::overrun(process::current().pid, id as i32).map(u64::from)
}

pub fn timer_delete(id: u64) -> Result<u64> {
    timers::delete(process::current().pid, id as i32).map(|_| 0)
}

const ITIMER_REAL: u64 = 0;

fn read_timeval(address: u64) -> Result<u64> {
    let [seconds, microseconds]: [i64; 2] = user::read_value(address)?;
    if seconds < 0 || !(0..1_000_000).contains(&microseconds) {
        return Err(Errno::EINVAL);
    }
    Ok(seconds as u64 * NANOS + microseconds as u64 * 1000)
}

fn timeval(nanoseconds: u64) -> [u64; 2] {
    [nanoseconds / NANOS, nanoseconds % NANOS / 1000]
}

/// `setitimer(which, new, old)`: only the real-time timer (SIGALRM).
pub fn setitimer(which: u64, new: u64, old: u64) -> Result<u64> {
    if which != ITIMER_REAL {
        return Err(Errno::EINVAL);
    }
    let (interval, value) = (read_timeval(new)?, read_timeval(new + 16)?);
    let (left, old_interval) = timers::set(process::current().pid, timers::INTERVAL_TIMER, value, interval)?;
    if old != 0 {
        user::write_value(old, &[timeval(old_interval), timeval(left)])?;
    }
    Ok(0)
}

pub fn getitimer(which: u64, current: u64) -> Result<u64> {
    if which != ITIMER_REAL {
        return Err(Errno::EINVAL);
    }
    let (left, interval) = timers::get(process::current().pid, timers::INTERVAL_TIMER)?;
    user::write_value(current, &[timeval(interval), timeval(left)])?;
    Ok(0)
}

/// `alarm(seconds)`: SIGALRM after that long (0 cancels). Returns the
/// seconds that were left on the previous alarm.
pub fn alarm(seconds: u64) -> Result<u64> {
    let (left, _) = timers::set(process::current().pid, timers::INTERVAL_TIMER, seconds * NANOS, 0)?;
    Ok(left.div_ceil(NANOS))
}

/// `rt_sigtimedwait(set, info, timeout, size)`.
pub fn sigtimedwait(set: u64, info: u64, timeout: u64, size: u64) -> Result<u64> {
    if size != 8 {
        return Err(Errno::EINVAL);
    }
    let set: u64 = user::read_value(set)?;
    let deadline = if timeout == 0 { None } else { Some(time::ticks() + read_timespec(timeout)?.div_ceil(1_000_000)) };
    let (signal, details) = signal::wait_for(&process::current_thread(), set, deadline)?;
    if info != 0 {
        let mut whole = [0u8; 128];
        whole[..32].copy_from_slice(&details);
        user::write_value(info, &whole)?;
    }
    Ok(u64::from(signal))
}
