//! The boot menu: pick an older generation of the system.
//!
//! `eli` lists the system's generations in `\EFI\eliOS\generations.txt`
//! (the data partition is ext2, which eliboot can't read). If there's more
//! than one, eliboot waits a moment for a key; only then does it show the
//! list, so normal boots aren't slowed by a menu nobody looks at.

use crate::uefi::{self, Handle, InputKey, MemoryType};
use crate::{console, fs, print, println};

const GENERATIONS_PATH: &str = "\\EFI\\eliOS\\generations.txt";
/// How long to wait for a key before starting normally.
const WAIT_MS: usize = 1500;
const SCAN_UP: u16 = 1;
const SCAN_DOWN: u16 = 2;
const SCAN_ESCAPE: u16 = 0x17;
const MAX_ENTRIES: usize = 9;

/// A key, if one is waiting.
fn key() -> Option<InputKey> {
    let input = uefi::system_table()?.con_in;
    let mut key = InputKey::default();
    // SAFETY: the firmware's input protocol, with a valid out-pointer.
    let status = unsafe { ((*input).read_key_stroke)(input, &mut key) };
    (!status.is_error()).then_some(key)
}

fn sleep_ms(ms: usize) {
    // SAFETY: plain firmware call.
    unsafe { (uefi::boot_services().stall)(ms * 1000) };
}

/// The generation to start, if someone picked one other than the current.
pub fn choose_generation(image: Handle) -> Option<u32> {
    let text = fs::read_file(image, GENERATIONS_PATH, MemoryType::LOADER_DATA).ok()?;
    let text = core::str::from_utf8(text).ok()?;
    let mut entries: [(u32, &str); MAX_ENTRIES] = [(0, ""); MAX_ENTRIES];
    let mut count = 0;
    for line in text.lines().take(MAX_ENTRIES) {
        let Some((number, description)) = line.split_once('\t') else { continue };
        let Ok(number) = number.trim().parse() else { continue };
        entries[count] = (number, description);
        count += 1;
    }
    if count < 2 {
        return None;
    }

    // Anything typed before now isn't for us.
    while key().is_some() {}
    println!("Press any key for the boot menu (older generations of the system)");
    let mut pressed = false;
    for _ in 0..WAIT_MS / 10 {
        if key().is_some() {
            pressed = true;
            break;
        }
        sleep_ms(10);
    }
    if !pressed {
        return None;
    }

    let mut selected = 0;
    loop {
        console::clear();
        console::set_color(console::LIGHT_CYAN);
        println!("eliOS: which generation of the system?\n");
        console::set_color(console::LIGHT_GRAY);
        for (index, (number, description)) in entries[..count].iter().enumerate() {
            let marker = if index == selected { '>' } else { ' ' };
            println!(" {marker} {}. generation {number:<4} {description}", index + 1);
        }
        print!("\nUp and Down choose, Enter starts it, Escape starts the current one.");
        let key = loop {
            if let Some(key) = key() {
                break key;
            }
            sleep_ms(10);
        };
        match (key.scan_code, key.unicode_char) {
            (SCAN_UP, _) => selected = selected.checked_sub(1).unwrap_or(count - 1),
            (SCAN_DOWN, _) => selected = (selected + 1) % count,
            (SCAN_ESCAPE, _) => {
                console::clear();
                return None;
            }
            (_, 0x0D) => break,
            (_, digit @ 0x31..=0x39) if usize::from(digit - 0x31) < count => {
                selected = usize::from(digit - 0x31);
                break;
            }
            _ => {}
        }
    }
    console::clear();
    // The first entry is the current one: nothing to ask for.
    (selected != 0).then(|| entries[selected].0)
}
