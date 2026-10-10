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

/// `uname`: six 65-byte strings.
pub fn uname(address: u64) -> Result<u64> {
    // The release looks like Linux's so that programs which check for a
    // minimum kernel version accept eliOS.
    let hostname = HOSTNAME.lock().clone();
    let fields = ["eliOS", &hostname, "6.8.0-eliOS", concat!("eliOS ", env!("CARGO_PKG_VERSION")), "x86_64", ""];
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
    if magic as u32 as u64 != REBOOT_MAGIC1 {
        return Err(Errno::EINVAL);
    }
    match command as u32 as u64 {
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
        output.extend_from_slice(&[0; 7]);
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
