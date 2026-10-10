//! edit: a small full-screen text editor, in the spirit of nano.
//!
//! The whole screen is redrawn after each key, which the console makes
//! cheap. Shortcuts are listed along the bottom.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eli::io::{self, STDIN, STDOUT};
use eli::{eprintln, fs, term};

use crate::editor::{Key, read_key};

const TAB: &str = "    ";

struct Editor {
    path: String,
    lines: Vec<Vec<char>>,
    /// Cursor position: line, and column in characters.
    row: usize,
    column: usize,
    /// The first line and column on screen.
    top: usize,
    left: usize,
    modified: bool,
    /// A message for the status line, shown until the next key.
    message: String,
    /// Lines cut with Ctrl+K, for Ctrl+U.
    clipboard: Vec<Vec<char>>,
    /// Whether the last key was a cut (consecutive cuts collect lines).
    cutting: bool,
    /// Set by a Ctrl+Q with unsaved changes; a second one quits.
    quit_warned: bool,
    search: String,
    columns: usize,
    rows: usize,
}

pub fn main(arguments: &[String]) -> i32 {
    let Some(path) = arguments.get(1) else {
        eprintln!("usage: edit FILE");
        return 2;
    };
    let lines = match fs::read(path) {
        Ok(data) => {
            let text = String::from_utf8_lossy(&data);
            let mut lines: Vec<Vec<char>> = text.split('\n').map(|line| line.chars().collect()).collect();
            // A trailing newline doesn't start another line.
            if lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
                lines.pop();
            }
            lines
        }
        Err(eli::Error::NOT_FOUND) => vec![Vec::new()],
        Err(error) => {
            eprintln!("edit: {path}: {error}");
            return 1;
        }
    };
    let Ok(original) = term::enter_raw_mode(STDIN) else {
        eprintln!("edit: needs a terminal");
        return 1;
    };
    let (columns, rows) = term::size(STDOUT);
    let mut editor = Editor {
        path: path.clone(),
        lines,
        row: 0,
        column: 0,
        top: 0,
        left: 0,
        modified: false,
        message: String::from("^S save  ^Q quit  ^F find  ^K cut line  ^U paste  ^G help"),
        clipboard: Vec::new(),
        cutting: false,
        quit_warned: false,
        search: String::new(),
        columns: usize::from(columns),
        // Two rows for the title and status bars.
        rows: usize::from(rows).saturating_sub(2).max(1),
    };
    editor.run();
    let _ = term::set_settings(STDIN, &original);
    // Clear the screen and leave the cursor at the top for the shell.
    output("\x1b[0m\x1b[2J\x1b[H");
    0
}

fn output(text: &str) {
    let _ = io::write_all(STDOUT, text.as_bytes());
}

impl Editor {
    fn run(&mut self) {
        loop {
            self.scroll_into_view();
            self.draw();
            let Some(key) = read_key() else { return };
            let was_cutting = core::mem::take(&mut self.cutting);
            let was_warned = core::mem::take(&mut self.quit_warned);
            self.message.clear();
            match key {
                Key::Control(b'q') => {
                    if !self.modified || was_warned {
                        return;
                    }
                    self.message = "Unsaved changes! ^S saves; ^Q again quits without saving.".into();
                    self.quit_warned = true;
                }
                Key::Control(b's') => self.save(),
                Key::Control(b'f') => self.find(),
                Key::Control(b'g') => {
                    self.message = "Arrows/PgUp/PgDn/Home/End move. ^S save ^Q quit ^F find ^K cut ^U paste".into();
                }
                Key::Control(b'k') => self.cut_line(was_cutting),
                Key::Control(b'u') => self.paste(),
                Key::Control(b'a') | Key::Home => self.column = 0,
                Key::Control(b'e') | Key::End => self.column = self.lines[self.row].len(),
                Key::Up => self.move_vertically(-1),
                Key::Down => self.move_vertically(1),
                Key::PageUp => self.move_vertically(-(self.rows as isize)),
                Key::PageDown => self.move_vertically(self.rows as isize),
                Key::Left => self.move_left(),
                Key::Right => self.move_right(),
                Key::Enter => self.split_line(),
                Key::Backspace => self.backspace(),
                Key::Delete => self.delete(),
                Key::Tab => TAB.chars().for_each(|c| self.insert(c)),
                Key::Char(c) => self.insert(c),
                _ => {}
            }
        }
    }

    fn insert(&mut self, c: char) {
        self.lines[self.row].insert(self.column, c);
        self.column += 1;
        self.modified = true;
    }

    fn split_line(&mut self) {
        let rest = self.lines[self.row].split_off(self.column);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.column = 0;
        self.modified = true;
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
            self.lines[self.row].remove(self.column);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.lines[self.row].len();
            self.lines[self.row].extend(line);
        } else {
            return;
        }
        self.modified = true;
    }

    fn delete(&mut self) {
        if self.column < self.lines[self.row].len() {
            self.lines[self.row].remove(self.column);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        } else {
            return;
        }
        self.modified = true;
    }

    fn move_left(&mut self) {
        if self.column > 0 {
            self.column -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.column = self.lines[self.row].len();
        }
    }

    fn move_right(&mut self) {
        if self.column < self.lines[self.row].len() {
            self.column += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.column = 0;
        }
    }

    fn move_vertically(&mut self, lines: isize) {
        let target = (self.row as isize + lines).clamp(0, self.lines.len() as isize - 1);
        self.row = target as usize;
        self.column = self.column.min(self.lines[self.row].len());
    }

    fn cut_line(&mut self, continuing: bool) {
        if !continuing {
            self.clipboard.clear();
        }
        let line = if self.lines.len() > 1 { self.lines.remove(self.row) } else { core::mem::take(&mut self.lines[0]) };
        self.clipboard.push(line);
        self.row = self.row.min(self.lines.len() - 1);
        self.column = 0;
        self.modified = true;
        self.cutting = true;
    }

    fn paste(&mut self) {
        for (offset, line) in self.clipboard.iter().enumerate() {
            self.lines.insert(self.row + offset, line.clone());
        }
        self.row += self.clipboard.len();
        self.row = self.row.min(self.lines.len() - 1);
        self.column = 0;
        self.modified |= !self.clipboard.is_empty();
    }

    fn save(&mut self) {
        let mut text: String = self.lines.iter().map(|line| line.iter().collect::<String>() + "\n").collect();
        if self.lines.len() == 1 && self.lines[0].is_empty() {
            text.clear();
        }
        match fs::write(&self.path, text.as_bytes()) {
            Ok(()) => {
                self.modified = false;
                self.message = format!("Saved {} lines to {}", self.lines.len(), self.path);
            }
            Err(error) => self.message = format!("Can't save: {error}"),
        }
    }

    /// Asks for text on the status line; `None` if cancelled.
    fn prompt(&mut self, question: &str, initial: &str) -> Option<String> {
        let mut answer = String::from(initial);
        loop {
            output(&format!("\x1b[{};1H\x1b[44;97m {question}{answer}\x1b[K\x1b[0m", self.rows + 2));
            match read_key()? {
                Key::Enter => return Some(answer),
                Key::Escape | Key::Control(b'c') | Key::Control(b'q') => return None,
                Key::Backspace => {
                    answer.pop();
                }
                Key::Char(c) => answer.push(c),
                _ => {}
            }
        }
    }

    fn find(&mut self) {
        let Some(query) = self.prompt("Find: ", &self.search.clone()) else { return };
        if query.is_empty() {
            return;
        }
        self.search = query.clone();
        let needle: Vec<char> = query.chars().collect();
        let count = self.lines.len();
        // Search forward from just after the cursor, wrapping around.
        for step in 0..=count {
            let row = (self.row + step) % count;
            let start = if step == 0 { self.column + 1 } else { 0 };
            let line = &self.lines[row];
            if let Some(found) =
                (start..line.len().saturating_sub(needle.len() - 1)).find(|&i| line[i..].starts_with(&needle))
            {
                self.row = row;
                self.column = found;
                return;
            }
        }
        self.message = format!("\"{query}\" not found");
    }

    fn scroll_into_view(&mut self) {
        if self.row < self.top {
            self.top = self.row;
        }
        if self.row >= self.top + self.rows {
            self.top = self.row + 1 - self.rows;
        }
        if self.column < self.left {
            self.left = self.column;
        }
        if self.column >= self.left + self.columns {
            self.left = self.column + 1 - self.columns;
        }
    }

    fn draw(&self) {
        let mut screen = String::from("\x1b[?25l\x1b[H");
        let title = format!(" eliOS edit: {}{}", self.path, if self.modified { " (modified)" } else { "" });
        let position = format!("line {}/{}, column {} ", self.row + 1, self.lines.len(), self.column + 1);
        let padding = self.columns.saturating_sub(title.chars().count() + position.len());
        screen.push_str(&format!("\x1b[44;97m{title}{}{position}\x1b[0m\r\n", " ".repeat(padding)));
        for screen_row in 0..self.rows {
            match self.lines.get(self.top + screen_row) {
                Some(line) => {
                    let visible: String = line.iter().skip(self.left).take(self.columns).collect();
                    screen.push_str(&visible);
                }
                None => screen.push_str("\x1b[34m~\x1b[0m"),
            }
            screen.push_str("\x1b[K\r\n");
        }
        let status = if self.message.is_empty() { "^G help".to_string() } else { self.message.clone() };
        screen.push_str(&format!("\x1b[44;97m {status}\x1b[K\x1b[0m"));
        let cursor_row = self.row - self.top + 2;
        let cursor_column = self.column - self.left + 1;
        screen.push_str(&format!("\x1b[{cursor_row};{cursor_column}H\x1b[?25h"));
        output(&screen);
    }
}
