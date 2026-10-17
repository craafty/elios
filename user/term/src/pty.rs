//! Starting the shell on a pseudo-terminal: the terminal holds the master
//! side, and the shell gets the slave side as its controlling terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eli::sys::{Result, call};
use ui::sys;

const SETSID: usize = 112;
const SIGPIPE: i32 = 13;
const O_NOCTTY: usize = 0o400;
const TIOCSCTTY: usize = 0x540E;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;

/// Opens a pty and runs `command` on it; returns the master side (non-
/// blocking) and the program's pid.
pub fn spawn(command: &[String], columns: usize, rows: usize) -> Result<(i32, u32)> {
    let master = sys::open("/dev/ptmx", sys::O_RDWR | O_NOCTTY | sys::O_NONBLOCK)?;
    let unlock: i32 = 0;
    let mut number: u32 = 0;
    // SAFETY: both ioctls take a pointer to an int, which these are.
    unsafe {
        sys::ioctl(master, TIOCSPTLCK, &unlock as *const i32 as usize)?;
        sys::ioctl(master, TIOCGPTN, &mut number as *mut u32 as usize)?;
    }
    set_size(master, columns, rows);
    let slave_path = format!("/dev/pts/{number}");

    let mut environment: Vec<String> = eli::env::vars()
        .into_iter()
        .filter(|variable| !["TERM=", "COLORTERM=", "LINES=", "COLUMNS="].iter().any(|name| variable.starts_with(name)))
        .collect();
    environment.push(String::from("TERM=xterm-256color"));
    environment.push(String::from("COLORTERM=truecolor"));
    let path = if command[0].contains('/') {
        command[0].clone()
    } else {
        eli::process::find_program(&command[0]).unwrap_or_else(|| command[0].clone())
    };

    let pid = eli::process::fork()?;
    if pid == 0 {
        // A session of its own, with the pty as its terminal.
        // SAFETY: setsid takes no arguments.
        let _ = unsafe { call(SETSID, [0; 6]) };
        let slave = match sys::open(&slave_path, sys::O_RDWR) {
            Ok(fd) => fd,
            Err(error) => {
                eli::eprintln!("term: can't open {slave_path}: {error}");
                eli::process::exit(127);
            }
        };
        // SAFETY: TIOCSCTTY takes an int argument, not a pointer.
        let _ = unsafe { sys::ioctl(slave, TIOCSCTTY, 0) };
        for fd in 0..3 {
            let _ = eli::io::dup2(slave, fd);
        }
        let _ = eli::io::close(slave);
        // Whatever the desktop ignores, the shell shouldn't.
        for signal in [eli::process::SIGINT, eli::process::SIGQUIT, SIGPIPE] {
            eli::process::default_signal(signal);
        }
        let mut arguments = Vec::from(command);
        arguments[0] = String::from(command[0].rsplit('/').next().unwrap_or(&command[0]));
        let error = eli::process::exec(&path, &arguments, &environment);
        eli::eprintln!("term: can't run {path}: {error}");
        eli::process::exit(127);
    }
    Ok((master, pid))
}

/// Tells the pty (and so the programs on it, by SIGWINCH) its size.
pub fn set_size(master: i32, columns: usize, rows: usize) {
    let size: [u16; 4] = [rows.min(65535) as u16, columns.min(65535) as u16, 0, 0];
    // SAFETY: TIOCSWINSZ reads a struct winsize, four u16s.
    let _ = unsafe { sys::ioctl(master, TIOCSWINSZ, size.as_ptr() as usize) };
}
