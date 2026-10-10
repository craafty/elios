//! Kernel output. Everything printed goes to the serial port and, once there
//! is one, the screen console. Debug messages go to serial only, to keep the
//! screen readable.
//!
//! Output is also kept in an in-memory log, so messages printed before the
//! console existed can be shown on it afterwards.

use core::fmt::{self, Write as _};

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

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = LOG.lock().write_fmt(args);
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
    if level == Level::Debug {
        serial::write_fmt(line);
    } else {
        _print(line);
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
