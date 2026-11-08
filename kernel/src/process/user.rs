//! Reading and writing user memory on a program's behalf.
//!
//! Every access is checked first: the range must lie in areas the program
//! may access, and any pages not yet touched are mapped. After that the
//! access can't fault, so the kernel simply reads or writes through the
//! user address (the program's address space is the active one).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::address_space;
use crate::errno::{Errno, Result};
use crate::task;

/// Checks that the running process may access `address..address + len`,
/// mapping the pages if needed.
pub fn check(address: u64, len: usize, write: bool) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let process = task::current_process().ok_or(Errno::EFAULT)?;
    loop {
        // A page that must be fetched first (perhaps from a disk) is
        // fetched without the lock, then the check starts over.
        let pending = process.space.lock().as_mut().ok_or(Errno::EFAULT)?.prepare_access(address, len as u64, write)?;
        let Some((page, _)) = pending else { return Ok(()) };
        if !address_space::resolve_fault(&process.space, page, write, false) {
            return Err(Errno::EFAULT);
        }
    }
}

pub fn read_value<T: Copy>(address: u64) -> Result<T> {
    check(address, size_of::<T>(), false)?;
    // SAFETY: checked above; unaligned because user pointers can be.
    Ok(unsafe { core::ptr::read_unaligned(address as *const T) })
}

pub fn write_value<T: Copy>(address: u64, value: &T) -> Result<()> {
    check(address, size_of::<T>(), true)?;
    // SAFETY: as for `read_value`.
    unsafe { core::ptr::write_unaligned(address as *mut T, *value) };
    Ok(())
}

/// Atomically replaces the `u32` at `address` with `new` if it holds
/// `current` (for PI futexes, whose variable both sides change). Returns
/// what it held.
pub fn compare_exchange_u32(address: u64, current: u32, new: u32) -> Result<u32> {
    if !address.is_multiple_of(4) {
        return Err(Errno::EINVAL);
    }
    check(address, 4, true)?;
    // SAFETY: checked above, and aligned.
    let atomic = unsafe { core::sync::atomic::AtomicU32::from_ptr(address as *mut u32) };
    Ok(atomic.compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst).unwrap_or_else(|held| held))
}

pub fn write_bytes(address: u64, data: &[u8]) -> Result<()> {
    check(address, data.len(), true)?;
    // SAFETY: checked above.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), address as *mut u8, data.len()) };
    Ok(())
}

pub fn read_bytes(address: u64, len: usize) -> Result<Vec<u8>> {
    check(address, len, false)?;
    // SAFETY: checked above.
    Ok(unsafe { core::slice::from_raw_parts(address as *const u8, len) }.to_vec())
}

/// A user buffer the kernel can fill in place (for `read`).
///
/// The slice stays valid while the system call runs unless another thread
/// of the program unmaps it meanwhile (see "Limits" in HOW-IT-WORKS).
pub fn buffer_mut(address: u64, len: usize) -> Result<&'static mut [u8]> {
    check(address, len, true)?;
    if len == 0 {
        return Ok(&mut []);
    }
    // SAFETY: checked above; see the note on lifetime.
    Ok(unsafe { core::slice::from_raw_parts_mut(address as *mut u8, len) })
}

/// A user buffer the kernel can read in place (for `write`).
pub fn buffer(address: u64, len: usize) -> Result<&'static [u8]> {
    check(address, len, false)?;
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: as for `buffer_mut`.
    Ok(unsafe { core::slice::from_raw_parts(address as *const u8, len) })
}

const PATH_MAX: usize = 4096;

/// Reads a NUL-terminated string (a path, say).
pub fn read_string(address: u64) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        // Read up to the end of each page in turn: the string may end
        // before the next page, which needn't be accessible.
        let chunk_address = address.checked_add(bytes.len() as u64).ok_or(Errno::EFAULT)?;
        let chunk_len = (4096 - chunk_address % 4096) as usize;
        let chunk = read_bytes(chunk_address, chunk_len)?;
        match chunk.iter().position(|&b| b == 0) {
            Some(end) => {
                bytes.extend_from_slice(&chunk[..end]);
                break;
            }
            None => bytes.extend_from_slice(&chunk),
        }
        if bytes.len() > PATH_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
    }
    String::from_utf8(bytes).map_err(|_| Errno::EINVAL)
}

/// Reads a NULL-terminated array of string pointers (argv or envp).
pub fn read_string_array(address: u64) -> Result<Vec<String>> {
    let mut strings = Vec::new();
    if address == 0 {
        return Ok(strings);
    }
    for index in 0..4096u64 {
        let pointer: u64 = read_value(address + index * 8)?;
        if pointer == 0 {
            return Ok(strings);
        }
        strings.push(read_string(pointer)?);
    }
    Err(Errno::E2BIG)
}
