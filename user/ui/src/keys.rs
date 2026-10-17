//! Keys: what the compositor's key codes mean.
//!
//! The compositor sends Linux evdev key codes and a US keyboard map in
//! XKB's text format, meant for libxkbcommon. Our programs know the
//! desktop uses that map, so they translate codes with this table instead
//! of compiling the map.

/// A key press, with Shift (and Caps Lock) already applied to characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
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
    Insert,
    Delete,
    /// F1 to F12.
    Function(u8),
    /// Anything else (by evdev code): modifiers on their own, media keys.
    Other(u32),
}

/// Which modifier keys are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub logo: bool,
    pub caps_lock: bool,
}

impl Modifiers {
    /// From the compositor's `modifiers` event (XKB's real modifier bits:
    /// Shift 1, Lock 2, Control 4, Mod1 (Alt) 8, Mod4 (Super) 64).
    pub fn from_xkb(depressed: u32, locked: u32) -> Self {
        let active = depressed | locked;
        Self {
            shift: active & 1 != 0,
            caps_lock: active & 2 != 0,
            control: active & 4 != 0,
            alt: active & 8 != 0,
            logo: active & 64 != 0,
        }
    }
}

/// Keys that type characters: (evdev code, plain, shifted).
const CHARACTERS: &[(u32, char, char)] = &[
    (2, '1', '!'),
    (3, '2', '@'),
    (4, '3', '#'),
    (5, '4', '$'),
    (6, '5', '%'),
    (7, '6', '^'),
    (8, '7', '&'),
    (9, '8', '*'),
    (10, '9', '('),
    (11, '0', ')'),
    (12, '-', '_'),
    (13, '=', '+'),
    (16, 'q', 'Q'),
    (17, 'w', 'W'),
    (18, 'e', 'E'),
    (19, 'r', 'R'),
    (20, 't', 'T'),
    (21, 'y', 'Y'),
    (22, 'u', 'U'),
    (23, 'i', 'I'),
    (24, 'o', 'O'),
    (25, 'p', 'P'),
    (26, '[', '{'),
    (27, ']', '}'),
    (30, 'a', 'A'),
    (31, 's', 'S'),
    (32, 'd', 'D'),
    (33, 'f', 'F'),
    (34, 'g', 'G'),
    (35, 'h', 'H'),
    (36, 'j', 'J'),
    (37, 'k', 'K'),
    (38, 'l', 'L'),
    (39, ';', ':'),
    (40, '\'', '"'),
    (41, '`', '~'),
    (43, '\\', '|'),
    (44, 'z', 'Z'),
    (45, 'x', 'X'),
    (46, 'c', 'C'),
    (47, 'v', 'V'),
    (48, 'b', 'B'),
    (49, 'n', 'N'),
    (50, 'm', 'M'),
    (51, ',', '<'),
    (52, '.', '>'),
    (53, '/', '?'),
    (57, ' ', ' '),
    (86, '<', '>'),
    // The keypad (as if Num Lock were on).
    (55, '*', '*'),
    (71, '7', '7'),
    (72, '8', '8'),
    (73, '9', '9'),
    (74, '-', '-'),
    (75, '4', '4'),
    (76, '5', '5'),
    (77, '6', '6'),
    (78, '+', '+'),
    (79, '1', '1'),
    (80, '2', '2'),
    (81, '3', '3'),
    (82, '0', '0'),
    (83, '.', '.'),
    (98, '/', '/'),
];

/// What the key with evdev code `code` does with `modifiers` held.
pub fn translate(code: u32, modifiers: Modifiers) -> Key {
    if let Some(&(_, plain, shifted)) = CHARACTERS.iter().find(|&&(c, _, _)| c == code) {
        let mut shift = modifiers.shift;
        if modifiers.caps_lock && plain.is_ascii_lowercase() {
            shift = !shift;
        }
        return Key::Char(if shift { shifted } else { plain });
    }
    match code {
        1 => Key::Escape,
        14 => Key::Backspace,
        15 => Key::Tab,
        28 | 96 => Key::Enter,
        59..=68 => Key::Function((code - 58) as u8),
        87 => Key::Function(11),
        88 => Key::Function(12),
        102 => Key::Home,
        103 => Key::Up,
        104 => Key::PageUp,
        105 => Key::Left,
        106 => Key::Right,
        107 => Key::End,
        108 => Key::Down,
        109 => Key::PageDown,
        110 => Key::Insert,
        111 => Key::Delete,
        _ => Key::Other(code),
    }
}

/// Whether holding the key should repeat it (not modifiers or locks).
pub fn repeats(code: u32) -> bool {
    !matches!(code, 29 | 42 | 54 | 56 | 58 | 69 | 70 | 97 | 100 | 125 | 126)
}
