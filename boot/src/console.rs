//! Text output through the firmware console (screen, and serial on most
//! firmware). Output silently stops once boot services are exited.

use core::fmt;

use crate::uefi::{self, SimpleTextOutput};

pub const LIGHT_GRAY: usize = 0x07;
pub const LIGHT_CYAN: usize = 0x0B;
pub const LIGHT_RED: usize = 0x0C;

pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let Some(system_table) = uefi::system_table() else {
            return Ok(());
        };
        let out = system_table.con_out;

        // UEFI wants NUL-terminated UCS-2 with CRLF line endings. Convert in
        // fixed-size chunks so we never need an allocator.
        let mut buf = [0u16; 128];
        let mut len = 0;
        for c in s.chars() {
            // Room for '\r', a surrogate pair and the terminating NUL.
            if len + 4 > buf.len() {
                flush(out, &mut buf, &mut len);
            }
            if c == '\n' {
                buf[len] = u16::from(b'\r');
                len += 1;
            }
            len += c.encode_utf16(&mut buf[len..]).len();
        }
        flush(out, &mut buf, &mut len);
        Ok(())
    }
}

fn flush(out: *mut SimpleTextOutput, buf: &mut [u16], len: &mut usize) {
    if *len == 0 {
        return;
    }
    buf[*len] = 0;
    // SAFETY: `out` is the firmware console and `buf` is NUL-terminated.
    unsafe { ((*out).output_string)(out, buf.as_ptr()) };
    *len = 0;
}

pub fn set_color(attribute: usize) {
    if let Some(system_table) = uefi::system_table() {
        let out = system_table.con_out;
        // SAFETY: `out` is the firmware console.
        unsafe { ((*out).set_attribute)(out, attribute) };
    }
}

pub fn clear() {
    if let Some(system_table) = uefi::system_table() {
        let out = system_table.con_out;
        // SAFETY: `out` is the firmware console.
        unsafe { ((*out).clear_screen)(out) };
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::console::Console, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
