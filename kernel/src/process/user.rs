//! Reading and writing user memory on a program's behalf.
//!
//! Every access is checked first: the range must lie in areas the program
//! may access, and any pages not yet touched are mapped. After that the
//! access can't fault, so the kernel simply reads or writes through the
//! user address (the program's address space is the active one).

use alloc::string::String;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::task;

/// Checks that the running process may access `address..address + len`,
/// mapping the pages if needed.
pub fn check(address: u64, len: usize, write: bool) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let process = task::current_process().ok_or(Errno::EFAULT)?;
    let mut space = process.space.lock();
    space.as_mut().ok_or(Errno::EFAULT)?.prepare_access(address, len as u64, write)
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
/// The slice stays valid while the system call runs: processes have a
/// single thread, so nothing can unmap it meanwhile.
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
