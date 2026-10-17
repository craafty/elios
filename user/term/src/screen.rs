//! What the terminal shows: a grid of character cells, the lines that
//! scrolled off the top, and the parser that turns a program's output
//! (text and xterm's escape sequences) into changes to them.
//!
//! The parser follows the state machine DEC terminals use (Paul Williams'
//! description at vt100.net), on characters rather than bytes: the output
//! is decoded from UTF-8 first, and every control is ASCII. What's
//! implemented is what `TERM=xterm-256color` promises, plus the modes
//! full-screen programs switch on (the alternate screen, mouse reports,
//! bracketed paste).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Default,
    /// One of the 256 palette colors.
    Index(u8),
    /// 0xRRGGBB.
    Rgb(u32),
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const INVERSE: u8 = 16;
pub const HIDDEN: u8 = 32;
pub const STRIKE: u8 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub flags: u8,
}

impl Style {
    pub const PLAIN: Style = Style { foreground: Color::Default, background: Color::Default, flags: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub style: Style,
}

impl Cell {
    pub const BLANK: Cell = Cell { c: ' ', style: Style::PLAIN };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// The text carries on into the next line (it was wrapped there, not
    /// ended with a newline), so copying joins them.
    pub wrapped: bool,
}

impl Line {
    fn new(columns: usize, blank: Cell) -> Self {
        Self { cells: vec![blank; columns], wrapped: false }
    }

    /// The line's text, without trailing blanks.
    pub fn text(&self, from: usize, to: usize) -> String {
        let to = to.min(self.cells.len());
        let mut text: String = self.cells.get(from..to).unwrap_or(&[]).iter().map(|cell| cell.c).collect();
        text.truncate(text.trim_end().len());
        text
    }
}

/// What mouse events the program running in the terminal wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    Off,
    /// Presses and releases (1000).
    Clicks,
    /// Also motion while a button is held (1002).
    Drags,
    /// All motion (1003).
    Motion,
}

/// The cursor as `ESC 7` saves it.
#[derive(Debug, Clone, Copy)]
struct Saved {
    row: usize,
    column: usize,
    style: Style,
    origin: bool,
    graphics: [bool; 2],
    shifted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    /// A sequence too long or malformed: skipped up to its final character.
    CsiIgnore,
    Osc,
    /// DCS, APC, PM and SOS strings: nothing here uses them.
    IgnoreString,
}

/// Lines kept after they scroll off the top.
const SCROLLBACK: usize = 5000;
const MAX_PARAMETERS: usize = 32;
const MAX_OSC: usize = 4096;

pub struct Screen {
    pub columns: usize,
    pub rows: usize,
    lines: Vec<Line>,
    /// The other screen: the main one while the alternate one is showing.
    other: Vec<Line>,
    pub alternate: bool,
    pub scrollback: VecDeque<Line>,
    /// Lines that fell off the front of the scrollback, ever: line numbers
    /// that count from the first line stay valid as lines drop.
    pub dropped: u64,
    pub row: usize,
    pub column: usize,
    /// The last column was just written: the next character wraps first.
    pending_wrap: bool,
    style: Style,
    saved: [Saved; 2],
    /// The scrolling region, `top..bottom`.
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    /// G0 and G1 are DEC's line-drawing set rather than ASCII; `shifted`
    /// means G1 is in use (after SO).
    graphics: [bool; 2],
    shifted: bool,
    last: char,

    // Modes.
    origin: bool,
    autowrap: bool,
    insert: bool,
    newline: bool,
    pub application_cursor: bool,
    pub cursor_visible: bool,
    pub reverse_video: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub mouse_sgr: bool,
    pub focus_reports: bool,
    /// Without mouse reports, the wheel on the alternate screen sends
    /// arrow keys (so it scrolls `less` and `man`).
    pub alternate_scroll: bool,
    /// The cursor's shape (DECSCUSR): 0-2 block, 3-4 underline, 5-6 bar.
    pub cursor_shape: u8,

    // The parser.
    state: State,
    parameters: Vec<(u32, bool)>,
    /// `?`, `>`, `=` or `<` before the parameters.
    private: Option<char>,
    intermediate: Option<char>,
    osc: String,
    /// ESC seen inside a string: a `\` ends it.
    string_escape: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,

    /// Bytes to send back to the program (answers to its queries).
    pub replies: Vec<u8>,
    /// A new title the program asked for.
    pub title: Option<String>,
    pub bell: bool,
    /// Lines the whole screen scrolled up since `take_scrolled` was last
    /// called (the renderer moves pixels instead of redrawing).
    scrolled: usize,
    /// The palette's background and foreground, for answering queries.
    pub default_colors: (u32, u32),
}

impl Screen {
    pub fn new(columns: usize, rows: usize) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let saved =
            Saved { row: 0, column: 0, style: Style::PLAIN, origin: false, graphics: [false; 2], shifted: false };
        Self {
            columns,
            rows,
            lines: vec![Line::new(columns, Cell::BLANK); rows],
            other: vec![Line::new(columns, Cell::BLANK); rows],
            alternate: false,
            scrollback: VecDeque::new(),
            dropped: 0,
            row: 0,
            column: 0,
            pending_wrap: false,
            style: Style::PLAIN,
            saved: [saved; 2],
            top: 0,
            bottom: rows,
            tabs: (0..columns).map(|column| column % 8 == 0).collect(),
            graphics: [false; 2],
            shifted: false,
            last: ' ',
            origin: false,
            autowrap: true,
            insert: false,
            newline: false,
            application_cursor: false,
            cursor_visible: true,
            reverse_video: false,
            bracketed_paste: false,
            mouse: MouseMode::Off,
            mouse_sgr: false,
            focus_reports: false,
            alternate_scroll: true,
            cursor_shape: 0,
            state: State::Ground,
            parameters: Vec::new(),
            private: None,
            intermediate: None,
            osc: String::new(),
            string_escape: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
            replies: Vec::new(),
            title: None,
            bell: false,
            scrolled: 0,
            default_colors: (0, 0),
        }
    }

    /// Row `row` of what's on the screen now.
    pub fn line(&self, row: usize) -> &Line {
        &self.lines[row]
    }

    /// Lines that scrolled off the whole screen since the last call.
    pub fn take_scrolled(&mut self) -> usize {
        core::mem::take(&mut self.scrolled)
    }

    /// A blank cell in the current colors: erasing fills with the
    /// background color in use (as xterm does, and its terminfo says).
    fn blank(&self) -> Cell {
        Cell { c: ' ', style: Style { background: self.style.background, ..Style::PLAIN } }
    }

    /// Reads a program's output.
    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.utf8_need > 0 {
                if byte & 0xC0 == 0x80 {
                    self.utf8[self.utf8_len] = byte;
                    self.utf8_len += 1;
                    if self.utf8_len == self.utf8_need {
                        let c = core::str::from_utf8(&self.utf8[..self.utf8_len])
                            .ok()
                            .and_then(|text| text.chars().next())
                            .unwrap_or(char::REPLACEMENT_CHARACTER);
                        self.utf8_need = 0;
                        self.input(c);
                    }
                    continue;
                }
                // Cut short: what we had was garbage; this byte starts afresh.
                self.utf8_need = 0;
                self.input(char::REPLACEMENT_CHARACTER);
            }
            let need = match byte {
                0x00..=0x7F => {
                    self.input(char::from(byte));
                    continue;
                }
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => {
                    self.input(char::REPLACEMENT_CHARACTER);
                    continue;
                }
            };
            self.utf8[0] = byte;
            self.utf8_len = 1;
            self.utf8_need = need;
        }
    }

    fn input(&mut self, c: char) {
        // CAN and SUB abort a sequence; ESC starts a new one anywhere
        // (except where it may begin a string's terminator).
        match (self.state, c) {
            (State::Osc | State::IgnoreString, _) => return self.string_input(c),
            (_, '\x18' | '\x1A') => {
                self.state = State::Ground;
                return;
            }
            (_, '\x1B') => {
                self.state = State::Escape;
                self.intermediate = None;
                return;
            }
            (_, '\0'..='\x1F') => return self.control(c),
            _ => {}
        }
        match self.state {
            State::Ground => {
                if c != '\x7F' {
                    self.print(c);
                }
            }
            State::Escape => self.escape(c),
            State::Csi => self.csi_input(c),
            State::CsiIgnore => {
                if ('@'..='~').contains(&c) {
                    self.state = State::Ground;
                }
            }
            State::Osc | State::IgnoreString => {}
        }
    }

    fn string_input(&mut self, c: char) {
        if self.string_escape {
            self.string_escape = false;
            if c == '\\' {
                self.end_string();
                return;
            }
            // Some other sequence: the string ends and that one begins.
            self.end_string();
            self.state = State::Escape;
            self.intermediate = None;
            return self.input(c);
        }
        match c {
            '\x07' => self.end_string(),
            '\x1B' => self.string_escape = true,
            '\x18' | '\x1A' => self.state = State::Ground,
            _ if self.state == State::Osc && self.osc.len() < MAX_OSC => self.osc.push(c),
            _ => {}
        }
    }

    fn end_string(&mut self) {
        if self.state == State::Osc {
            let osc = core::mem::take(&mut self.osc);
            self.operating_system_command(&osc);
        }
        self.state = State::Ground;
    }

    fn control(&mut self, c: char) {
        match c {
            '\x07' => self.bell = true,
            '\x08' => {
                self.pending_wrap = false;
                self.column = self.column.saturating_sub(1);
            }
            '\t' => self.tab_forward(1),
            '\n' | '\x0B' | '\x0C' => {
                self.index();
                if self.newline {
                    self.column = 0;
                }
            }
            '\r' => {
                self.column = 0;
                self.pending_wrap = false;
            }
            '\x0E' => self.shifted = true,
            '\x0F' => self.shifted = false,
            _ => {}
        }
    }

    fn escape(&mut self, c: char) {
        if let Some(designate) = self.intermediate {
            self.state = State::Ground;
            match designate {
                '(' => self.graphics[0] = c == '0',
                ')' => self.graphics[1] = c == '0',
                '#' if c == '8' => self.alignment_test(),
                _ => {}
            }
            return;
        }
        self.state = State::Ground;
        match c {
            '[' => {
                self.state = State::Csi;
                self.parameters.clear();
                self.private = None;
                self.intermediate = None;
            }
            ']' => {
                self.state = State::Osc;
                self.osc.clear();
                self.string_escape = false;
            }
            'P' | 'X' | '^' | '_' => {
                self.state = State::IgnoreString;
                self.string_escape = false;
            }
            ' '..='/' => {
                self.intermediate = Some(c);
                self.state = State::Escape;
            }
            '7' => self.save_cursor(),
            '8' => self.restore_cursor(),
            'D' => self.index(),
            'E' => {
                self.index();
                self.column = 0;
            }
            'H' => self.tabs[self.column] = true,
            'M' => self.reverse_index(),
            'c' => self.reset(),
            // Keypad modes (our keypad always sends digits) and ST.
            _ => {}
        }
    }

    fn csi_input(&mut self, c: char) {
        match c {
            '0'..='9' => {
                if self.intermediate.is_some() {
                    self.state = State::CsiIgnore;
                    return;
                }
                if self.parameters.is_empty() {
                    self.parameters.push((0, false));
                }
                let last = self.parameters.last_mut().unwrap();
                last.0 = (last.0 * 10 + c.to_digit(10).unwrap()).min(65535);
            }
            ';' | ':' => {
                if self.parameters.is_empty() {
                    self.parameters.push((0, false));
                }
                if self.parameters.len() == MAX_PARAMETERS {
                    self.state = State::CsiIgnore;
                    return;
                }
                self.parameters.push((0, c == ':'));
            }
            '<'..='?' => {
                if self.parameters.is_empty() && self.private.is_none() {
                    self.private = Some(c);
                } else {
                    self.state = State::CsiIgnore;
                }
            }
            ' '..='/' => self.intermediate = Some(c),
            '@'..='~' => {
                self.state = State::Ground;
                self.csi(c);
            }
            _ => self.state = State::CsiIgnore,
        }
    }

    /// Parameter `index`, with 0 or a missing one meaning `default`.
    fn parameter(&self, index: usize, default: usize) -> usize {
        match self.parameters.get(index) {
            Some(&(value, _)) if value != 0 => value as usize,
            _ => default,
        }
    }

    fn csi(&mut self, c: char) {
        let n = self.parameter(0, 1);
        match (self.private, self.intermediate, c) {
            (None, None, '@') => self.insert_blanks(n),
            (None, None, 'A') => self.move_up(n),
            (None, None, 'B' | 'e') => self.move_down(n),
            (None, None, 'C' | 'a') => self.move_to(self.row, self.column.saturating_add(n)),
            (None, None, 'D') => self.move_to(self.row, self.column.saturating_sub(n)),
            (None, None, 'E') => {
                self.move_down(n);
                self.column = 0;
            }
            (None, None, 'F') => {
                self.move_up(n);
                self.column = 0;
            }
            (None, None, 'G' | '`') => self.move_to(self.row, n - 1),
            (None, None, 'H' | 'f') => {
                let row = self.parameter(0, 1) - 1;
                let column = self.parameter(1, 1) - 1;
                self.move_to_origin(row, column);
            }
            (None, None, 'I') => self.tab_forward(n),
            (None | Some('?'), None, 'J') => self.erase_display(self.parameter(0, 0)),
            (None | Some('?'), None, 'K') => self.erase_line(self.parameter(0, 0)),
            (None, None, 'L') => self.insert_lines(n),
            (None, None, 'M') => self.delete_lines(n),
            (None, None, 'P') => self.delete_characters(n),
            (None, None, 'S') => self.scroll_up(n),
            (None, None, 'T') => self.scroll_down(n),
            (None, None, 'X') => {
                let blank = self.blank();
                let (row, from) = (self.row, self.column);
                let to = (from + n).min(self.columns);
                self.lines[row].cells[from..to].fill(blank);
                self.pending_wrap = false;
            }
            (None, None, 'Z') => {
                for _ in 0..n {
                    self.column = self.column.saturating_sub(1);
                    while self.column > 0 && !self.tabs[self.column] {
                        self.column -= 1;
                    }
                }
                self.pending_wrap = false;
            }
            (None, None, 'b') => {
                for _ in 0..n.min(65535) {
                    self.print(self.last);
                }
            }
            (None, None, 'c') => {
                if self.parameter(0, 0) == 0 {
                    // A VT220 with ANSI color.
                    self.replies.extend_from_slice(b"\x1b[?62;22c");
                }
            }
            (Some('>'), None, 'c') => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            (None, None, 'd') => {
                let row = n - 1;
                self.move_to_origin(row, self.column);
            }
            (None, None, 'g') => match self.parameter(0, 0) {
                0 => self.tabs[self.column] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            (None, None, 'h') => self.set_modes(true),
            (None, None, 'l') => self.set_modes(false),
            (Some('?'), None, 'h') => self.set_private_modes(true),
            (Some('?'), None, 'l') => self.set_private_modes(false),
            (None, None, 'm') => self.select_graphic_rendition(),
            (None, None, 'n') => match self.parameter(0, 0) {
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let row = if self.origin { self.row - self.top } else { self.row };
                    let reply = format!("\x1b[{};{}R", row + 1, self.column + 1);
                    self.replies.extend_from_slice(reply.as_bytes());
                }
                _ => {}
            },
            (None, None, 'r') => {
                let top = self.parameter(0, 1) - 1;
                let bottom = self.parameter(1, self.rows).min(self.rows);
                if top + 1 < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.move_to_origin(0, 0);
                }
            }
            (None, None, 's') => self.save_cursor(),
            (None, None, 'u') => self.restore_cursor(),
            (None, Some(' '), 'q') => self.cursor_shape = self.parameter(0, 0).min(6) as u8,
            (None, Some('!'), 'p') => self.soft_reset(),
            _ => {}
        }
    }

    fn set_modes(&mut self, on: bool) {
        for index in 0..self.parameters.len().max(1) {
            match self.parameter(index, 0) {
                4 => self.insert = on,
                20 => self.newline = on,
                _ => {}
            }
        }
    }

    fn set_private_modes(&mut self, on: bool) {
        for index in 0..self.parameters.len().max(1) {
            match self.parameter(index, 0) {
                1 => self.application_cursor = on,
                5 => self.reverse_video = on,
                6 => {
                    self.origin = on;
                    self.move_to_origin(0, 0);
                }
                7 => {
                    self.autowrap = on;
                    self.pending_wrap = false;
                }
                25 => self.cursor_visible = on,
                47 | 1047 => self.switch_screen(on, false),
                1048 => {
                    if on {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                1049 => {
                    if on {
                        self.save_cursor();
                        self.switch_screen(true, true);
                    } else {
                        self.switch_screen(false, false);
                        self.restore_cursor();
                    }
                }
                1000 => self.mouse = if on { MouseMode::Clicks } else { MouseMode::Off },
                1002 => self.mouse = if on { MouseMode::Drags } else { MouseMode::Off },
                1003 => self.mouse = if on { MouseMode::Motion } else { MouseMode::Off },
                1004 => self.focus_reports = on,
                1006 => self.mouse_sgr = on,
                1007 => self.alternate_scroll = on,
                2004 => self.bracketed_paste = on,
                _ => {}
            }
        }
    }

    fn switch_screen(&mut self, alternate: bool, clear: bool) {
        if alternate != self.alternate {
            core::mem::swap(&mut self.lines, &mut self.other);
            self.alternate = alternate;
        }
        if alternate && clear {
            let blank = self.blank();
            for line in &mut self.lines {
                *line = Line::new(self.columns, blank);
            }
        }
        self.pending_wrap = false;
    }

    fn select_graphic_rendition(&mut self) {
        if self.parameters.is_empty() {
            self.style = Style::PLAIN;
            return;
        }
        let mut index = 0;
        while index < self.parameters.len() {
            let (value, _) = self.parameters[index];
            index += 1;
            // Sub-parameters that follow (4:3, 38:2::R:G:B).
            let subparameters = self.parameters[index..].iter().take_while(|&&(_, sub)| sub).count();
            let style = &mut self.style;
            match value {
                0 => *style = Style::PLAIN,
                1 => style.flags |= BOLD,
                2 => style.flags |= DIM,
                3 => style.flags |= ITALIC,
                4 => {
                    // 4:0 is "no underline"; 4:N any kind of underline.
                    if subparameters > 0 && self.parameters[index].0 == 0 {
                        style.flags &= !UNDERLINE;
                    } else {
                        style.flags |= UNDERLINE;
                    }
                }
                7 => style.flags |= INVERSE,
                8 => style.flags |= HIDDEN,
                9 => style.flags |= STRIKE,
                21 => style.flags |= UNDERLINE,
                22 => style.flags &= !(BOLD | DIM),
                23 => style.flags &= !ITALIC,
                24 => style.flags &= !UNDERLINE,
                27 => style.flags &= !INVERSE,
                28 => style.flags &= !HIDDEN,
                29 => style.flags &= !STRIKE,
                30..=37 => style.foreground = Color::Index((value - 30) as u8),
                39 => style.foreground = Color::Default,
                40..=47 => style.background = Color::Index((value - 40) as u8),
                49 => style.background = Color::Default,
                90..=97 => style.foreground = Color::Index((value - 90 + 8) as u8),
                100..=107 => style.background = Color::Index((value - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, used) = if subparameters > 0 {
                        (self.extended_color(index, subparameters, true), subparameters)
                    } else {
                        let rest = self.parameters.len() - index;
                        let used = match self.parameters.get(index).map(|&(kind, _)| kind) {
                            Some(5) => 2.min(rest),
                            Some(2) => 4.min(rest),
                            _ => rest.min(1),
                        };
                        (self.extended_color(index, used, false), used)
                    };
                    index += used;
                    match (value, color) {
                        (38, Some(color)) => self.style.foreground = color,
                        (48, Some(color)) => self.style.background = color,
                        _ => {}
                    }
                    continue;
                }
                _ => {}
            }
            index += subparameters;
        }
    }

    /// The color in `count` parameters from `index`: `5;N` or `2;R;G;B`
    /// (with colons, `2` may be followed by a color space id first).
    fn extended_color(&self, index: usize, count: usize, colons: bool) -> Option<Color> {
        let values: Vec<u32> = self.parameters[index..index + count].iter().map(|&(value, _)| value).collect();
        match values.as_slice() {
            [5, n, ..] => Some(Color::Index(*n as u8)),
            [2, _, r, g, b, ..] if colons => Some(Color::Rgb((r & 255) << 16 | (g & 255) << 8 | (b & 255))),
            [2, r, g, b, ..] => Some(Color::Rgb((r & 255) << 16 | (g & 255) << 8 | (b & 255))),
            _ => None,
        }
    }

    fn operating_system_command(&mut self, command: &str) {
        let (number, rest) = command.split_once(';').unwrap_or((command, ""));
        match number {
            "0" | "2" => self.title = Some(String::from(rest)),
            // Color queries: programs pick light or dark themes by them.
            "10" | "11" if rest == "?" => {
                let color = if number == "10" { self.default_colors.1 } else { self.default_colors.0 };
                let channel = |shift: u32| (color >> shift & 255) * 0x101;
                let reply =
                    format!("\x1b]{number};rgb:{:04x}/{:04x}/{:04x}\x1b\\", channel(16), channel(8), channel(0));
                self.replies.extend_from_slice(reply.as_bytes());
            }
            _ => {}
        }
    }

    /// Puts `c` at the cursor and moves it on.
    fn print(&mut self, mut c: char) {
        // Combining marks would need a second character per cell; they're
        // dropped (the base character still shows).
        if ('\u{300}'..='\u{36F}').contains(&c) || ('\u{200B}'..='\u{200F}').contains(&c) || c == '\u{FE0F}' {
            return;
        }
        if self.graphics[usize::from(self.shifted)] {
            c = dec_graphics(c);
        }
        if self.pending_wrap && self.autowrap {
            self.lines[self.row].wrapped = true;
            self.index();
            self.column = 0;
        }
        self.pending_wrap = false;
        let (row, column) = (self.row, self.column);
        let cells = &mut self.lines[row].cells;
        if self.insert {
            cells.insert(column, Cell::BLANK);
            cells.truncate(self.columns);
        }
        cells[column] = Cell { c, style: self.style };
        self.last = c;
        if column + 1 >= self.columns {
            self.pending_wrap = self.autowrap;
        } else {
            self.column += 1;
        }
    }

    /// Down a line, scrolling at the bottom of the scrolling region.
    fn index(&mut self) {
        self.pending_wrap = false;
        if self.row + 1 == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.row == self.top {
            self.scroll_down(1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
    }

    /// Scrolls the region up `n` lines. Lines leaving the top of the main
    /// screen go to the scrollback.
    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top);
        let blank = self.blank();
        let keep = self.top == 0 && !self.alternate;
        let removed: Vec<Line> = self.lines.drain(self.top..self.top + n).collect();
        if keep {
            for line in removed {
                if self.scrollback.len() == SCROLLBACK {
                    self.scrollback.pop_front();
                    self.dropped += 1;
                }
                self.scrollback.push_back(line);
            }
        }
        for _ in 0..n {
            self.lines.insert(self.bottom - n, Line::new(self.columns, blank));
        }
        if self.top == 0 && self.bottom == self.rows {
            self.scrolled += n;
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top);
        let blank = self.blank();
        self.lines.drain(self.bottom - n..self.bottom);
        for _ in 0..n {
            self.lines.insert(self.top, Line::new(self.columns, blank));
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if (self.top..self.bottom).contains(&self.row) {
            let top = core::mem::replace(&mut self.top, self.row);
            self.scroll_down(n);
            self.top = top;
            self.column = 0;
            self.pending_wrap = false;
        }
    }

    fn delete_lines(&mut self, n: usize) {
        if (self.top..self.bottom).contains(&self.row) {
            let (top, alternate) = (core::mem::replace(&mut self.top, self.row), self.alternate);
            // Deleted lines never go to the scrollback.
            self.alternate = true;
            self.scroll_up(n);
            self.alternate = alternate;
            self.top = top;
            self.column = 0;
            self.pending_wrap = false;
        }
    }

    fn insert_blanks(&mut self, n: usize) {
        let blank = self.blank();
        let (columns, column) = (self.columns, self.column);
        let cells = &mut self.lines[self.row].cells;
        let n = n.min(columns - column);
        cells.truncate(columns - n);
        for _ in 0..n {
            cells.insert(column, blank);
        }
        self.pending_wrap = false;
    }

    fn delete_characters(&mut self, n: usize) {
        let blank = self.blank();
        let (columns, column) = (self.columns, self.column);
        let cells = &mut self.lines[self.row].cells;
        let n = n.min(columns - column);
        cells.drain(column..column + n);
        cells.resize(columns, blank);
        self.pending_wrap = false;
    }

    fn erase_display(&mut self, how: usize) {
        let blank = self.blank();
        let (row, column) = (self.row, self.column);
        match how {
            0 => {
                self.erase_line(0);
                for line in &mut self.lines[row + 1..] {
                    *line = Line::new(self.columns, blank);
                }
            }
            1 => {
                self.erase_line(1);
                for line in &mut self.lines[..row] {
                    *line = Line::new(self.columns, blank);
                }
            }
            2 => {
                for line in &mut self.lines {
                    *line = Line::new(self.columns, blank);
                }
            }
            3 => {
                self.dropped += self.scrollback.len() as u64;
                self.scrollback.clear();
            }
            _ => {}
        }
        self.column = column;
        self.pending_wrap = false;
    }

    fn erase_line(&mut self, how: usize) {
        let blank = self.blank();
        let (columns, column) = (self.columns, self.column.min(self.columns - 1));
        let line = &mut self.lines[self.row];
        match how {
            0 => {
                line.cells[column..].fill(blank);
                line.wrapped = false;
            }
            1 => line.cells[..=column].fill(blank),
            2 => *line = Line::new(columns, blank),
            _ => {}
        }
        self.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        let limit = if self.row >= self.top { self.top } else { 0 };
        self.row = self.row.saturating_sub(n).max(limit);
        self.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let limit = if self.row < self.bottom { self.bottom - 1 } else { self.rows - 1 };
        self.row = self.row.saturating_add(n).min(limit);
        self.pending_wrap = false;
    }

    fn move_to(&mut self, row: usize, column: usize) {
        self.row = row.min(self.rows - 1);
        self.column = column.min(self.columns - 1);
        self.pending_wrap = false;
    }

    /// Moves to a position counted from the scrolling region's top in
    /// origin mode, and confined to it.
    fn move_to_origin(&mut self, row: usize, column: usize) {
        if self.origin {
            self.move_to((self.top + row).min(self.bottom - 1), column);
        } else {
            self.move_to(row, column);
        }
    }

    fn tab_forward(&mut self, n: usize) {
        for _ in 0..n {
            while self.column + 1 < self.columns {
                self.column += 1;
                if self.tabs[self.column] {
                    break;
                }
            }
        }
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved[usize::from(self.alternate)] = Saved {
            row: self.row,
            column: self.column,
            style: self.style,
            origin: self.origin,
            graphics: self.graphics,
            shifted: self.shifted,
        };
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved[usize::from(self.alternate)];
        self.style = saved.style;
        self.origin = saved.origin;
        self.graphics = saved.graphics;
        self.shifted = saved.shifted;
        self.move_to(saved.row, saved.column);
    }

    fn alignment_test(&mut self) {
        let cell = Cell { c: 'E', style: Style::PLAIN };
        for line in &mut self.lines {
            *line = Line::new(self.columns, cell);
        }
        self.move_to(0, 0);
    }

    fn soft_reset(&mut self) {
        self.style = Style::PLAIN;
        self.origin = false;
        self.autowrap = true;
        self.insert = false;
        self.application_cursor = false;
        self.cursor_visible = true;
        self.top = 0;
        self.bottom = self.rows;
        self.graphics = [false; 2];
        self.shifted = false;
        self.pending_wrap = false;
    }

    /// `ESC c`: back to how it started, keeping the scrollback.
    fn reset(&mut self) {
        let scrollback = core::mem::take(&mut self.scrollback);
        let (dropped, colors) = (self.dropped, self.default_colors);
        if self.alternate {
            self.switch_screen(false, false);
        }
        *self = Screen::new(self.columns, self.rows);
        self.scrollback = scrollback;
        self.dropped = dropped;
        self.default_colors = colors;
    }

    /// Changes the size. Lines aren't rewrapped (as in xterm); when rows
    /// go, lines above the cursor move to the scrollback so it stays on
    /// the screen, and they come back when rows return.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        let (columns, rows) = (columns.max(1), rows.max(1));
        if (columns, rows) == (self.columns, self.rows) {
            return;
        }
        for screen in [&mut self.lines, &mut self.other] {
            for line in screen.iter_mut() {
                line.cells.resize(columns, Cell::BLANK);
            }
        }
        let main_is_current = !self.alternate;
        // The main screen: shrink from the top while the cursor is below
        // the new bottom (from the bottom otherwise); grow from the
        // scrollback.
        {
            let (main, cursor_row) = if main_is_current {
                (&mut self.lines, &mut self.row)
            } else {
                (&mut self.other, &mut self.saved[0].row)
            };
            while main.len() > rows {
                if *cursor_row >= rows {
                    let line = main.remove(0);
                    if self.scrollback.len() == SCROLLBACK {
                        self.scrollback.pop_front();
                        self.dropped += 1;
                    }
                    self.scrollback.push_back(line);
                    *cursor_row -= 1;
                } else {
                    main.pop();
                }
            }
            while main.len() < rows {
                if let Some(mut line) = self.scrollback.pop_back() {
                    line.cells.resize(columns, Cell::BLANK);
                    main.insert(0, line);
                    *cursor_row += 1;
                } else {
                    main.push(Line::new(columns, Cell::BLANK));
                }
            }
        }
        let alternate = if main_is_current { &mut self.other } else { &mut self.lines };
        alternate.resize(rows, Line::new(columns, Cell::BLANK));
        self.columns = columns;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows;
        self.tabs = (0..columns).map(|column| column % 8 == 0).collect();
        for saved in &mut self.saved {
            saved.row = saved.row.min(rows - 1);
            saved.column = saved.column.min(columns - 1);
        }
        self.move_to(self.row, self.column);
        self.scrolled = 0;
    }
}

/// DEC's special graphics set, which `ESC ( 0` switches to: line drawing
/// on lowercase letters.
fn dec_graphics(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}
