//! term: eliOS's terminal.
//!
//! `term [command...]` opens a window running the user's shell (or the
//! command) on a pseudo-terminal, and shows what it writes as xterm
//! would: colors, cursor movement, the alternate screen of full-screen
//! programs, mouse reports. Lines that scroll off the top are kept (5000
//! of them) and can be scrolled back to with the wheel or Shift+PageUp.
//!
//! - Selecting with the mouse copies (double click: a word, triple: a
//!   line); the middle button, Shift+Insert and Ctrl+Shift+V paste, and
//!   Ctrl+Shift+C copies again.
//! - Ctrl+= and Ctrl+- change the font size, Ctrl+0 resets it.
//! - With Shift held, the mouse selects even when a program wants it.
//!
//! Only the rows that changed are drawn again, and output that scrolls the
//! whole screen moves its pixels rather than redrawing them; drawing waits
//! for the desktop to be ready for a frame, so a program printing as fast
//! as it can costs a frame's work per frame, not per line.

#![no_std]
#![no_main]

extern crate alloc;

mod boxes;
mod pty;
mod screen;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use screen::{BOLD, Cell, Color, DIM, HIDDEN, INVERSE, Line, MouseMode, STRIKE, Screen, UNDERLINE};
use ui::draw::mix;
use ui::widgets::theme;
use ui::{Button, Canvas, Event, Font, Key, Modifiers, Rect, Window};

eli::main!(main);

const FONT_SIZE: f32 = 15.0;
/// Space between the window's edge and the text.
const PADDING: i32 = 4;

const BACKGROUND: u32 = theme::BACKGROUND;
const FOREGROUND: u32 = theme::TEXT;
const CURSOR: u32 = 0xFFE8_EAF2;
const SELECTION: u32 = theme::SELECTION;
/// The 16 basic colors, chosen to sit on the desktop's dark blue.
const PALETTE: [u32; 16] = [
    0x262C48, 0xE0606E, 0x8CC77A, 0xE5C07B, 0x6A9EF0, 0xC48AE0, 0x5CC5D0, 0xC8CCDA, // normal
    0x5C6488, 0xFF7A88, 0xA8E090, 0xF5D58F, 0x8AB6FF, 0xD8A6F5, 0x7FE0E8, 0xF2F4FA, // bright
];

/// The color of palette entry `index`: the 16 above, then xterm's 6x6x6
/// cube and 24 grays.
fn palette(index: u8) -> u32 {
    let rgb = match index {
        0..=15 => PALETTE[index as usize],
        16..=231 => {
            let n = u32::from(index - 16);
            let level = |value: u32| if value == 0 { 0 } else { 55 + value * 40 };
            level(n / 36) << 16 | level(n / 6 % 6) << 8 | level(n % 6)
        }
        _ => {
            let gray = 8 + u32::from(index - 232) * 10;
            gray << 16 | gray << 8 | gray
        }
    };
    0xFF00_0000 | rgb
}

/// How text is picked out with the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Characters,
    Words,
    Lines,
}

/// A place in the text: a line number counting from the first line ever
/// kept (see `Screen::dropped`) and a column.
type Position = (u64, usize);

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: Position,
    head: Position,
    unit: Unit,
}

/// What a row of the window was last drawn as.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    cells: Vec<Cell>,
    cursor: Option<(usize, bool, u8)>,
    selected: Option<(usize, usize)>,
    reverse: bool,
}

struct Terminal {
    window: Window,
    font: Font,
    font_size: f32,
    cell_width: i32,
    cell_height: i32,
    screen: Screen,
    pty: i32,
    child: u32,
    /// How many lines back into the scrollback the view is.
    view: usize,
    selection: Option<Selection>,
    /// The left button is down, extending the selection.
    selecting: bool,
    /// The latest selection, which the middle button pastes (like X's
    /// primary selection; the desktop's clipboard is Ctrl+Shift+C).
    primary: String,
    focused: bool,
    /// Each row as last drawn (`None`: has to be drawn).
    shown: Vec<Option<Row>>,
    /// Which button a program is told is held, for drag reports.
    reported_button: Option<u32>,
    last_report: (usize, usize),
    pointer: (i32, i32),
    title: String,
    /// The scrollbar as last drawn: (view, scrollback length).
    scrollbar: (usize, usize),
}

fn main() -> i32 {
    let mut arguments = eli::env::args();
    arguments.remove(0);
    if arguments.first().is_some_and(|argument| argument == "-e") {
        arguments.remove(0);
    }
    let command = if arguments.is_empty() {
        let shell = eli::env::var("SHELL")
            .or_else(|| eli::users::user_with_id(eli::users::uid()).map(|user| String::from(user.login_shell())))
            .unwrap_or_else(|| String::from("/bin/sh"));
        vec![shell]
    } else {
        arguments
    };
    let font = match Font::load(FONT_SIZE) {
        Ok(font) => font,
        Err(error) => {
            eli::eprintln!("term: {error}");
            return 1;
        }
    };
    let mut terminal = match Terminal::new(font, &command) {
        Ok(terminal) => terminal,
        Err(error) => {
            eli::eprintln!("term: {error}");
            return 1;
        }
    };
    terminal.run()
}

impl Terminal {
    fn new(mut font: Font, command: &[String]) -> Result<Self, String> {
        let (cell_width, cell_height) = cell_size(&mut font);
        let title = String::from("Terminal");
        let window = Window::open(&title, "term", 80 * cell_width + 2 * PADDING, 24 * cell_height + 2 * PADDING)?;
        let (columns, rows) = grid_size(window.width, window.height, cell_width, cell_height);
        let (pty, child) = pty::spawn(command, columns, rows)
            .map_err(|error| alloc::format!("can't start {}: {error}", command[0]))?;
        let mut screen = Screen::new(columns, rows);
        screen.default_colors = (BACKGROUND & 0xFF_FFFF, FOREGROUND & 0xFF_FFFF);
        Ok(Self {
            window,
            font,
            font_size: FONT_SIZE,
            cell_width,
            cell_height,
            screen,
            pty,
            child,
            view: 0,
            selection: None,
            selecting: false,
            primary: String::new(),
            focused: true,
            shown: Vec::new(),
            reported_button: None,
            last_report: (0, 0),
            pointer: (0, 0),
            title,
            scrollbar: (0, 0),
        })
    }

    fn run(&mut self) -> i32 {
        loop {
            for event in self.window.wait(&[self.pty], -1) {
                match event {
                    Event::Close => return self.quit(),
                    Event::Readable(_) => {
                        if !self.read_output() {
                            return self.quit();
                        }
                    }
                    Event::Resize { .. } => self.resize(),
                    Event::Focus(focused) => {
                        self.focused = focused;
                        if self.screen.focus_reports {
                            self.send(if focused { b"\x1b[I" } else { b"\x1b[O" });
                        }
                    }
                    Event::Key { key, modifiers } => self.key(key, modifiers),
                    Event::PointerMove { x, y } => self.pointer_moved(x, y),
                    Event::PointerLeave => {}
                    Event::Button { button, pressed, x, y, clicks } => self.button(button, pressed, x, y, clicks),
                    Event::Scroll { lines } => self.wheel(lines),
                }
            }
            let replies = core::mem::take(&mut self.screen.replies);
            if !replies.is_empty() {
                self.send(&replies);
            }
            if let Some(title) = self.screen.title.take() {
                let title = if title.is_empty() { String::from("Terminal") } else { title };
                if title != self.title {
                    self.window.set_title(&title);
                    self.title = title;
                }
            }
            if self.window.ready() {
                self.draw();
            }
        }
    }

    /// Ends the program the terminal ran, if it's still going, and says
    /// how it ended.
    fn quit(&mut self) -> i32 {
        let _ = eli::io::close(self.pty);
        match eli::process::wait(self.child as i32, false) {
            Ok(Some((_, status))) => status.shell_code(),
            _ => 0,
        }
    }

    /// Reads what the program wrote; returns false once it's gone.
    fn read_output(&mut self) -> bool {
        let mut buffer = [0u8; 16384];
        // A limit, so a program that never stops writing can't keep the
        // terminal from drawing or reading keys.
        for _ in 0..64 {
            match eli::io::read(self.pty, &mut buffer) {
                Ok(0) => return false,
                Ok(count) => {
                    let before = self.screen.scrollback.len() as u64 + self.screen.dropped;
                    self.screen.write(&buffer[..count]);
                    // Looking back: stay on the same lines as more arrive.
                    if self.view > 0 {
                        let added = (self.screen.scrollback.len() as u64 + self.screen.dropped - before) as usize;
                        self.view = (self.view + added).min(self.screen.scrollback.len());
                    }
                }
                Err(error) if error == eli::sys::Error::AGAIN => return true,
                Err(error) if error == eli::sys::Error::INTERRUPTED => {}
                Err(_) => return false,
            }
        }
        true
    }

    fn send(&mut self, bytes: &[u8]) {
        let _ = eli::io::write_all(self.pty, bytes);
    }

    fn resize(&mut self) {
        let (columns, rows) = grid_size(self.window.width, self.window.height, self.cell_width, self.cell_height);
        if (columns, rows) != (self.screen.columns, self.screen.rows) {
            self.screen.resize(columns, rows);
            pty::set_size(self.pty, columns, rows);
        }
        self.view = self.view.min(self.screen.scrollback.len());
        self.selection = None;
        self.shown.clear();
        let mut canvas = self.window.canvas();
        canvas.fill(canvas.bounds(), BACKGROUND);
        self.window.present_all();
    }

    fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(8.0, 40.0);
        if size == self.font_size {
            return;
        }
        self.font_size = size;
        self.font = self.font.resized(size);
        (self.cell_width, self.cell_height) = cell_size(&mut self.font);
        self.resize();
    }

    // Input.

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        let control_shift = modifiers.control && modifiers.shift;
        match key {
            Key::Char('C' | 'c') if control_shift => {
                if let Some(text) = self.selected_text() {
                    self.window.set_clipboard(&text);
                }
                return;
            }
            Key::Char('V' | 'v') if control_shift => return self.paste_clipboard(),
            Key::Insert if modifiers.shift => return self.paste_clipboard(),
            Key::PageUp if modifiers.shift => return self.scroll_view(self.screen.rows as i32 - 1),
            Key::PageDown if modifiers.shift => return self.scroll_view(-(self.screen.rows as i32 - 1)),
            Key::Char('=' | '+') if modifiers.control => return self.set_font_size(self.font_size + 1.0),
            Key::Char('-' | '_') if modifiers.control && !modifiers.shift => {
                return self.set_font_size(self.font_size - 1.0);
            }
            Key::Char('0') if modifiers.control => return self.set_font_size(FONT_SIZE),
            _ => {}
        }
        let bytes = encode_key(key, modifiers, self.screen.application_cursor);
        if !bytes.is_empty() {
            self.view = 0;
            self.send(&bytes);
        }
    }

    fn paste_clipboard(&mut self) {
        if let Some(text) = self.window.clipboard() {
            self.paste(&text);
        }
    }

    fn paste(&mut self, clipboard: &str) {
        if clipboard.is_empty() {
            return;
        }
        // Newlines go as Enter does; escapes are dropped so pasted text
        // can't end a bracketed paste early or drive the terminal.
        let text: Vec<u8> =
            clipboard.replace("\r\n", "\r").replace('\n', "\r").bytes().filter(|&byte| byte != 0x1B).collect();
        self.view = 0;
        if self.screen.bracketed_paste {
            self.send(b"\x1b[200~");
            self.send(&text);
            self.send(b"\x1b[201~");
        } else {
            self.send(&text);
        }
    }

    fn scroll_view(&mut self, lines: i32) {
        if self.screen.alternate {
            return;
        }
        let view = (self.view as i32 + lines).clamp(0, self.screen.scrollback.len() as i32) as usize;
        self.view = view;
    }

    fn wheel(&mut self, lines: i32) {
        if self.reports_mouse() {
            let (column, row) = self.cell_at(self.pointer.0, self.pointer.1);
            let button = if lines < 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(button, true, column, row);
            }
        } else if self.screen.alternate && self.screen.alternate_scroll {
            let arrow: &[u8] = match (lines < 0, self.screen.application_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            for _ in 0..lines.unsigned_abs() * 3 {
                self.send(arrow);
            }
        } else {
            self.scroll_view(-lines * 3);
        }
    }

    /// Whether mouse events go to the program (Shift keeps them here).
    fn reports_mouse(&self) -> bool {
        self.screen.mouse != MouseMode::Off && !self.window.modifiers().shift
    }

    /// The cell under a point in the window, clamped to the grid.
    fn cell_at(&self, x: i32, y: i32) -> (usize, usize) {
        let column = ((x - PADDING).max(0) / self.cell_width) as usize;
        let row = ((y - PADDING).max(0) / self.cell_height) as usize;
        (column.min(self.screen.columns - 1), row.min(self.screen.rows - 1))
    }

    /// The text position of a cell in the view.
    fn position(&self, column: usize, row: usize) -> Position {
        let first = self.screen.dropped + (self.screen.scrollback.len() - self.view) as u64;
        (first + row as u64, column)
    }

    /// Line `number` (see `Position`), if it's still kept.
    fn line(&self, number: u64) -> Option<&Line> {
        let index = number.checked_sub(self.screen.dropped)? as usize;
        let scrollback = self.screen.scrollback.len();
        if index < scrollback {
            self.screen.scrollback.get(index)
        } else if index - scrollback < self.screen.rows {
            Some(self.screen.line(index - scrollback))
        } else {
            None
        }
    }

    /// Sends a mouse event the way the program asked for: `button` is 0-2
    /// for left, middle, right (3: none), 64/65 for the wheel, plus 32
    /// for motion.
    fn report_mouse(&mut self, button: u32, pressed: bool, column: usize, row: usize) {
        let modifiers = self.window.modifiers();
        let mut code = button;
        if modifiers.shift {
            code += 4;
        }
        if modifiers.alt {
            code += 8;
        }
        if modifiers.control {
            code += 16;
        }
        if self.screen.mouse_sgr {
            let end = if pressed { 'M' } else { 'm' };
            let report = alloc::format!("\x1b[<{code};{};{}{end}", column + 1, row + 1);
            self.send(report.as_bytes());
        } else {
            // The old encoding: released is button 3, and positions stop
            // at 223.
            let code = if pressed { code } else { (code & !3) | 3 };
            let at = |value: usize| (value.min(222) + 33) as u8;
            self.send(&[0x1B, b'[', b'M', (code + 32) as u8, at(column), at(row)]);
        }
        self.last_report = (column, row);
    }

    fn pointer_moved(&mut self, x: i32, y: i32) {
        self.pointer = (x, y);
        let (column, row) = self.cell_at(x, y);
        if self.selecting {
            // Dragging past the top or bottom scrolls.
            if y < PADDING {
                self.scroll_view(1);
            } else if y >= PADDING + self.screen.rows as i32 * self.cell_height {
                self.scroll_view(-1);
            }
            let head = self.position(column, row);
            if let Some(selection) = &mut self.selection {
                selection.head = head;
            }
            return;
        }
        if !self.reports_mouse() || (column, row) == self.last_report {
            return;
        }
        match (self.screen.mouse, self.reported_button) {
            (MouseMode::Drags | MouseMode::Motion, Some(button)) => self.report_mouse(button + 32, true, column, row),
            (MouseMode::Motion, None) => self.report_mouse(3 + 32, true, column, row),
            _ => {}
        }
    }

    fn button(&mut self, button: Button, pressed: bool, x: i32, y: i32, clicks: u32) {
        self.pointer = (x, y);
        let (column, row) = self.cell_at(x, y);
        if self.reports_mouse() {
            let code = match button {
                Button::Left => 0,
                Button::Middle => 1,
                Button::Right => 2,
                Button::Other(_) => return,
            };
            self.reported_button = if pressed { Some(code) } else { None };
            self.report_mouse(code, pressed, column, row);
            return;
        }
        match (button, pressed) {
            (Button::Left, true) => {
                let unit = match clicks {
                    1 => Unit::Characters,
                    2 => Unit::Words,
                    _ => Unit::Lines,
                };
                let at = self.position(column, row);
                self.selection = Some(Selection { anchor: at, head: at, unit });
                self.selecting = true;
            }
            (Button::Left, false) => {
                self.selecting = false;
                // A click without a drag selects nothing.
                if let Some(selection) = self.selection
                    && selection.unit == Unit::Characters
                    && selection.anchor == selection.head
                {
                    self.selection = None;
                }
                if let Some(text) = self.selected_text() {
                    self.primary = text;
                }
            }
            (Button::Middle, true) => self.paste(&self.primary.clone()),
            _ => {}
        }
    }

    // Selection.

    /// The selection's ends, first to last, widened to whole words or
    /// lines; the end is inclusive.
    fn selected_range(&self) -> Option<(Position, Position)> {
        let selection = self.selection?;
        let (mut start, mut end) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        match selection.unit {
            Unit::Characters => {}
            Unit::Words => {
                start.1 = self.word_edge(start, false);
                end.1 = self.word_edge(end, true);
            }
            Unit::Lines => {
                start.1 = 0;
                end.1 = self.screen.columns - 1;
            }
        }
        Some((start, end))
    }

    /// Where the word at `at` starts (or ends, with `forward`).
    fn word_edge(&self, at: Position, forward: bool) -> usize {
        let Some(line) = self.line(at.0) else { return at.1 };
        let is_word = |c: char| c.is_alphanumeric() || "-_./~:@%+=?&#".contains(c);
        let cells = &line.cells;
        let mut column = at.1.min(cells.len().saturating_sub(1));
        if !cells.get(column).is_some_and(|cell| is_word(cell.c)) {
            return column;
        }
        if forward {
            while column + 1 < cells.len() && is_word(cells[column + 1].c) {
                column += 1;
            }
        } else {
            while column > 0 && is_word(cells[column - 1].c) {
                column -= 1;
            }
        }
        column
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selected_range()?;
        let mut text = String::new();
        for number in start.0..=end.0 {
            let Some(line) = self.line(number) else { continue };
            let from = if number == start.0 { start.1 } else { 0 };
            let to = if number == end.0 { end.1 + 1 } else { line.cells.len() };
            text.push_str(&line.text(from, to));
            if number != end.0 && !line.wrapped {
                text.push('\n');
            }
        }
        (!text.is_empty()).then_some(text)
    }

    // Drawing.

    fn draw(&mut self) {
        let (columns, rows) = (self.screen.columns, self.screen.rows);
        let scrolled = self.screen.take_scrolled();
        let width = self.window.width;
        let grid = Rect::new(0, PADDING, width, rows as i32 * self.cell_height);
        let mut changed = Rect::default();
        if self.shown.len() != rows {
            self.shown = vec![None; rows];
        } else if scrolled > 0 && scrolled < rows && self.view == 0 {
            // The whole screen moved up: move the pixels and draw only
            // the new rows (and any that differ anyway).
            let mut canvas = self.window.canvas();
            canvas.scroll(grid, scrolled as i32 * self.cell_height);
            self.shown.drain(..scrolled);
            self.shown.resize(rows, None);
            changed = grid;
        }
        let selection = self.selected_range();
        let cursor_shown = self.view == 0 && self.screen.cursor_visible;
        for row in 0..rows {
            let (number, _) = self.position(0, row);
            let Some(line) = self.line(number) else { continue };
            let mut cells = line.cells.clone();
            cells.resize(columns, Cell::BLANK);
            let selected = selection.and_then(|(start, end)| {
                if number < start.0 || number > end.0 {
                    return None;
                }
                let from = if number == start.0 { start.1 } else { 0 };
                let to = if number == end.0 { end.1 + 1 } else { columns };
                Some((from, to.min(columns)))
            });
            let cursor = (cursor_shown && row == self.screen.row).then_some((
                self.screen.column,
                self.focused,
                self.screen.cursor_shape,
            ));
            let wanted = Row { cells, cursor, selected, reverse: self.screen.reverse_video };
            if self.shown[row].as_ref() != Some(&wanted) {
                let rect = self.draw_row(row, &wanted);
                changed = changed.union(&rect);
                self.shown[row] = Some(wanted);
            }
        }
        // Looking back: a thumb in the right margin says how far.
        let scrollback = self.screen.scrollback.len();
        let bar = (self.view, scrollback);
        if (self.view > 0 && (bar != self.scrollbar || !changed.is_empty())) || (self.view == 0 && self.scrollbar.0 > 0)
        {
            let track = Rect::new(width - PADDING, PADDING, PADDING - 1, rows as i32 * self.cell_height);
            let mut canvas = self.window.canvas();
            canvas.fill(track, BACKGROUND);
            if self.view > 0 {
                let thumb = ui::widgets::scrollbar_thumb(track, scrollback + rows, rows, scrollback - self.view);
                canvas.fill(track, mix(BACKGROUND, FOREGROUND, 30));
                canvas.fill(thumb, mix(BACKGROUND, FOREGROUND, 160));
            }
            self.scrollbar = bar;
            changed = changed.union(&track);
        }
        if !changed.is_empty() {
            self.window.present(changed);
        }
    }

    /// Draws one row of cells; returns the part of the window it covers.
    fn draw_row(&mut self, row: usize, wanted: &Row) -> Rect {
        let (cell_width, cell_height) = (self.cell_width, self.cell_height);
        let y = PADDING + row as i32 * cell_height;
        let width = self.window.width;
        let rect = Rect::new(0, y, width, cell_height);
        let font = &mut self.font;
        let mut canvas = self.window.canvas();
        let old_clip = canvas.set_clip(rect);
        let default_background = if wanted.reverse { FOREGROUND } else { BACKGROUND };
        canvas.fill(rect, default_background);
        let colors: Vec<(u32, u32)> = wanted
            .cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let selected = wanted.selected.is_some_and(|(from, to)| (from..to).contains(&column));
                cell_colors(cell, selected, wanted.reverse)
            })
            .collect();
        // Backgrounds first, so glyphs that reach into the next cell
        // aren't painted over.
        for (column, &(_, background)) in colors.iter().enumerate() {
            if background != default_background {
                canvas.fill(Rect::new(PADDING + column as i32 * cell_width, y, cell_width, cell_height), background);
            }
        }
        for (column, cell) in wanted.cells.iter().enumerate() {
            let (foreground, background) = colors[column];
            let x = PADDING + column as i32 * cell_width;
            draw_cell(&mut canvas, font, cell, Rect::new(x, y, cell_width, cell_height), foreground, background);
        }
        if let Some((column, focused, shape)) = wanted.cursor
            && column < wanted.cells.len()
        {
            let cell = Rect::new(PADDING + column as i32 * cell_width, y, cell_width, cell_height);
            match (focused, shape) {
                (false, _) => canvas.border(cell, 1, CURSOR),
                (true, 3 | 4) => canvas.fill(Rect::new(cell.x, cell.bottom() - 2, cell.width, 2), CURSOR),
                (true, 5 | 6) => canvas.fill(Rect::new(cell.x, cell.y, 2, cell.height), CURSOR),
                (true, _) => {
                    canvas.fill(cell, CURSOR);
                    let (_, background) = colors[column];
                    draw_cell(&mut canvas, font, &wanted.cells[column], cell, background, CURSOR);
                }
            }
        }
        canvas.set_clip(old_clip);
        rect
    }
}

/// A cell's text and background colors.
fn cell_colors(cell: &Cell, selected: bool, reverse_video: bool) -> (u32, u32) {
    let style = cell.style;
    let mut foreground = match style.foreground {
        Color::Default => FOREGROUND,
        // Bold makes the first 8 colors bright, as most terminals do.
        Color::Index(index) if index < 8 && style.flags & BOLD != 0 => palette(index + 8),
        Color::Index(index) => palette(index),
        Color::Rgb(rgb) => 0xFF00_0000 | rgb,
    };
    let mut background = match style.background {
        Color::Default => BACKGROUND,
        Color::Index(index) => palette(index),
        Color::Rgb(rgb) => 0xFF00_0000 | rgb,
    };
    if (style.flags & INVERSE != 0) != reverse_video {
        core::mem::swap(&mut foreground, &mut background);
    }
    if selected {
        background = SELECTION;
        if foreground == SELECTION {
            foreground = FOREGROUND;
        }
    }
    if style.flags & DIM != 0 {
        foreground = mix(background, foreground, 150);
    }
    if style.flags & HIDDEN != 0 {
        foreground = background;
    }
    (foreground, background)
}

fn draw_cell(canvas: &mut Canvas, font: &mut Font, cell: &Cell, rect: Rect, foreground: u32, background: u32) {
    let flags = cell.style.flags;
    if cell.c != ' ' && !boxes::draw(canvas, cell.c, rect, foreground, background) {
        canvas.glyph(font, rect.x, rect.y, cell.c, foreground);
        if flags & BOLD != 0 {
            // No bold font: the same glyph again, a pixel over.
            canvas.glyph(font, rect.x + 1, rect.y, cell.c, foreground);
        }
    }
    if flags & UNDERLINE != 0 {
        canvas.fill(Rect::new(rect.x, rect.y + font.ascent + 2, rect.width, 1), foreground);
    }
    if flags & STRIKE != 0 {
        canvas.fill(Rect::new(rect.x, rect.y + rect.height / 2, rect.width, 1), foreground);
    }
}

/// A cell's size for `font`: its advance (it's monospaced) by its line
/// height.
fn cell_size(font: &mut Font) -> (i32, i32) {
    let width = (font.advance('M') + 0.5) as i32;
    (width.max(1), font.line_height.max(1))
}

fn grid_size(width: i32, height: i32, cell_width: i32, cell_height: i32) -> (usize, usize) {
    let columns = (width - 2 * PADDING) / cell_width;
    let rows = (height - 2 * PADDING) / cell_height;
    (columns.max(2) as usize, rows.max(1) as usize)
}

/// What a key sends to the program, as xterm encodes it.
fn encode_key(key: Key, modifiers: Modifiers, application_cursor: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    // xterm's modifier parameter: 1 + Shift 1 + Alt 2 + Control 4.
    let parameter = 1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.control);
    // Cursor-style keys: ESC [ X (or ESC O X), ESC [ 1 ; m X modified.
    let cursor = |bytes: &mut Vec<u8>, letter: u8| {
        if parameter > 1 {
            bytes.extend_from_slice(alloc::format!("\x1b[1;{parameter}").as_bytes());
        } else if application_cursor {
            bytes.extend_from_slice(b"\x1bO");
        } else {
            bytes.extend_from_slice(b"\x1b[");
        }
        bytes.push(letter);
    };
    // Editing keys: ESC [ n ~, ESC [ n ; m ~ modified.
    let tilde = |bytes: &mut Vec<u8>, number: u8| {
        let text =
            if parameter > 1 { alloc::format!("\x1b[{number};{parameter}~") } else { alloc::format!("\x1b[{number}~") };
        bytes.extend_from_slice(text.as_bytes());
    };
    match key {
        Key::Char(c) => {
            if modifiers.alt {
                bytes.push(0x1B);
            }
            if modifiers.control
                && let Some(control) = control_character(c)
            {
                bytes.push(control);
            } else {
                let mut utf8 = [0; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
        Key::Enter => {
            if modifiers.alt {
                bytes.push(0x1B);
            }
            bytes.push(b'\r');
        }
        Key::Backspace => {
            if modifiers.alt {
                bytes.push(0x1B);
            }
            bytes.push(if modifiers.control { 0x08 } else { 0x7F });
        }
        Key::Tab if modifiers.shift => bytes.extend_from_slice(b"\x1b[Z"),
        Key::Tab => bytes.push(b'\t'),
        Key::Escape => bytes.push(0x1B),
        Key::Up => cursor(&mut bytes, b'A'),
        Key::Down => cursor(&mut bytes, b'B'),
        Key::Right => cursor(&mut bytes, b'C'),
        Key::Left => cursor(&mut bytes, b'D'),
        Key::Home => cursor(&mut bytes, b'H'),
        Key::End => cursor(&mut bytes, b'F'),
        Key::Insert => tilde(&mut bytes, 2),
        Key::Delete => tilde(&mut bytes, 3),
        Key::PageUp => tilde(&mut bytes, 5),
        Key::PageDown => tilde(&mut bytes, 6),
        Key::Function(n @ 1..=4) => {
            if parameter > 1 {
                bytes.extend_from_slice(alloc::format!("\x1b[1;{parameter}").as_bytes());
            } else {
                bytes.extend_from_slice(b"\x1bO");
            }
            bytes.push(b'P' + n - 1);
        }
        Key::Function(n) => {
            let number = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde(&mut bytes, number);
        }
        Key::Other(_) => {}
    }
    bytes
}

/// The control character Ctrl+`c` types, if any.
fn control_character(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1B,
        '\\' | '4' => 0x1C,
        ']' | '5' => 0x1D,
        '^' | '6' | '~' => 0x1E,
        '_' | '/' | '7' => 0x1F,
        '?' | '8' => 0x7F,
        _ => return None,
    })
}
