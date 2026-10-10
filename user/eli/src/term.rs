//! Terminal control: raw mode, window size, and the foreground process
//! group.

use alloc::string::String;

use crate::sys::{self, Result};

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;

const ICRNL: u32 = 0o400;
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;

/// The kernel's `struct termios`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line: u8,
    control_chars: [u8; 19],
}

fn ioctl(fd: i32, request: usize, argument: usize) -> Result<usize> {
    // SAFETY: callers pass arguments matching the request.
    unsafe { sys::call(sys::IOCTL, [fd as usize, request, argument, 0, 0, 0]) }
}

pub fn settings(fd: i32) -> Result<Termios> {
    let mut termios = core::mem::MaybeUninit::<Termios>::zeroed();
    ioctl(fd, TCGETS, termios.as_mut_ptr() as usize)?;
    // SAFETY: zeroed, then filled by the kernel.
    Ok(unsafe { termios.assume_init() })
}

pub fn set_settings(fd: i32, termios: &Termios) -> Result<()> {
    ioctl(fd, TCSETS, core::ptr::from_ref(termios) as usize).map(|_| ())
}

pub fn is_terminal(fd: i32) -> bool {
    settings(fd).is_ok()
}

/// Switches to raw mode (keys arrive one at a time, unechoed, Ctrl+C as a
/// byte) and returns the previous settings, to restore later.
pub fn enter_raw_mode(fd: i32) -> Result<Termios> {
    let original = settings(fd)?;
    let mut raw = original;
    raw.local_flags &= !(ICANON | ECHO | ISIG);
    raw.input_flags &= !ICRNL;
    set_settings(fd, &raw)?;
    Ok(original)
}

/// Columns and rows.
pub fn size(fd: i32) -> (u16, u16) {
    let mut size = [0u16; 4];
    match ioctl(fd, TIOCGWINSZ, size.as_mut_ptr() as usize) {
        Ok(_) if size[1] > 0 => (size[1], size[0]),
        _ => (80, 24),
    }
}

/// Makes a process group the terminal's foreground: the one that gets
/// Ctrl+C.
pub fn set_foreground_group(fd: i32, group: u32) -> Result<()> {
    ioctl(fd, TIOCSPGRP, &raw const group as usize).map(|_| ())
}

/// Asks for a password: shows `prompt`, then reads a line without echoing
/// it. Returns `None` at end of input. When input isn't a terminal (a
/// script), the line is just read.
pub fn read_password(prompt: &str) -> Option<String> {
    use crate::io::{self, STDIN};
    let original = settings(STDIN).ok();
    // The prompt goes to the terminal even if output is redirected.
    let _ = io::write_all(io::STDERR, prompt.as_bytes());
    if let Some(original) = original {
        let mut quiet = original;
        quiet.local_flags &= !ECHO;
        let _ = set_settings(STDIN, &quiet);
    }
    let mut line = alloc::vec::Vec::new();
    let mut byte = [0u8];
    let complete = loop {
        match io::read(STDIN, &mut byte) {
            Ok(1) if byte[0] == b'\n' => break true,
            Ok(1) => line.push(byte[0]),
            _ => break !line.is_empty(),
        }
    };
    if let Some(original) = original {
        let _ = set_settings(STDIN, &original);
        let _ = io::write_all(io::STDERR, b"\n");
    }
    complete.then(|| String::from_utf8_lossy(&line).trim_end_matches('\r').into())
}
