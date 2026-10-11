//! The kernel command line: `name=value` options (and bare words) from
//! `\EFI\eliOS\cmdline.txt`, which eliboot passes on.
//!
//! - `root=PARTUUID=<guid>`: mount that ext2 partition as the root (an
//!   installed system); without it, the root is the initrd (the live system).

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::Once;

static OPTIONS: Once<Vec<(String, String)>> = Once::new();

pub fn init(line: &str) {
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
