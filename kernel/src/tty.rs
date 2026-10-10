//! The console terminal (`/dev/console`).
//!
//! Input arrives from the keyboard and the serial port as bytes, the way a
//! terminal would send them. In canonical mode (the default) the terminal
//! edits a line at a time, echoing as you type, and programs read whole
//! lines. Programs can switch to raw mode with the standard termios ioctls.
//! Output goes to both the screen and the serial port.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, POLLIN, POLLOUT, ramfs};
use crate::process::{self, user};
use crate::sync::{Once, SpinLock};
use crate::task::WaitQueue;
use crate::{console, log};

// termios flag bits (Linux values).
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const CS8: u32 = 0o60;
const CREAD: u32 = 0o200;
const B38400: u32 = 0o17;
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;

// ioctl requests.
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TCFLSH: u64 = 0x540B;
const TIOCGPGRP: u64 = 0x540F;
const TIOCSPGRP: u64 = 0x5410;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const FIONREAD: u64 = 0x541B;
// The termios2 variants, which add explicit baud rates (newer C libraries
// use these).
const TCGETS2: u64 = 0x802C_542A;
const TCSETS2: u64 = 0x402C_542B;
const TCSETSW2: u64 = 0x402C_542C;
const TCSETSF2: u64 = 0x402C_542D;
const BAUD_38400: u32 = 38_400;

/// The kernel's `struct termios`, as the ioctls exchange it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line: u8,
    control_chars: [u8; 19],
}

impl Termios {
    const DEFAULT: Self = Self {
        input_flags: ICRNL | IXON,
        output_flags: OPOST | ONLCR,
        control_flags: B38400 | CS8 | CREAD,
        local_flags: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
        line: 0,
        // ^C ^\ DEL ^U ^D, VTIME 0, VMIN 1, then the rarely used ones.
        control_chars: [3, 0x1C, 0x7F, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1A, 0, 0x12, 0x0F, 0x17, 0x16, 0, 0, 0],
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// Saw ESC.
    Start,
    /// Inside `ESC [ ...`.
    Sequence,
}

struct State {
    termios: Termios,
    /// The line being edited, in canonical mode.
    line: Vec<u8>,
    /// Bytes ready for programs to read.
    ready: VecDeque<u8>,
    /// Ctrl+D on an empty line: the next read returns end of file.
    end_of_file: bool,
    /// Escape sequences (arrow keys and such) are dropped in canonical mode.
    escape: Escape,
    /// The process group Ctrl+C is sent to.
    foreground: u32,
}

pub struct Tty {
    inode: u64,
    state: SpinLock<State>,
    readable: WaitQueue,
}

static CONSOLE: Once<Arc<Tty>> = Once::new();

pub fn console() -> Arc<Tty> {
    if let Some(tty) = CONSOLE.get() {
        return tty.clone();
    }
    let tty = Arc::new(Tty {
        inode: ramfs::next_inode_number(),
        state: SpinLock::new(State {
            termios: Termios::DEFAULT,
            line: Vec::new(),
            ready: VecDeque::new(),
            end_of_file: false,
            escape: Escape::None,
            foreground: 0,
        }),
        readable: WaitQueue::new(),
    });
    match CONSOLE.set(tty) {
        Ok(tty) => tty.clone(),
        Err(_) => CONSOLE.get().unwrap().clone(),
    }
}

/// Writes program output (or echo) to the screen and serial port.
fn output(bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        log::write_terminal(chunk.valid());
        if !chunk.invalid().is_empty() {
            log::write_terminal("\u{FFFD}");
        }
    }
}

/// Feeds bytes typed by the user. Callable from interrupt handlers.
pub fn input(bytes: &[u8]) {
    let tty = console();
    let mut interrupt_group = None;
    let mut wake = false;
    {
        let mut state = tty.state.lock();
        for &byte in bytes {
            let flags = state.termios.local_flags;
            let byte = if byte == b'\r' && state.termios.input_flags & ICRNL != 0 { b'\n' } else { byte };
            let echo = flags & ECHO != 0;

            if flags & ISIG != 0 && byte == state.termios.control_chars[0] {
                if echo {
                    output(b"^C\n");
                }
                state.line.clear();
                interrupt_group = Some(state.foreground);
                continue;
            }

            if flags & ICANON == 0 {
                state.ready.push_back(byte);
                if echo {
                    output(&[byte]);
                }
                wake = true;
                continue;
            }

            // Canonical mode: drop escape sequences, edit the line.
            match (state.escape, byte) {
                (Escape::None, 0x1B) => {
                    state.escape = Escape::Start;
                    continue;
                }
                (Escape::Start, b'[' | b'O') => {
                    state.escape = Escape::Sequence;
                    continue;
                }
                (Escape::Sequence, 0x40..=0x7E) | (Escape::Start, _) => {
                    state.escape = Escape::None;
                    continue;
                }
                (Escape::Sequence, _) => continue,
                (Escape::None, _) => {}
            }
            match byte {
                0x7F | 0x08 => {
                    // Remove one whole character, however many bytes it is.
                    while let Some(removed) = state.line.pop() {
                        if removed & 0xC0 != 0x80 {
                            if echo {
                                output(b"\x08 \x08");
                            }
                            break;
                        }
                    }
                }
                0x15 => {
                    // Ctrl+U: erase the whole line.
                    let characters = state.line.iter().filter(|&&b| b & 0xC0 != 0x80).count();
                    state.line.clear();
                    if echo {
                        for _ in 0..characters {
                            output(b"\x08 \x08");
                        }
                    }
                }
                0x04 => {
                    // Ctrl+D: hand over the line as is; on an empty line,
                    // that's end of file.
                    if state.line.is_empty() {
                        state.end_of_file = true;
                    }
                    let line = core::mem::take(&mut state.line);
                    state.ready.extend(line);
                    wake = true;
                }
                b'\n' => {
                    let mut line = core::mem::take(&mut state.line);
                    line.push(b'\n');
                    state.ready.extend(line);
                    if echo {
                        output(b"\n");
                    }
                    wake = true;
                }
                byte if byte >= 0x20 || byte == b'\t' => {
                    state.line.push(byte);
                    if echo {
                        output(&[byte]);
                    }
                }
                _ => {}
            }
        }
    }
    if wake {
        tty.readable.wake_all();
        fs::notify_poll();
    }
    if let Some(group) = interrupt_group {
        process::signal_group(group, process::SIGINT);
        tty.readable.wake_all();
    }
}

/// Columns and rows of the screen (or a standard terminal size without one).
fn window_size() -> (u16, u16) {
    console::with(|console| console.size()).map_or((80, 24), |(columns, rows)| (columns as u16, rows as u16))
}

impl Inode for Tty {
    fn metadata(&self) -> Metadata {
        Metadata { kind: FileKind::CharDevice, inode: self.inode, size: 0, permissions: 0o620, modified: 0 }
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.readable.wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut state = self.state.lock();
            if state.ready.is_empty() {
                return state.end_of_file.then(|| {
                    state.end_of_file = false;
                    Ok(0)
                });
            }
            let canonical = state.termios.local_flags & ICANON != 0;
            let mut count = 0;
            while count < buf.len() {
                let Some(byte) = state.ready.pop_front() else { break };
                buf[count] = byte;
                count += 1;
                // A canonical read returns at most one line.
                if canonical && byte == b'\n' {
                    break;
                }
            }
            Some(Ok(count))
        })
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        output(buf);
        Ok(buf.len())
    }

    fn poll(&self, _flags: u32) -> u16 {
        let state = self.state.lock();
        if state.ready.is_empty() && !state.end_of_file { POLLOUT } else { POLLIN | POLLOUT }
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            TCGETS => {
                let termios = self.state.lock().termios;
                user::write_value(argument, &termios)?;
            }
            TCGETS2 => {
                let termios = self.state.lock().termios;
                user::write_value(argument, &(termios, BAUD_38400, BAUD_38400))?;
            }
            TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                // termios2 starts with a termios, so the same read works.
                let termios: Termios = user::read_value(argument)?;
                let mut state = self.state.lock();
                if request == TCSETSF || request == TCSETSF2 {
                    state.ready.clear();
                }
                // Leaving canonical mode hands over the partial line.
                if termios.local_flags & ICANON == 0 {
                    let line = core::mem::take(&mut state.line);
                    state.ready.extend(line);
                }
                state.termios = termios;
            }
            TCFLSH => self.state.lock().ready.clear(),
            TIOCGPGRP => user::write_value(argument, &self.state.lock().foreground)?,
            TIOCSPGRP => self.state.lock().foreground = user::read_value(argument)?,
            TIOCGWINSZ => {
                let (columns, rows) = window_size();
                user::write_value(argument, &[rows, columns, 0, 0])?;
            }
            TIOCSWINSZ => {}
            FIONREAD => {
                let available = self.state.lock().ready.len() as u32;
                user::write_value(argument, &available)?;
            }
            _ => return Err(Errno::ENOTTY),
        }
        Ok(0)
    }
}
