//! Line editing for the interactive shell.
//!
//! The terminal is switched to raw mode while a line is being typed, so
//! every key arrives here: arrows move the cursor or walk the history, Tab
//! completes, and the usual Ctrl shortcuts work (A/E home/end, U/K/W cut,
//! L clear, C cancel, D end of input).

use alloc::string::String;
use alloc::vec::Vec;

use eli::io::{self, STDIN, STDOUT};
use eli::term;

/// What reading a line produced.
pub enum Line {
    Text(String),
    /// Ctrl+C: the line was abandoned.
    Cancelled,
    /// Ctrl+D on an empty line.
    EndOfInput,
}

pub struct Editor<'a> {
    prompt: &'a str,
    line: Vec<char>,
    cursor: usize,
    history: &'a [String],
    /// Position in the history while walking it; `history.len()` is the
    /// line being typed.
    history_index: usize,
    /// The typed line, kept while walking the history.
    draft: Vec<char>,
}

fn output(text: &str) {
    let _ = io::write_all(STDOUT, text.as_bytes());
}

/// Reads one key's worth of bytes from the terminal.
fn read_byte() -> Option<u8> {
    let mut byte = [0];
    match io::read(STDIN, &mut byte) {
        Ok(1) => Some(byte[0]),
        _ => None,
    }
}

enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
    Control(u8),
    Unknown,
}

fn read_key() -> Option<Key> {
    let byte = read_byte()?;
    Some(match byte {
        b'\r' | b'\n' => Key::Enter,
        0x7F | 0x08 => Key::Backspace,
        b'\t' => Key::Tab,
        0x1B => read_escape(),
        1..=26 => Key::Control(byte + b'a' - 1),
        0x20..=0x7E => Key::Char(byte as char),
        0x80.. => {
            // A multi-byte UTF-8 character.
            let len = match byte {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            let mut bytes = [byte, 0, 0, 0];
            for slot in bytes.iter_mut().take(len).skip(1) {
                *slot = read_byte()?;
            }
            core::str::from_utf8(&bytes[..len]).ok().and_then(|s| s.chars().next()).map_or(Key::Unknown, Key::Char)
        }
        _ => Key::Unknown,
    })
}

/// Decodes `ESC [ ...` (or `ESC O ...`) sequences for the special keys.
fn read_escape() -> Key {
    let Some(kind) = read_byte() else { return Key::Unknown };
    if kind != b'[' && kind != b'O' {
        return Key::Unknown;
    }
    let mut parameter = 0;
    loop {
        let Some(byte) = read_byte() else { return Key::Unknown };
        match byte {
            b'0'..=b'9' => parameter = parameter * 10 + u32::from(byte - b'0'),
            b'A' => return Key::Up,
            b'B' => return Key::Down,
            b'C' => return Key::Right,
            b'D' => return Key::Left,
            b'H' => return Key::Home,
            b'F' => return Key::End,
            b'~' => {
                return match parameter {
                    1 | 7 => Key::Home,
                    4 | 8 => Key::End,
                    3 => Key::Delete,
                    _ => Key::Unknown,
                };
            }
            _ => return Key::Unknown,
        }
    }
}

impl<'a> Editor<'a> {
    pub fn new(prompt: &'a str, history: &'a [String]) -> Self {
        Self { prompt, line: Vec::new(), cursor: 0, history, history_index: history.len(), draft: Vec::new() }
    }

    /// Reads a line. `complete` lists the candidates for a partial word,
    /// given the line up to the cursor.
    pub fn read(mut self, complete: impl Fn(&str) -> Vec<String>) -> Line {
        let Ok(original) = term::enter_raw_mode(STDIN) else {
            return self.read_plain();
        };
        output(self.prompt);
        let result = loop {
            let Some(key) = read_key() else { break Line::EndOfInput };
            match key {
                Key::Enter => {
                    output("\n");
                    break Line::Text(self.line.iter().collect());
                }
                Key::Control(b'c') => {
                    output("^C\n");
                    break Line::Cancelled;
                }
                Key::Control(b'd') if self.line.is_empty() => {
                    output("\n");
                    break Line::EndOfInput;
                }
                Key::Control(b'd') | Key::Delete => {
                    if self.cursor < self.line.len() {
                        self.line.remove(self.cursor);
                    }
                }
                Key::Char(c) => {
                    self.line.insert(self.cursor, c);
                    self.cursor += 1;
                }
                Key::Backspace => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.line.remove(self.cursor);
                    }
                }
                Key::Left | Key::Control(b'b') => self.cursor = self.cursor.saturating_sub(1),
                Key::Right | Key::Control(b'f') => self.cursor = (self.cursor + 1).min(self.line.len()),
                Key::Home | Key::Control(b'a') => self.cursor = 0,
                Key::End | Key::Control(b'e') => self.cursor = self.line.len(),
                Key::Control(b'u') => {
                    self.line.drain(..self.cursor);
                    self.cursor = 0;
                }
                Key::Control(b'k') => self.line.truncate(self.cursor),
                Key::Control(b'w') => {
                    let mut start = self.cursor;
                    while start > 0 && self.line[start - 1] == ' ' {
                        start -= 1;
                    }
                    while start > 0 && self.line[start - 1] != ' ' {
                        start -= 1;
                    }
                    self.line.drain(start..self.cursor);
                    self.cursor = start;
                }
                Key::Control(b'l') => {
                    output("\x1b[2J\x1b[H");
                    output(self.prompt);
                }
                Key::Up => self.walk_history(-1),
                Key::Down => self.walk_history(1),
                Key::Tab => self.complete(&complete),
                _ => continue,
            }
            self.redraw();
        };
        let _ = term::set_settings(STDIN, &original);
        result
    }

    /// Without a terminal (input from a file or pipe), just read a line.
    fn read_plain(self) -> Line {
        output(self.prompt);
        let mut line = Vec::new();
        loop {
            match read_byte() {
                Some(b'\n') => return Line::Text(String::from_utf8_lossy(&line).into_owned()),
                Some(byte) => line.push(byte),
                None if line.is_empty() => return Line::EndOfInput,
                None => return Line::Text(String::from_utf8_lossy(&line).into_owned()),
            }
        }
    }

    fn redraw(&self) {
        let text: String = self.line.iter().collect();
        let mut screen = alloc::format!("\r{}{}\x1b[K", self.prompt, text);
        let back = self.line.len() - self.cursor;
        if back > 0 {
            screen.push_str(&alloc::format!("\x1b[{back}D"));
        }
        output(&screen);
    }

    fn walk_history(&mut self, step: isize) {
        let target = self.history_index as isize + step;
        if target < 0 || target > self.history.len() as isize {
            return;
        }
        if self.history_index == self.history.len() {
            self.draft = self.line.clone();
        }
        self.history_index = target as usize;
        self.line = match self.history.get(self.history_index) {
            Some(entry) => entry.chars().collect(),
            None => self.draft.clone(),
        };
        self.cursor = self.line.len();
    }

    fn complete(&mut self, complete: &impl Fn(&str) -> Vec<String>) {
        let before: String = self.line[..self.cursor].iter().collect();
        let word_start = before.rfind(' ').map_or(0, |space| space + 1);
        let word = &before[word_start..];
        let candidates = complete(&before);
        let Some(first) = candidates.first() else { return };

        // Insert the longest prefix all candidates share.
        let mut common: Vec<char> = first.chars().collect();
        for candidate in &candidates[1..] {
            let shared = common.iter().zip(candidate.chars()).take_while(|(a, b)| **a == *b).count();
            common.truncate(shared);
        }
        let typed = word.chars().count();
        if common.len() > typed {
            for &c in &common[typed..] {
                self.line.insert(self.cursor, c);
                self.cursor += 1;
            }
            if candidates.len() == 1 && !first.ends_with('/') {
                self.line.insert(self.cursor, ' ');
                self.cursor += 1;
            }
        } else if candidates.len() > 1 {
            // Nothing more to add: show the choices.
            output("\n");
            let (columns, _) = term::size(STDOUT);
            let width = candidates.iter().map(|c| c.chars().count()).max().unwrap_or(0) + 2;
            let per_line = (usize::from(columns) / width).max(1);
            for (index, candidate) in candidates.iter().enumerate() {
                output(&alloc::format!("{candidate:<width$}"));
                if (index + 1) % per_line == 0 {
                    output("\n");
                }
            }
            if !candidates.len().is_multiple_of(per_line) {
                output("\n");
            }
        }
    }
}
