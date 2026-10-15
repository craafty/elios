//! Saves the kernel log, debug messages included, to the boot partition:
//! `/disk/logs/boot.log`, with the previous boot's log kept as
//! `previous-boot.log`.
//!
//! Real laptops have no serial port, and a problem with a driver usually
//! shows up only on the machine that has the hardware. Writing the log to
//! the USB stick eliOS started from means the stick can be read on another
//! computer afterwards, even after a hang. The file is brought up to date
//! every couple of seconds and just before a restart or power off.
//!
//! It's on by default for the live system (started from the initrd), and
//! the command line's `savelog` or `savelog=0` overrides that.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fs::{self, Attributes, FileKind, Inode};
use crate::sync::Mutex;
use crate::{cmdline, log, task};

const DIRECTORY: &str = "/disk/logs";
const INTERVAL_MS: u64 = 2000;

/// The open log file and how much of the log it has.
struct Saver {
    file: Arc<dyn Inode>,
    saved: u64,
    size: u64,
}

static SAVER: Mutex<Option<Saver>> = Mutex::new(None);

/// Starts saving the log if it's wanted and /disk is mounted.
pub fn start(live_system: bool) {
    let wanted = match cmdline::get("savelog") {
        Some("0" | "no" | "off") => false,
        Some(_) => true,
        None => live_system,
    };
    if !wanted {
        return;
    }
    match open() {
        Ok(file) => {
            *SAVER.lock() = Some(Saver { file, saved: 0, size: 0 });
            info!("log: saving to {DIRECTORY}/boot.log");
            task::spawn("logfile", || {
                loop {
                    flush();
                    task::sleep_ms(INTERVAL_MS);
                }
            });
        }
        // No /disk (started from a disk eliOS can't read): nothing to do.
        Err(crate::errno::Errno::ENOENT) => {}
        Err(error) => warn!("log: can't save to {DIRECTORY}: {error:?}"),
    }
}

/// Moves the last boot's log aside and creates a fresh one.
fn open() -> crate::errno::Result<Arc<dyn Inode>> {
    let disk = fs::lookup("/", "/disk", true)?;
    let directory = match disk.lookup("logs") {
        Ok(directory) => directory,
        Err(_) => disk.create("logs", FileKind::Directory, Attributes::root(0o755))?,
    };
    if directory.lookup("boot.log").is_ok() {
        let _ = directory.unlink("previous-boot.log");
        directory.rename("boot.log", &directory, "previous-boot.log")?;
    }
    directory.create("boot.log", FileKind::Regular, Attributes::root(0o644))
}

/// Appends whatever has been logged since the last save.
pub fn flush() {
    let mut saver = SAVER.lock();
    let Some(saver) = saver.as_mut() else { return };
    let mut text = Vec::new();
    let saved = log::full_since(saver.saved, &mut text);
    if text.is_empty() {
        return;
    }
    // Not logged, which would make more to save.
    if let Ok(written) = saver.file.write_at(saver.size, &text) {
        saver.size += written as u64;
        saver.saved = saved;
    }
}
