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
