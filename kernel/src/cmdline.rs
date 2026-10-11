//! The kernel command line: `name=value` options (and bare words) from
//! `\EFI\eliOS\cmdline.txt`, which eliboot passes on.
//!
//! - `data=PARTUUID=<guid>`: mount that ext2 partition at /data, where an
//!   installed system keeps everything that lasts (the system itself is
//!   the initrd, fresh at every boot).
//! - `root=PARTUUID=<guid>`: mount that ext2 partition as the root instead
//!   of the initrd (the first installer's layout).
//! - `generation=N`: which system generation to start (eliboot's menu adds
//!   it; `eli` reads it from /proc/cmdline).

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::Once;

static OPTIONS: Once<Vec<(String, String)>> = Once::new();
static LINE: Once<String> = Once::new();

/// The whole line, for /proc/cmdline.
pub fn line() -> &'static str {
    LINE.get().map_or("", String::as_str)
}

pub fn init(line: &str) {
    let _ = LINE.set(String::from(line));
    let options = line
        .split_whitespace()
        .map(|word| match word.split_once('=') {
            Some((name, value)) => (String::from(name), String::from(value)),
            None => (String::from(word), String::new()),
        })
        .collect();
    let _ = OPTIONS.set(options);
    if !line.is_empty() {
        info!("command line: {line}");
    }
}

/// The value of option `name` (the last one, if given twice).
pub fn get(name: &str) -> Option<&'static str> {
    OPTIONS.get()?.iter().rev().find(|(option, _)| option == name).map(|(_, value)| value.as_str())
}
