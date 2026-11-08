//! System information, time, and eliOS's own system calls.

use alloc::string::String;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::memory::{frames, heap};
use crate::process::{self, user};
use crate::sync::SpinLock;
use crate::{log, power, random, time};

/// The machine's name, read from /etc/hostname at boot.
pub static HOSTNAME: SpinLock<String> = SpinLock::new(String::new());
/// The kernel release `uname` reports: like Linux's, for programs that
/// check for a recent enough kernel.
pub const KERNEL_RELEASE: &str = "6.8.0-eliOS";

/// `uname`: six 65-byte strings.
pub fn uname(address: u64) -> Result<u64> {
    // The release looks like Linux's so that programs which check for a
    // minimum kernel version accept eliOS.
    let hostname = HOSTNAME.lock().clone();
    let fields = ["eliOS", &hostname, KERNEL_RELEASE, concat!("eliOS ", env!("CARGO_PKG_VERSION")), "x86_64", ""];
    let mut buffer = [0u8; 65 * 6];
    for (index, field) in fields.iter().enumerate() {
        let bytes = field.as_bytes();
        let len = bytes.len().min(64);
        buffer[index * 65..index * 65 + len].copy_from_slice(&bytes[..len]);
    }
    user::write_bytes(address, &buffer)?;
    Ok(0)
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_REALTIME_COARSE: u64 = 5;

fn clock_now(clock: u64) -> Result<u64> {
    match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => Ok(time::unix_nanos()),
        // Monotonic, boot time and the CPU-time clocks all count from boot.
        1..=4 | 6 | 7 => Ok(time::uptime_nanos()),
        _ => Err(Errno::EINVAL),
    }
}

pub fn clock_gettime(clock: u64, timespec: u64) -> Result<u64> {
    let now = clock_now(clock)?;
    user::write_value(timespec, &[now / 1_000_000_000, now % 1_000_000_000])?;
    Ok(0)
}

pub fn clock_getres(clock: u64, timespec: u64) -> Result<u64> {
    clock_now(clock)?;
    if timespec != 0 {
        user::write_value(timespec, &[0u64, 1])?;
    }
    Ok(0)
}

pub fn gettimeofday(timeval: u64) -> Result<u64> {
    if timeval != 0 {
        let now = time::unix_nanos();
        user::write_value(timeval, &[now / 1_000_000_000, now % 1_000_000_000 / 1000])?;
    }
    Ok(0)
}

pub fn time(address: u64) -> Result<u64> {
    let now = time::unix_time();
    if address != 0 {
        user::write_value(address, &now)?;
    }
    Ok(now)
}

/// `getrusage(who, usage)`: CPU time used and the largest resident size.
/// Firefox asks many times a second (to watch its own CPU use), so this
/// must be cheap and never fail. The scheduler counts running time but
/// not where it went, so all of it is reported as user time. Children's
/// use (`RUSAGE_CHILDREN`) isn't kept once they're reaped: it reads zero.
pub fn getrusage(who: u64, usage: u64) -> Result<u64> {
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_THREAD: i32 = 1;
    const RUSAGE_CHILDREN: i32 = -1;
    let process = process::current();
    let ids: Vec<crate::task::ThreadId> = match who as i32 {
        RUSAGE_SELF => process.threads.lock().iter().map(|thread| thread.id).collect(),
        RUSAGE_THREAD => alloc::vec![crate::task::current()],
        RUSAGE_CHILDREN => Vec::new(),
        _ => return Err(Errno::EINVAL),
    };
    let milliseconds = crate::task::cpu_ticks_of(&ids);
    // struct rusage: user and system `timeval`s, then 14 longs, the first
    // of which is the largest resident set size in KiB.
    let mut fields = [0u64; 18];
    fields[0] = milliseconds / 1000;
    fields[1] = milliseconds % 1000 * 1000;
    if who as i32 != RUSAGE_CHILDREN {
        fields[4] = process.space.lock().as_ref().map_or(0, process::AddressSpace::resident_bytes) / 1024;
    }
    user::write_value(usage, &fields)?;
    Ok(0)
}

/// `sysinfo`: uptime, memory and process count.
pub fn sysinfo(address: u64) -> Result<u64> {
    let memory = frames::stats();
    let mut info = [0u64; 14];
    info[0] = time::uptime_nanos() / 1_000_000_000;
    info[4] = memory.managed_bytes;
    info[5] = memory.free_bytes;
    info[7] = heap::stats().bytes_mapped;
    // procs (u16) sits at byte 80; mem_unit (u32) at 104.
    info[10] = process::count() as u64;
    info[13] = 1;
    user::write_value(address, &info)?;
    Ok(0)
}

pub fn getrandom(buffer: u64, len: u64) -> Result<u64> {
    let buffer = user::buffer_mut(buffer, len as usize)?;
    random::fill(buffer);
    Ok(len)
}

const RLIMIT_STACK: u64 = 3;
const RLIMIT_NOFILE: u64 = 7;
const RLIM_INFINITY: u64 = u64::MAX;

pub fn prlimit(resource: u64, old_limit: u64) -> Result<u64> {
    if old_limit != 0 {
        let limit = match resource {
            RLIMIT_STACK => 8 << 20,
            RLIMIT_NOFILE => crate::fs::MAX_FILES as u64,
            _ => RLIM_INFINITY,
        };
        user::write_value(old_limit, &[limit, limit])?;
    }
    Ok(0)
}

const REBOOT_MAGIC1: u64 = 0xFEE1_DEAD;
const REBOOT_CMD_RESTART: u64 = 0x0123_4567;
const REBOOT_CMD_HALT: u64 = 0xCDEF_0123;
const REBOOT_CMD_POWER_OFF: u64 = 0x4321_FEDC;

pub fn reboot(magic: u64, command: u64) -> Result<u64> {
    if !crate::process::credentials::current().is_root() {
        return Err(Errno::EPERM);
    }
    if magic as u32 as u64 != REBOOT_MAGIC1 {
        return Err(Errno::EINVAL);
    }
    let command = command as u32 as u64;
    if ![REBOOT_CMD_RESTART, REBOOT_CMD_POWER_OFF, REBOOT_CMD_HALT].contains(&command) {
        return Err(Errno::EINVAL);
    }
    crate::logfile::flush();
    // Disks are left consistent and marked cleanly unmounted.
    crate::fs::mount::shut_down();
    match command {
        REBOOT_CMD_RESTART => power::restart(),
        REBOOT_CMD_POWER_OFF | REBOOT_CMD_HALT => power::power_off(),
        _ => Err(Errno::EINVAL),
    }
}

/// eliOS system call: a table of processes, for `ps`. Each entry is a
/// `struct eli_process` (see the user library). Returns the entry count.
pub fn process_list(buffer: u64, capacity: u64) -> Result<u64> {
    let mut output = Vec::new();
    let processes = process::list();
    for info in processes.iter().take(capacity as usize) {
        output.extend_from_slice(&info.pid.to_le_bytes());
        output.extend_from_slice(&info.parent.to_le_bytes());
        output.push(info.state as u8);
        output.extend_from_slice(&[0; 3]);
        // Who it runs as (its effective user).
        output.extend_from_slice(&info.credentials.euid.to_le_bytes());
        output.extend_from_slice(&info.cpu_ms.to_le_bytes());
        output.extend_from_slice(&info.memory.to_le_bytes());
        let mut name = [0u8; 32];
        let len = info.name.len().min(31);
        name[..len].copy_from_slice(&info.name.as_bytes()[..len]);
        output.extend_from_slice(&name);
    }
    user::write_bytes(buffer, &output)?;
    Ok(processes.len().min(capacity as usize) as u64)
}

/// eliOS system call: copies the kernel log (what `dmesg` shows). Returns
/// the number of bytes copied.
pub fn kernel_log(buffer: u64, len: u64) -> Result<u64> {
    let mut text = alloc::string::String::new();
    log::replay(&mut text);
    let bytes = text.as_bytes();
    // The most recent part, if it doesn't all fit.
    let start = bytes.len().saturating_sub(len as usize);
    user::write_bytes(buffer, &bytes[start..])?;
    Ok((bytes.len() - start) as u64)
}

/// `sethostname(name, len)`: root only.
pub fn sethostname(name: u64, len: u64) -> Result<u64> {
    if !crate::process::credentials::current().is_root() {
        return Err(Errno::EPERM);
    }
    if len > 64 {
        return Err(Errno::EINVAL);
    }
    let bytes = user::buffer(name, len as usize)?;
    let name = core::str::from_utf8(bytes).map_err(|_| Errno::EINVAL)?;
    *HOSTNAME.lock() = String::from(name);
    Ok(0)
}

const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `capget`: Linux's capabilities, which here follow the user: root has
/// them all, others none.
pub fn capget(header: u64, data: u64) -> Result<u64> {
    let [version, _pid]: [u32; 2] = user::read_value(header)?;
    if version != CAPABILITY_VERSION_3 {
        user::write_value(header, &CAPABILITY_VERSION_3)?;
        return Err(Errno::EINVAL);
    }
    if data != 0 {
        let all = if crate::process::credentials::current().euid == 0 { u32::MAX } else { 0 };
        // Two sets of (effective, permitted, inheritable).
        user::write_value(data, &[all, all, 0, all, all, 0])?;
    }
    Ok(0)
}

/// `capset`: accepted (capabilities aren't separate from being root).
pub fn capset(header: u64, _data: u64) -> Result<u64> {
    let [version, _pid]: [u32; 2] = user::read_value(header)?;
    if version != CAPABILITY_VERSION_3 {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `membarrier`: makes every other CPU pass a memory barrier, by
/// interrupting them all (the TLB shootdown path does exactly that).
/// Garbage collectors and some lock-free code use it.
pub fn membarrier(command: u64) -> Result<u64> {
    const QUERY: u64 = 0;
    const GLOBAL: u64 = 1;
    const PRIVATE_EXPEDITED: u64 = 8;
    const REGISTER_PRIVATE_EXPEDITED: u64 = 16;
    match command {
        QUERY => Ok(GLOBAL | PRIVATE_EXPEDITED | REGISTER_PRIVATE_EXPEDITED),
        GLOBAL | PRIVATE_EXPEDITED => {
            crate::arch::smp::flush_tlb(0, 0);
            Ok(0)
        }
        REGISTER_PRIVATE_EXPEDITED => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}
