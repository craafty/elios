//! Kernel output. Everything printed goes to the serial port and, once there
//! is one, the screen console. Debug messages go to serial only, to keep the
//! screen readable.

use core::fmt::{self, Write as _};

use crate::{console, serial, time};

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

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
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
