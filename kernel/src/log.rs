//! Kernel output. Everything printed goes to the serial port and, once there
//! is one, the screen console. Debug messages go to serial only, to keep the
//! screen readable, and once programs are running (see [`quiet_console`])
//! so do info messages: they'd land in the middle of the shell's prompt.
//!
//! Output is also kept in an in-memory log, so messages printed before the
//! console existed can be shown on it afterwards. A second, larger log keeps
//! everything including debug messages, as plain text, for
//! [`logfile`](crate::logfile) to save to disk: on a laptop without a serial
//! port, that file is the only record of what happened.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;
use crate::{console, serial, time};

const LOG_SIZE: usize = 64 * 1024;

/// The most recent output, oldest bytes dropped first.
struct Log {
    bytes: [u8; LOG_SIZE],
    start: usize,
    len: usize,
}

impl fmt::Write for Log {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            if self.len == LOG_SIZE {
                self.start = (self.start + 1) % LOG_SIZE;
                self.len -= 1;
            }
            self.bytes[(self.start + self.len) % LOG_SIZE] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

static LOG: SpinLock<Log> = SpinLock::new(Log { bytes: [0; LOG_SIZE], start: 0, len: 0 });

const FULL_LOG_SIZE: usize = 512 * 1024;

/// Everything logged, debug messages included, without color escapes. Bytes
/// are numbered from the start of boot, so a reader can ask for what it
/// hasn't seen yet.
struct FullLog {
    bytes: [u8; FULL_LOG_SIZE],
    /// How many bytes have ever been written.
    written: u64,
    /// Inside an escape sequence being dropped: 1 after ESC, 2 after `ESC [`.
    escape: u8,
}

impl fmt::Write for FullLog {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            match (self.escape, byte) {
                (0, 0x1B) => self.escape = 1,
                (0, _) => {
                    self.bytes[(self.written % FULL_LOG_SIZE as u64) as usize] = byte;
                    self.written += 1;
                }
                (1, b'[') => self.escape = 2,
                // The final byte of a control sequence (or a two-byte escape).
                (1, _) | (2, 0x40..=0x7E) => self.escape = 0,
                _ => {}
            }
        }
        Ok(())
    }
}

static FULL_LOG: SpinLock<FullLog> = SpinLock::new(FullLog { bytes: [0; FULL_LOG_SIZE], written: 0, escape: 0 });

/// Appends the full log from byte `from` onwards to `out`, and returns the
/// position to continue from next time. Bytes overwritten before they were
/// read are replaced by a note saying how many were lost.
pub fn full_since(from: u64, out: &mut alloc::vec::Vec<u8>) -> u64 {
    let log = FULL_LOG.lock();
    let oldest = log.written.saturating_sub(FULL_LOG_SIZE as u64);
    if from < oldest {
        out.extend_from_slice(alloc::format!("\n[... {} bytes of log lost ...]\n", oldest - from).as_bytes());
    }
    for position in from.max(oldest)..log.written {
        out.push(log.bytes[(position % FULL_LOG_SIZE as u64) as usize]);
    }
    log.written
}

/// Writes the whole log so far to `out`.
pub fn replay(out: &mut impl fmt::Write) {
    let log = LOG.lock();
    let end = log.start + log.len;
    let (first, second) = if end <= LOG_SIZE {
        (&log.bytes[log.start..end], &[][..])
    } else {
        (&log.bytes[log.start..], &log.bytes[..end - LOG_SIZE])
    };
    for chunk in first.utf8_chunks().chain(second.utf8_chunks()) {
        let _ = out.write_str(chunk.valid());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    // Only constructed through `error!`, which nothing needs yet.
    #[allow(dead_code)]
    Error,
}

impl Level {
    /// The tag printed before each message, colored with ANSI escapes.
    fn tag(self) -> &'static str {
        match self {
            Level::Debug => "\x1B[90m[debug]\x1B[0m",
            Level::Info => "\x1B[36m[info ]\x1B[0m",
            Level::Warn => "\x1B[33m[warn ]\x1B[0m",
            Level::Error => "\x1B[91m[error]\x1B[0m",
        }
    }
}

/// Writes program output to the screen and serial port. Unlike kernel
/// messages, it isn't kept in the kernel log.
pub fn write_terminal(text: &str) {
    serial::write_fmt(format_args!("{text}"));
    console::with(|console| {
        let _ = console.write_str(text);
        console.flush();
    });
}

/// Whether only warnings and errors still go to the screen.
static QUIET: AtomicBool = AtomicBool::new(false);

/// Stops info messages from reaching the screen, as Linux's console log
/// level does once the system is up: a driver logging while the shell
/// waits for input would push its prompt up and leave the cursor on an
/// empty line. They still go to serial, `dmesg` and the log file.
// The test kernel never starts init.
#[cfg_attr(test, allow(dead_code))]
pub fn quiet_console() {
    QUIET.store(true, Ordering::Relaxed);
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = LOG.lock().write_fmt(args);
    let _ = FULL_LOG.lock().write_fmt(args);
    serial::write_fmt(args);
    console::with(|console| {
        let _ = console.write_fmt(args);
        console.flush();
    });
}

#[doc(hidden)]
pub fn _log(level: Level, args: fmt::Arguments) {
    let uptime = time::uptime_nanos();
    let (seconds, micros) = (uptime / 1_000_000_000, uptime % 1_000_000_000 / 1000);
    let line = format_args!("\x1B[90m{seconds:>4}.{micros:06}\x1B[0m {} {args}\n", level.tag());
    match level {
        Level::Debug => {
            let _ = FULL_LOG.lock().write_fmt(line);
            serial::write_fmt(line);
        }
        Level::Info if QUIET.load(Ordering::Relaxed) => {
            let _ = LOG.lock().write_fmt(line);
            let _ = FULL_LOG.lock().write_fmt(line);
            serial::write_fmt(line);
        }
        _ => _print(line),
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::log::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Debug, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Info, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Warn, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Error, format_args!($($arg)*)) };
}
