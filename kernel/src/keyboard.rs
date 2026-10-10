//! PS/2 keyboard support: the i8042 controller and scancode decoding.
//!
//! The interrupt handler only queues raw scancodes; decoding happens later,
//! outside interrupt context, through [`Decoder`].
//!
//! Many modern machines have no real PS/2 port, and their USB keyboards only
//! appear here if the firmware emulates PS/2. Real USB support comes later.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::interrupts::{self, IRQ_BASE, InterruptFrame};
use crate::arch::{apic, inb, outb};
use crate::sync::ByteQueue;
use crate::task::WaitQueue;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;

const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const ENABLE_FIRST_PORT: u8 = 0xAE;
#[cfg(test)]
const WRITE_FIRST_PORT_OUTPUT: u8 = 0xD2;

const CONFIG_FIRST_PORT_INTERRUPT: u8 = 1 << 0;
const CONFIG_FIRST_PORT_CLOCK_DISABLED: u8 = 1 << 4;
const CONFIG_TRANSLATION: u8 = 1 << 6;

const KEYBOARD_IRQ: u8 = 1;
const KEYBOARD_VECTOR: u8 = IRQ_BASE + KEYBOARD_IRQ;

static SCANCODES: ByteQueue<256> = ByteQueue::new();
static INPUT: WaitQueue = WaitQueue::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Configures the controller to interrupt on keypresses and routes its IRQ.
pub fn init() -> Result<(), &'static str> {
    // Discard anything left over from the firmware.
    for _ in 0..16 {
        // SAFETY: reading the status and data ports has no harmful effects.
        unsafe {
            if inb(STATUS) & STATUS_OUTPUT_FULL == 0 {
                break;
            }
            inb(DATA);
        }
    }

    send_command(READ_CONFIG)?;
    let mut config = read_data()?;
    // Interrupts on, clock on, and translation to scancode set 1 (the set
    // the decoder understands, whatever the keyboard itself speaks).
    config |= CONFIG_FIRST_PORT_INTERRUPT | CONFIG_TRANSLATION;
    config &= !CONFIG_FIRST_PORT_CLOCK_DISABLED;
    send_command(WRITE_CONFIG)?;
    write_data(config)?;
    send_command(ENABLE_FIRST_PORT)?;

    interrupts::set_handler(KEYBOARD_VECTOR, on_interrupt);
    apic::route_isa_irq(KEYBOARD_IRQ, KEYBOARD_VECTOR)
}

fn on_interrupt(_frame: &mut InterruptFrame) {
    // SAFETY: the controller has a byte for us; reading it acknowledges it.
    let scancode = unsafe { inb(DATA) };
    if !SCANCODES.push(scancode) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    INPUT.wake_all();
}

/// The oldest scancode not yet processed, if any.
pub fn next_scancode() -> Option<u8> {
    SCANCODES.pop()
}

/// The next scancode, waiting for a keypress if necessary.
pub fn read_scancode() -> u8 {
    INPUT.wait_until(next_scancode)
}

/// Makes the controller act as if the keyboard sent `scancode`, interrupt
/// and all. For testing the whole input path.
#[cfg(test)]
pub fn simulate_scancode(scancode: u8) {
    send_command(WRITE_FIRST_PORT_OUTPUT).unwrap();
    write_data(scancode).unwrap();
}

/// Waits (briefly) until the controller can accept a byte.
fn wait_for_input_space() -> Result<(), &'static str> {
    for _ in 0..100_000 {
        // SAFETY: reading the status port has no side effects.
        if unsafe { inb(STATUS) } & STATUS_INPUT_FULL == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("PS/2 controller is not responding")
}

fn send_command(command: u8) -> Result<(), &'static str> {
    wait_for_input_space()?;
    // SAFETY: a documented controller command.
    unsafe { outb(COMMAND, command) };
    Ok(())
}

fn write_data(byte: u8) -> Result<(), &'static str> {
    wait_for_input_space()?;
    // SAFETY: the parameter for the command just sent.
    unsafe { outb(DATA, byte) };
    Ok(())
}

fn read_data() -> Result<u8, &'static str> {
    for _ in 0..100_000 {
        // SAFETY: reading status, then data once it's available.
        unsafe {
            if inb(STATUS) & STATUS_OUTPUT_FULL != 0 {
                return Ok(inb(DATA));
            }
        }
        core::hint::spin_loop();
    }
    Err("PS/2 controller did not answer")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// A letter pressed with Ctrl held, as the lowercase letter.
    Ctrl(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Function(u8),
}

/// US layout for scancode set 1 make codes 0x00-0x39. Zero means the key
/// has no character (modifiers and the like).
const UNSHIFTED: &[u8; 0x3A] = b"\0\x1B1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8; 0x3A] = b"\0\x1B!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

const LEFT_SHIFT: u8 = 0x2A;
const RIGHT_SHIFT: u8 = 0x36;
const CTRL: u8 = 0x1D;
const CAPS_LOCK: u8 = 0x3A;
const EXTENDED_PREFIX: u8 = 0xE0;
const RELEASED: u8 = 0x80;

/// Turns scancode set 1 bytes into key presses, tracking modifier state.
#[derive(Default)]
pub struct Decoder {
    extended: bool,
    left_shift: bool,
    right_shift: bool,
    ctrl: bool,
    caps_lock: bool,
}

impl Decoder {
    /// Feeds one scancode byte; returns a key when one was pressed.
    pub fn feed(&mut self, scancode: u8) -> Option<Key> {
        if scancode == EXTENDED_PREFIX {
            self.extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let pressed = scancode & RELEASED == 0;
        let code = scancode & !RELEASED;

        // Modifiers. Right Ctrl is the same code with the extended prefix.
        match code {
            CTRL => self.ctrl = pressed,
            // Extended shift codes are fake keys some keyboards send around
            // other keys; they're not real shift presses.
            LEFT_SHIFT if !extended => self.left_shift = pressed,
            RIGHT_SHIFT if !extended => self.right_shift = pressed,
            CAPS_LOCK if pressed => self.caps_lock = !self.caps_lock,
            _ if !pressed => {}
            _ if extended => return self.extended_key(code),
            _ => return self.key(code),
        }
        None
    }

    fn key(&self, code: u8) -> Option<Key> {
        match code {
            0x3B..=0x44 => return Some(Key::Function(code - 0x3B + 1)),
            0x57 => return Some(Key::Function(11)),
            0x58 => return Some(Key::Function(12)),
            _ => {}
        }

        let shift = self.left_shift || self.right_shift;
        let table = if shift { SHIFTED } else { UNSHIFTED };
        let byte = *table.get(usize::from(code))?;
        Some(match byte {
            0 => return None,
            0x1B => Key::Escape,
            0x08 => Key::Backspace,
            b'\t' => Key::Tab,
            b'\n' => Key::Enter,
            b if self.ctrl && b.is_ascii_alphabetic() => Key::Ctrl(char::from(b.to_ascii_lowercase())),
            // Caps Lock inverts the case of letters only.
            b if self.caps_lock && b.is_ascii_alphabetic() => {
                Key::Char(char::from(if shift { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() }))
            }
            b => Key::Char(char::from(b)),
        })
    }

    fn extended_key(&self, code: u8) -> Option<Key> {
        Some(match code {
            0x1C => Key::Enter, // keypad Enter
            0x35 => Key::Char('/'),
            0x47 => Key::Home,
            0x48 => Key::Up,
            0x49 => Key::PageUp,
            0x4B => Key::Left,
            0x4D => Key::Right,
            0x4F => Key::End,
            0x50 => Key::Down,
            0x51 => Key::PageDown,
            0x53 => Key::Delete,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(scancodes: &[u8]) -> [Option<Key>; 8] {
        let mut decoder = Decoder::default();
        let mut keys = [None; 8];
        let mut count = 0;
        for &scancode in scancodes {
            if let Some(key) = decoder.feed(scancode) {
                keys[count] = Some(key);
                count += 1;
            }
        }
        keys
    }

    crate::ktest! {
        fn letters_digits_and_releases() {
            // a, release a, 1, space
            let keys = decode(&[0x1E, 0x9E, 0x02, 0x39]);
            assert_eq!(keys[..4], [Some(Key::Char('a')), Some(Key::Char('1')), Some(Key::Char(' ')), None]);
        }

        fn shift_and_caps_lock() {
            // Shift+a, a, Caps Lock, a, Shift+a
            let keys = decode(&[0x2A, 0x1E, 0xAA, 0x1E, 0x3A, 0xBA, 0x1E, 0x36, 0x1E, 0xB6]);
            let expected = ['A', 'a', 'A', 'a'].map(|c| Some(Key::Char(c)));
            assert_eq!(keys[..4], expected);
        }

        fn shift_changes_symbols_but_caps_lock_does_not() {
            let keys = decode(&[0x3A, 0x02, 0x2A, 0x02]);
            assert_eq!(keys[..2], [Some(Key::Char('1')), Some(Key::Char('!'))]);
        }

        fn extended_keys_and_ctrl() {
            // Up, right Ctrl+L, Enter (main), Enter (keypad)
            let keys = decode(&[0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0x1D, 0x26, 0xE0, 0x9D, 0x1C, 0xE0, 0x1C]);
            assert_eq!(keys[..4], [Some(Key::Up), Some(Key::Ctrl('l')), Some(Key::Enter), Some(Key::Enter)]);
        }

        fn scancodes_arrive_by_interrupt() {
            while next_scancode().is_some() {}
            simulate_scancode(0x1E);
            let deadline = crate::time::uptime_nanos() + 1_000_000_000;
            let received = loop {
                if let Some(scancode) = next_scancode() {
                    break Some(scancode);
                }
                if crate::time::uptime_nanos() > deadline {
                    break None;
                }
                core::hint::spin_loop();
            };
            assert_eq!(received, Some(0x1E));
        }
    }
}
