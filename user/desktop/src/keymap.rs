//! The keyboard map sent to clients: a US layout in XKB's text format,
//! which they compile with libxkbcommon to turn key codes into symbols.
//!
//! Key codes are Linux's evdev codes plus 8 (an X11 tradition XKB keeps).
//! Clients learn which modifiers are active from the compositor's
//! `modifiers` events, given as XKB's real modifier bits: Shift 1, Lock 2,
//! Control 4, Mod1 (Alt) 8, Mod2 (Num Lock) 16, Mod4 (Super) 64.

use alloc::format;
use alloc::string::String;

pub const SHIFT: u32 = 1;
pub const LOCK: u32 = 2;
pub const CONTROL: u32 = 4;
pub const ALT: u32 = 8;
pub const SUPER: u32 = 64;

/// (key name, evdev code, symbols, type) for every key.
const KEYS: &[(&str, u32, &str, &str)] = &[
    ("ESC", 1, "Escape", ""),
    ("AE01", 2, "1, exclam", ""),
    ("AE02", 3, "2, at", ""),
    ("AE03", 4, "3, numbersign", ""),
    ("AE04", 5, "4, dollar", ""),
    ("AE05", 6, "5, percent", ""),
    ("AE06", 7, "6, asciicircum", ""),
    ("AE07", 8, "7, ampersand", ""),
    ("AE08", 9, "8, asterisk", ""),
    ("AE09", 10, "9, parenleft", ""),
    ("AE10", 11, "0, parenright", ""),
    ("AE11", 12, "minus, underscore", ""),
    ("AE12", 13, "equal, plus", ""),
    ("BKSP", 14, "BackSpace", ""),
    ("TAB", 15, "Tab, ISO_Left_Tab", ""),
    ("AD01", 16, "q, Q", "ALPHABETIC"),
    ("AD02", 17, "w, W", "ALPHABETIC"),
    ("AD03", 18, "e, E", "ALPHABETIC"),
    ("AD04", 19, "r, R", "ALPHABETIC"),
    ("AD05", 20, "t, T", "ALPHABETIC"),
    ("AD06", 21, "y, Y", "ALPHABETIC"),
    ("AD07", 22, "u, U", "ALPHABETIC"),
    ("AD08", 23, "i, I", "ALPHABETIC"),
    ("AD09", 24, "o, O", "ALPHABETIC"),
    ("AD10", 25, "p, P", "ALPHABETIC"),
    ("AD11", 26, "bracketleft, braceleft", ""),
    ("AD12", 27, "bracketright, braceright", ""),
    ("RTRN", 28, "Return", ""),
    ("LCTL", 29, "Control_L", ""),
    ("AC01", 30, "a, A", "ALPHABETIC"),
    ("AC02", 31, "s, S", "ALPHABETIC"),
    ("AC03", 32, "d, D", "ALPHABETIC"),
    ("AC04", 33, "f, F", "ALPHABETIC"),
    ("AC05", 34, "g, G", "ALPHABETIC"),
    ("AC06", 35, "h, H", "ALPHABETIC"),
    ("AC07", 36, "j, J", "ALPHABETIC"),
    ("AC08", 37, "k, K", "ALPHABETIC"),
    ("AC09", 38, "l, L", "ALPHABETIC"),
    ("AC10", 39, "semicolon, colon", ""),
    ("AC11", 40, "apostrophe, quotedbl", ""),
    ("TLDE", 41, "grave, asciitilde", ""),
    ("LFSH", 42, "Shift_L", ""),
    ("BKSL", 43, "backslash, bar", ""),
    ("AB01", 44, "z, Z", "ALPHABETIC"),
    ("AB02", 45, "x, X", "ALPHABETIC"),
    ("AB03", 46, "c, C", "ALPHABETIC"),
    ("AB04", 47, "v, V", "ALPHABETIC"),
    ("AB05", 48, "b, B", "ALPHABETIC"),
    ("AB06", 49, "n, N", "ALPHABETIC"),
    ("AB07", 50, "m, M", "ALPHABETIC"),
    ("AB08", 51, "comma, less", ""),
    ("AB09", 52, "period, greater", ""),
    ("AB10", 53, "slash, question", ""),
    ("RTSH", 54, "Shift_R", ""),
    ("KPMU", 55, "KP_Multiply", ""),
    ("LALT", 56, "Alt_L", ""),
    ("SPCE", 57, "space", ""),
    ("CAPS", 58, "Caps_Lock", ""),
    ("FK01", 59, "F1", ""),
    ("FK02", 60, "F2", ""),
    ("FK03", 61, "F3", ""),
    ("FK04", 62, "F4", ""),
    ("FK05", 63, "F5", ""),
    ("FK06", 64, "F6", ""),
    ("FK07", 65, "F7", ""),
    ("FK08", 66, "F8", ""),
    ("FK09", 67, "F9", ""),
    ("FK10", 68, "F10", ""),
    ("NMLK", 69, "Num_Lock", ""),
    ("SCLK", 70, "Scroll_Lock", ""),
    ("KP7", 71, "KP_Home, KP_7", "KEYPAD"),
    ("KP8", 72, "KP_Up, KP_8", "KEYPAD"),
    ("KP9", 73, "KP_Prior, KP_9", "KEYPAD"),
    ("KPSU", 74, "KP_Subtract", ""),
    ("KP4", 75, "KP_Left, KP_4", "KEYPAD"),
    ("KP5", 76, "KP_Begin, KP_5", "KEYPAD"),
    ("KP6", 77, "KP_Right, KP_6", "KEYPAD"),
    ("KPAD", 78, "KP_Add", ""),
    ("KP1", 79, "KP_End, KP_1", "KEYPAD"),
    ("KP2", 80, "KP_Down, KP_2", "KEYPAD"),
    ("KP3", 81, "KP_Next, KP_3", "KEYPAD"),
    ("KP0", 82, "KP_Insert, KP_0", "KEYPAD"),
    ("KPDL", 83, "KP_Delete, KP_Decimal", "KEYPAD"),
    ("LSGT", 86, "less, greater", ""),
    ("FK11", 87, "F11", ""),
    ("FK12", 88, "F12", ""),
    ("KPEN", 96, "KP_Enter", ""),
    ("RCTL", 97, "Control_R", ""),
    ("KPDV", 98, "KP_Divide", ""),
    ("PRSC", 99, "Print", ""),
    ("RALT", 100, "Alt_R", ""),
    ("HOME", 102, "Home", ""),
    ("UP", 103, "Up", ""),
    ("PGUP", 104, "Prior", ""),
    ("LEFT", 105, "Left", ""),
    ("RGHT", 106, "Right", ""),
    ("END", 107, "End", ""),
    ("DOWN", 108, "Down", ""),
    ("PGDN", 109, "Next", ""),
    ("INS", 110, "Insert", ""),
    ("DELE", 111, "Delete", ""),
    ("MUTE", 113, "XF86AudioMute", ""),
    ("VOL-", 114, "XF86AudioLowerVolume", ""),
    ("VOL+", 115, "XF86AudioRaiseVolume", ""),
    ("POWR", 116, "XF86PowerOff", ""),
    ("PAUS", 119, "Pause", ""),
    ("LWIN", 125, "Super_L", ""),
    ("RWIN", 126, "Super_R", ""),
    ("COMP", 127, "Menu", ""),
];

/// The keymap's text.
pub fn text() -> String {
    let mut text = String::from("xkb_keymap {\nxkb_keycodes \"eliOS\" {\n    minimum = 8;\n    maximum = 255;\n");
    for (name, code, _, _) in KEYS {
        text += &format!("    <{name}> = {};\n", code + 8);
    }
    text += "    indicator 1 = \"Caps Lock\";\n    indicator 2 = \"Num Lock\";\n};\n";
    text += r#"xkb_types "eliOS" {
    virtual_modifiers NumLock,Alt,Super;
    type "ONE_LEVEL" {
        modifiers = none;
        level_name[Level1] = "Any";
    };
    type "TWO_LEVEL" {
        modifiers = Shift;
        map[Shift] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Shift";
    };
    type "ALPHABETIC" {
        modifiers = Shift+Lock;
        map[Shift] = Level2;
        map[Lock] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Caps";
    };
    type "KEYPAD" {
        modifiers = Shift+NumLock;
        map[None] = Level1;
        map[Shift] = Level2;
        map[NumLock] = Level2;
        map[Shift+NumLock] = Level1;
        level_name[Level1] = "Base";
        level_name[Level2] = "Number";
    };
};
xkb_compat "eliOS" {
    virtual_modifiers NumLock,Alt,Super;
    interpret.useModMapMods = AnyLevel;
    interpret.repeat = False;
    interpret Shift_L { action = SetMods(modifiers=Shift,clearLocks); };
    interpret Shift_R { action = SetMods(modifiers=Shift,clearLocks); };
    interpret Control_L { action = SetMods(modifiers=Control); };
    interpret Control_R { action = SetMods(modifiers=Control); };
    interpret Alt_L { useModMapMods = level1; virtualModifier = Alt; action = SetMods(modifiers=modMapMods); };
    interpret Alt_R { useModMapMods = level1; virtualModifier = Alt; action = SetMods(modifiers=modMapMods); };
    interpret Super_L { useModMapMods = level1; virtualModifier = Super; action = SetMods(modifiers=modMapMods); };
    interpret Super_R { useModMapMods = level1; virtualModifier = Super; action = SetMods(modifiers=modMapMods); };
    interpret Caps_Lock { action = LockMods(modifiers=Lock); };
    interpret Num_Lock { virtualModifier = NumLock; action = LockMods(modifiers=NumLock); };
    interpret Any + AnyOf(all) { action = SetMods(modifiers=modMapMods,clearLocks); };
    interpret Any { repeat = True; };
    indicator "Caps Lock" { whichModState = Locked; modifiers = Lock; };
    indicator "Num Lock" { whichModState = Locked; modifiers = NumLock; };
};
xkb_symbols "eliOS" {
    name[group1] = "English (US)";
"#;
    for (name, _, symbols, kind) in KEYS {
        if kind.is_empty() {
            text += &format!("    key <{name}> {{ [ {symbols} ] }};\n");
        } else {
            text += &format!("    key <{name}> {{ type = \"{kind}\", [ {symbols} ] }};\n");
        }
    }
    text += r#"    modifier_map Shift { <LFSH>, <RTSH> };
    modifier_map Lock { <CAPS> };
    modifier_map Control { <LCTL>, <RCTL> };
    modifier_map Mod1 { <LALT>, <RALT> };
    modifier_map Mod2 { <NMLK> };
    modifier_map Mod4 { <LWIN>, <RWIN> };
};
};
"#;
    text
}
