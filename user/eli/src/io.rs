//! Reading and writing file descriptors, and the print macros.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sys::{self, Result};

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
pub const STDERR: i32 = 2;

pub fn read(fd: i32, buffer: &mut [u8]) -> Result<usize> {
    loop {
        // SAFETY: the buffer is valid for its length.
        match unsafe { sys::call(sys::READ, [fd as usize, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0]) } {
            // A signal that doesn't end us (one we ignore) can still
            // interrupt the read; just retry.
            Err(sys::Error::INTERRUPTED) => continue,
            result => return result,
        }
    }
}

pub fn write(fd: i32, data: &[u8]) -> Result<usize> {
    // SAFETY: the buffer is valid for its length.
    unsafe { sys::call(sys::WRITE, [fd as usize, data.as_ptr() as usize, data.len(), 0, 0, 0]) }
}

pub fn write_all(fd: i32, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let written = write(fd, data)?;
        data = &data[written..];
    }
    Ok(())
}

/// Reads until end of file.
pub fn read_to_end(fd: i32) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = alloc::vec![0; 64 * 1024];
    loop {
        let count = read(fd, &mut buffer)?;
        if count == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&buffer[..count]);
    }
}

pub fn close(fd: i32) -> Result<()> {
    // SAFETY: closing takes no pointers.
    unsafe { sys::call(sys::CLOSE, [fd as usize, 0, 0, 0, 0, 0]) }.map(|_| ())
}

/// Makes `new` refer to the same open file as `old`.
pub fn dup2(old: i32, new: i32) -> Result<()> {
    // SAFETY: takes no pointers.
    unsafe { sys::call(sys::DUP2, [old as usize, new as usize, 0, 0, 0, 0]) }.map(|_| ())
}

/// A pipe: (read end, write end). Both are closed by `exec`, so only the
/// descriptors a child explicitly moves into place survive into it.
pub fn pipe() -> Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // O_CLOEXEC.
    // SAFETY: room for two descriptors.
    unsafe { sys::call(sys::PIPE2, [fds.as_mut_ptr() as usize, 0o2000000, 0, 0, 0, 0]) }?;
    Ok((fds[0], fds[1]))
}

/// Writes formatted text to a descriptor, ignoring errors (as `print!`
/// does).
pub struct Writer(pub i32);

impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_all(self.0, text.as_bytes()).map_err(|_| fmt::Error)
    }
}

#[doc(hidden)]
pub fn _print(fd: i32, args: fmt::Arguments) {
    // Format first, then write once: output from different programs
    // sharing a terminal stays in whole pieces.
    let text = alloc::fmt::format(args);
    let _ = write_all(fd, text.as_bytes());
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDOUT, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDOUT, format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDERR, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDERR, format_args!("{}\n", format_args!($($arg)*))) };
}

/// Reads a descriptor line by line.
pub struct Lines {
    fd: i32,
    buffer: Vec<u8>,
    finished: bool,
}

impl Lines {
    pub fn new(fd: i32) -> Self {
        Self { fd, buffer: Vec::new(), finished: false }
    }

    /// The next line without its newline, or `None` at end of file.
    pub fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                return Ok(Some(String::from_utf8_lossy(&line[..end]).into_owned()));
            }
            if self.finished {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let line = core::mem::take(&mut self.buffer);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let mut chunk = [0; 1024];
            let count = read(self.fd, &mut chunk)?;
            if count == 0 {
                self.finished = true;
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }
}
