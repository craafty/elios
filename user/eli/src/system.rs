//! Information about the system.

use alloc::string::String;

use crate::sys;

/// What `uname` reports.
pub struct Uname {
    pub system: String,
    pub hostname: String,
    pub release: String,
    pub version: String,
    pub machine: String,
}

/// How many CPUs programs can run on.
pub fn cpu_count() -> usize {
    let mut mask = [0u64; 16];
    // SAFETY: the buffer has room for the mask, whose size is passed.
    let result =
        unsafe { sys::syscall(sys::SCHED_GETAFFINITY, [0, size_of_val(&mask), mask.as_mut_ptr() as usize, 0, 0, 0]) };
    if result <= 0 {
        return 1;
    }
    mask.iter().map(|word| word.count_ones() as usize).sum::<usize>().max(1)
}

pub fn uname() -> Uname {
    let mut buffer = [0u8; 65 * 6];
    // SAFETY: room for a `struct utsname`.
    unsafe { sys::syscall(sys::UNAME, [buffer.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) };
    let field = |index: usize| {
        let bytes = &buffer[index * 65..(index + 1) * 65];
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(65);
        String::from_utf8_lossy(&bytes[..len]).into_owned()
    };
    Uname { system: field(0), hostname: field(1), release: field(2), version: field(3), machine: field(4) }
}
