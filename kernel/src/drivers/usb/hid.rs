//! USB keyboards, through the HID "boot protocol" that every USB keyboard
//! supports (it exists so firmware can use keyboards without parsing HID
//! report descriptors).
//!
//! A boot keyboard reports its whole state in 8 bytes whenever it changes:
//! a byte of modifier keys, a reserved byte, and up to six keys held down,
//! as HID usage codes. Key presses are the keys that appear in a report but
//! weren't in the one before. Unlike PS/2 keyboards, USB keyboards don't
//! repeat keys held down; the host does that, so it's done here.

use alloc::vec::Vec;

use crate::keyboard::Key;

const REPEAT_DELAY_MS: u64 = 500;
const REPEAT_INTERVAL_MS: u64 = 33;

const CAPS_LOCK: u8 = 0x39;
/// What a keyboard reports when too many keys are held to tell which.
const ROLLOVER_ERROR: u8 = 0x01;

const MODIFIER_CTRL: u8 = 0x01 | 0x10;
const MODIFIER_SHIFT: u8 = 0x02 | 0x20;

/// Usages 0x1E-0x27 and 0x2D-0x38, unshifted and shifted (US layout).
const DIGITS: &[u8; 10] = b"1234567890";
const DIGITS_SHIFTED: &[u8; 10] = b"!@#$%^&*()";
const SYMBOLS: &[u8; 12] = b"-=[]\\\\;'`,./";
const SYMBOLS_SHIFTED: &[u8; 12] = b"_+{}||:\"~<>?";
/// Keypad usages 0x54-0x63, with Num Lock on.
const KEYPAD: &[u8; 16] = b"/*-+\n1234567890.";

#[derive(Default)]
pub struct BootKeyboard {
    pressed: [u8; 6],
    modifiers: u8,
    caps_lock: bool,
    /// The key held down, and the tick at which it next repeats.
    held: Option<(u8, u64)>,
}

impl BootKeyboard {
    /// Takes a report received at tick `now`, adding newly pressed keys to
    /// `keys`. Returns whether the LEDs should change (Caps Lock toggled).
    pub fn report(&mut self, report: &[u8], now: u64, keys: &mut Vec<Key>) -> bool {
        let Some(&[modifiers, _, ref current @ ..]) = report.get(..8) else { return false };
        if current.contains(&ROLLOVER_ERROR) {
            return false;
        }
        let mut leds_changed = false;
        for &usage in current.iter().filter(|&&usage| usage > ROLLOVER_ERROR) {
            if self.pressed.contains(&usage) {
                continue;
            }
            if usage == CAPS_LOCK {
                self.caps_lock = !self.caps_lock;
                leds_changed = true;
            } else if let Some(key) = self.translate(usage, modifiers) {
                keys.push(key);
                self.held = Some((usage, now + REPEAT_DELAY_MS));
            }
        }
        if self.held.is_some_and(|(usage, _)| !current.contains(&usage)) {
            self.held = None;
        }
        self.pressed.copy_from_slice(current);
        self.modifiers = modifiers;
        leds_changed
    }

    /// The held key again, if it's time to repeat it.
    pub fn repeat(&mut self, now: u64) -> Option<Key> {
        let (usage, next) = self.held?;
        if now < next {
            return None;
        }
        self.held = Some((usage, now + REPEAT_INTERVAL_MS));
        self.translate(usage, self.modifiers)
    }

    /// When the held key next repeats, if one is held.
    pub fn next_repeat(&self) -> Option<u64> {
        self.held.map(|(_, next)| next)
    }

    /// The LED state to send the keyboard: Num Lock (always on) and Caps
    /// Lock.
    pub fn leds(&self) -> u8 {
        0x01 | if self.caps_lock { 0x02 } else { 0 }
    }

    fn translate(&self, usage: u8, modifiers: u8) -> Option<Key> {
        let shift = modifiers & MODIFIER_SHIFT != 0;
        let ctrl = modifiers & MODIFIER_CTRL != 0;
        let pick = |plain: &[u8], shifted: &[u8], index: u8| {
            let table = if shift { shifted } else { plain };
            Key::Char(char::from(table[usize::from(index)]))
        };
        Some(match usage {
            0x04..=0x1D => {
                let letter = b'a' + (usage - 0x04);
                if ctrl {
                    Key::Ctrl(char::from(letter))
                } else if shift != self.caps_lock {
                    Key::Char(char::from(letter.to_ascii_uppercase()))
                } else {
                    Key::Char(char::from(letter))
                }
            }
            0x1E..=0x27 => pick(DIGITS, DIGITS_SHIFTED, usage - 0x1E),
            0x28 | 0x58 => Key::Enter,
            0x29 => Key::Escape,
            0x2A => Key::Backspace,
            0x2B => Key::Tab,
            0x2C => Key::Char(' '),
            0x2D..=0x38 => pick(SYMBOLS, SYMBOLS_SHIFTED, usage - 0x2D),
            0x3A..=0x45 => Key::Function(usage - 0x39),
            0x4A => Key::Home,
            0x4B => Key::PageUp,
            0x4C => Key::Delete,
            0x4D => Key::End,
            0x4E => Key::PageDown,
            0x4F => Key::Right,
            0x50 => Key::Left,
            0x51 => Key::Down,
            0x52 => Key::Up,
            0x54..=0x63 => Key::Char(char::from(KEYPAD[usize::from(usage - 0x54)])),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn feed(keyboard: &mut BootKeyboard, reports: &[[u8; 8]]) -> Vec<Key> {
        let mut keys = Vec::new();
        for report in reports {
            keyboard.report(report, 0, &mut keys);
        }
        keys
    }

    crate::ktest! {
        fn new_keys_in_reports_are_presses() {
            let mut keyboard = BootKeyboard::default();
            // h, then h+i (only i is new), release all, Shift+1.
            let keys = feed(&mut keyboard, &[
                [0, 0, 0x0B, 0, 0, 0, 0, 0],
                [0, 0, 0x0B, 0x0C, 0, 0, 0, 0],
                [0; 8],
                [0x02, 0, 0x1E, 0, 0, 0, 0, 0],
            ]);
            assert_eq!(keys, vec![Key::Char('h'), Key::Char('i'), Key::Char('!')]);
        }

        fn caps_lock_ctrl_and_special_keys() {
            let mut keyboard = BootKeyboard::default();
            let keys = feed(&mut keyboard, &[
                [0, 0, CAPS_LOCK, 0, 0, 0, 0, 0],
                [0, 0, 0x04, 0, 0, 0, 0, 0],
                [0x01, 0, 0x06, 0, 0, 0, 0, 0],
                [0, 0, 0x52, 0x28, 0, 0, 0, 0],
            ]);
            assert_eq!(keys, vec![Key::Char('A'), Key::Ctrl('c'), Key::Up, Key::Enter]);
            assert_eq!(keyboard.leds(), 0x03);
        }

        fn held_keys_repeat() {
            let mut keyboard = BootKeyboard::default();
            let mut keys = Vec::new();
            keyboard.report(&[0, 0, 0x04, 0, 0, 0, 0, 0], 1000, &mut keys);
            assert_eq!(keyboard.repeat(1100), None);
            assert_eq!(keyboard.repeat(1500), Some(Key::Char('a')));
            assert_eq!(keyboard.repeat(1510), None);
            assert_eq!(keyboard.next_repeat(), Some(1533));
            keyboard.report(&[0; 8], 1520, &mut keys);
            assert_eq!(keyboard.next_repeat(), None);
        }
    }
}
