//! files: eliOS's file explorer.
//!
//! `files [folder]` opens a window on a folder (the home folder unless
//! told otherwise): a toolbar with back, forward and up buttons and the
//! path (which can be typed into), a sidebar of places (home, the root,
//! every mounted disk), the folder's contents with their sizes and dates
//! (folders first; click a column heading to sort by it), and a status
//! line.
//!
//! Double-clicking (or Enter) opens: a folder in place, a program in a
//! terminal, a text file in `edit` in a terminal. A right click opens a
//! menu of everything else. Keys:
//!
//! - Backspace or Alt+Up: the parent folder; Alt+Left and Alt+Right (and
//!   the mouse's back and forward buttons): back and forward.
//! - Ctrl+L: type a path (`~` is home). Typing a name's first letters
//!   selects it.
//! - F2 renames, Delete deletes (after asking), Ctrl+Shift+N makes a
//!   folder; Ctrl+C, Ctrl+X and Ctrl+V copy, cut and paste.
//! - Ctrl+H shows hidden files, F5 reloads, Ctrl+T opens a terminal here,
//!   Ctrl+W closes.
//!
//! The folder is read again whenever the window gets the keyboard back, so
//! changes made elsewhere (in a terminal, say) show up.

#![no_std]
#![no_main]

extern crate alloc;

mod folder;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::FileType;
use folder::{Column, Entry, Order, Place};
use ui::widgets::{self, ButtonState, FieldChange, List, ListChange, Menu, MenuChoice, MenuItem, TextField, theme};
use ui::{Button, Canvas, Event, Font, Key, Modifiers, Rect, Window};

eli::main!(main);

const FONT_SIZE: f32 = 15.0;
const TOOLBAR_HEIGHT: i32 = 44;
const STATUS_HEIGHT: i32 = 36;
const SIDEBAR_WIDTH: i32 = 190;
const TOOL_WIDTH: i32 = 36;
/// Typing a name's first letters selects it; a pause this long starts over.
const TYPEAHEAD_NS: u64 = 1_000_000_000;
/// evdev's codes for the mouse's back and forward buttons.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// The toolbar's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Back,
    Forward,
    Up,
}

const TOOLS: [(Tool, &str); 3] = [(Tool::Back, "←"), (Tool::Forward, "→"), (Tool::Up, "↑")];

/// Where the keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Places,
    Path,
    /// The question in the status line.
    Prompt,
}

/// A question asked in the status line.
#[derive(Debug, Clone)]
enum Prompt {
    /// A new name for this entry (the field holds it).
    Rename(String),
    NewFolder,
    /// Delete this entry? (Enter or Escape.)
    Delete(String),
}

/// What a menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Terminal,
    Rename,
    Delete,
    Copy,
    Cut,
    Paste,
    NewFolder,
    ToggleHidden,
    Reload,
}

struct Files {
    font: Font,
    width: i32,
    height: i32,
    home: String,
    /// The folder shown (absolute, without `.` or `..`).
    path: String,
    entries: Vec<Entry>,
    /// How many hidden entries `entries` leaves out.
    hidden: usize,
    order: Order,
    list: List,
    places: Vec<Place>,
    place_list: List,
    path_field: TextField,
    prompt: Option<Prompt>,
    prompt_field: TextField,
    focus: Focus,
    menu: Option<(Menu, Vec<Action>)>,
    back: Vec<String>,
    forward: Vec<String>,
    /// Copied or cut (true) with Ctrl+C or Ctrl+X: a full path.
    clipboard: Option<(String, bool)>,
    /// Said in the status line until the next folder change.
    message: Option<String>,
    /// What's been typed to select by name, and when the last letter came.
    typed: String,
    typed_at: u64,
    hovered: Option<Tool>,
    pressed: Option<Tool>,
    /// Where the pointer is (the wheel scrolls what's under it).
    pointer: (i32, i32),
}

impl Files {
    // Where everything goes.

    fn tool_rect(&self, index: usize) -> Rect {
        Rect::new(8 + index as i32 * (TOOL_WIDTH + 6), 6, TOOL_WIDTH, TOOLBAR_HEIGHT - 12)
    }

    fn path_rect(&self) -> Rect {
        let left = self.tool_rect(TOOLS.len() - 1).right() + 10;
        Rect::new(left, 6, (self.width - left - 8).max(0), TOOLBAR_HEIGHT - 12)
    }

    fn sidebar_rect(&self) -> Rect {
        Rect::new(0, TOOLBAR_HEIGHT, SIDEBAR_WIDTH.min(self.width / 3), self.height - TOOLBAR_HEIGHT - STATUS_HEIGHT)
    }

    fn header_rect(&self) -> Rect {
        let left = self.sidebar_rect().right() + 1;
        Rect::new(left, TOOLBAR_HEIGHT, self.width - left, self.list.row_height)
    }

    fn list_rect(&self) -> Rect {
        let header = self.header_rect();
        Rect::new(header.x, header.bottom(), header.width, self.height - header.bottom() - STATUS_HEIGHT)
    }

    fn status_rect(&self) -> Rect {
        Rect::new(0, self.height - STATUS_HEIGHT, self.width, STATUS_HEIGHT)
    }

    /// The columns shown, as (column, x, width) in a row starting at `x`
    /// and `width` wide; narrow windows drop the date, then the size.
    fn columns(&mut self, x: i32, width: i32) -> Vec<(Column, i32, i32)> {
        // Wide enough for the widest date and size, with padding.
        let modified = self.font.width("0000-00-00 00:00") + 36;
        let size = self.font.width("1023 KB ▲") + 24;
        let mut columns = Vec::new();
        let mut right = x + width;
        if width > modified + size + 200 {
            right -= modified;
            columns.push((Column::Modified, right, modified));
        }
        if width > size + 160 {
            right -= size;
            columns.push((Column::Size, right, size));
        }
        columns.push((Column::Name, x, right - x));
        columns.reverse();
        columns
    }

    fn prompt_field_rect(&mut self) -> Rect {
        let status = self.status_rect();
        let label = self.prompt_label();
        let left = 10 + self.font.width(&label) + 10;
        let hint = self.font.width("Enter: OK   Esc: cancel") + 20;
        Rect::new(left, status.y + 4, (status.width - left - hint).clamp(80, 400), STATUS_HEIGHT - 8)
    }

    fn prompt_label(&self) -> String {
        match &self.prompt {
            Some(Prompt::Rename(name)) => format!("Rename {name} to"),
            Some(Prompt::NewFolder) => String::from("New folder"),
            Some(Prompt::Delete(name)) => {
                let folder = self.entries.iter().any(|e| &e.name == name && e.is_folder());
                if folder {
                    format!("Delete the folder {name} and everything in it?")
                } else {
                    format!("Delete {name}?")
                }
            }
            None => String::new(),
        }
    }

    // Moving around.

    /// Shows the folder at `path`; `remember` puts the folder being left
    /// in the back history.
    fn go(&mut self, path: String, remember: bool) -> bool {
        let (entries, hidden) = match folder::read(&path, self.order) {
            Ok(read) => read,
            Err(error) => {
                self.message = Some(format!("can't open {path}: {error}"));
                return false;
            }
        };
        if remember && path != self.path {
            let old = core::mem::replace(&mut self.path, path);
            self.back.push(old);
            self.forward.clear();
        } else {
            self.path = path;
        }
        self.entries = entries;
        self.hidden = hidden;
        self.list.reset(self.entries.len());
        self.path_field.set_text(&self.path);
        self.message = None;
        self.typed.clear();
        true
    }

    fn go_up(&mut self) {
        if self.path == "/" {
            return;
        }
        let child = folder::base_name(&self.path).to_string();
        let parent = folder::parent(&self.path).to_string();
        if self.go(parent, true) {
            self.select_named(&child);
        }
    }

    fn go_back(&mut self) {
        if let Some(path) = self.back.pop() {
            let left = self.path.clone();
            if self.go(path, false) {
                self.forward.push(left);
            }
        }
    }

    fn go_forward(&mut self) {
        if let Some(path) = self.forward.pop() {
            let left = self.path.clone();
            if self.go(path, false) {
                self.back.push(left);
            }
        }
    }

    /// Reads the folder again, keeping the selection and scroll position.
    fn reload(&mut self) {
        let selected = self.selected().map(|entry| entry.name.clone());
        let top = self.list.top;
        let message = self.message.take();
        if !self.go(self.path.clone(), false) {
            // It's gone (deleted from a terminal, a USB stick pulled out):
            // go to the nearest folder that's still there.
            let mut path = self.path.clone();
            while path != "/" {
                path = folder::parent(&path).to_string();
                if self.go(path.clone(), false) {
                    break;
                }
            }
            return;
        }
        self.message = message;
        self.list.top = top.min(self.entries.len().saturating_sub(1));
        if let Some(name) = selected {
            self.select_named(&name);
        }
    }

    fn select_named(&mut self, name: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.name == name) {
            let rect = self.list_rect();
            self.list.select(index, rect);
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.list.selected.and_then(|index| self.entries.get(index))
    }

    fn selected_path(&self) -> Option<String> {
        self.selected().map(|entry| folder::join(&self.path, &entry.name))
    }

    /// Selects the next entry whose name starts with what's been typed.
    fn type_to_select(&mut self, c: char) {
        let now = eli::time::monotonic_nanos();
        if now.saturating_sub(self.typed_at) > TYPEAHEAD_NS {
            self.typed.clear();
        }
        self.typed_at = now;
        self.typed.extend(c.to_lowercase());
        let len = self.entries.len();
        // A single letter moves on to the next match; more letters refine
        // the current one.
        let start = match self.list.selected {
            Some(index) if self.typed.chars().count() == 1 => index + 1,
            Some(index) => index,
            None => 0,
        };
        let typed = &self.typed;
        let found =
            (0..len).map(|i| (start + i) % len).find(|&i| self.entries[i].name.to_lowercase().starts_with(typed));
        if let Some(index) = found {
            let rect = self.list_rect();
            self.list.select(index, rect);
        }
    }

    // Doing things.

    fn open(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else { return };
        let path = folder::join(&self.path, &entry.name);
        if entry.is_folder() {
            self.go(path, true);
        } else if entry.kind == FileType::File && entry.executable {
            self.spawn(&["term", &path]);
        } else if entry.kind == FileType::File && folder::is_text(&path) {
            self.spawn(&["term", "edit", &path]);
        } else {
            self.message = Some(format!("nothing here opens {}", entry.name));
        }
    }

    /// Starts a program in the folder shown. It runs as a grandchild, so
    /// it's never left as our zombie.
    fn spawn(&mut self, arguments: &[&str]) {
        let Some(program) = eli::process::find_program(arguments[0]) else {
            self.message = Some(format!("{} isn't installed", arguments[0]));
            return;
        };
        let argv: Vec<String> = arguments.iter().map(|argument| argument.to_string()).collect();
        match eli::process::fork() {
            Ok(0) => {
                if eli::process::fork().is_ok_and(|pid| pid == 0) {
                    let _ = eli::process::set_group(0, 0);
                    let _ = eli::fs::set_current_dir(&self.path);
                    let error = eli::process::exec(&program, &argv, &eli::env::vars());
                    eli::eprintln!("files: can't run {program}: {error}");
                    eli::process::exit(127);
                }
                eli::process::exit(0);
            }
            Ok(pid) => {
                let _ = eli::process::wait(pid as i32, false);
            }
            Err(error) => self.message = Some(format!("can't start {program}: {error}")),
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Open => {
                if let Some(index) = self.list.selected {
                    self.open(index);
                }
            }
            Action::Terminal => self.spawn(&["term"]),
            Action::Rename => {
                if let Some(name) = self.selected().map(|entry| entry.name.clone()) {
                    self.prompt_field.set_text(&name);
                    // Select the name without its extension, as people
                    // usually change just that.
                    let stem = match name.rfind('.') {
                        Some(dot) if dot > 0 && !self.selected().is_some_and(Entry::is_folder) => dot,
                        _ => name.len(),
                    };
                    self.prompt_field.select(0, stem);
                    self.ask(Prompt::Rename(name));
                }
            }
            Action::NewFolder => {
                self.prompt_field.set_text(&folder::free_name(&self.path, "New folder"));
                self.prompt_field.select_all();
                self.ask(Prompt::NewFolder);
            }
            Action::Delete => {
                if let Some(name) = self.selected().map(|entry| entry.name.clone()) {
                    self.ask(Prompt::Delete(name));
                }
            }
            Action::Copy | Action::Cut => {
                if let Some(path) = self.selected_path() {
                    let cut = action == Action::Cut;
                    self.message = Some(format!(
                        "{} {}: paste it with Ctrl+V",
                        if cut { "cut" } else { "copied" },
                        folder::base_name(&path)
                    ));
                    self.clipboard = Some((path, cut));
                }
            }
            Action::Paste => self.paste(),
            Action::ToggleHidden => {
                self.order.show_hidden = !self.order.show_hidden;
                self.reload();
            }
            Action::Reload => self.reload(),
        }
    }

    fn ask(&mut self, prompt: Prompt) {
        self.prompt = Some(prompt);
        self.focus = Focus::Prompt;
    }

    fn end_prompt(&mut self) {
        self.prompt = None;
        self.focus = Focus::Files;
    }

    /// Enter in the status line's question.
    fn answer(&mut self) {
        let Some(prompt) = self.prompt.clone() else { return };
        let typed = String::from(self.prompt_field.text().trim());
        let result = match &prompt {
            Prompt::Rename(old) if *old == typed => Ok(()),
            Prompt::Rename(_) | Prompt::NewFolder => {
                if let Err(problem) = folder::check_name(&typed) {
                    self.message = Some(String::from(problem));
                    return;
                }
                let path = folder::join(&self.path, &typed);
                if eli::fs::symlink_metadata(&path).is_ok() {
                    self.message = Some(format!("there's already something called {typed}"));
                    return;
                }
                match &prompt {
                    Prompt::Rename(old) => eli::fs::rename(&folder::join(&self.path, old), &path)
                        .map_err(|error| format!("can't rename {old}: {error}")),
                    _ => eli::fs::create_dir(&path).map_err(|error| format!("can't make {typed}: {error}")),
                }
            }
            Prompt::Delete(name) => eli::fs::remove_tree(&folder::join(&self.path, name))
                .map_err(|error| format!("can't delete {name}: {error}")),
        };
        self.end_prompt();
        self.reload();
        match result {
            Ok(()) => {
                self.message = None;
                if !matches!(prompt, Prompt::Delete(_)) {
                    self.select_named(&typed);
                }
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn paste(&mut self) {
        let Some((source, cut)) = self.clipboard.clone() else {
            self.message = Some(String::from("nothing to paste: copy something with Ctrl+C first"));
            return;
        };
        let result = folder::paste(&source, &self.path, cut);
        self.reload();
        match result {
            Ok(name) => {
                if cut {
                    self.clipboard = None;
                }
                self.select_named(&name);
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn open_menu(&mut self, x: i32, y: i32) {
        let mut items = Vec::new();
        let mut actions = Vec::new();
        let mut add = |item: MenuItem, action: Action| {
            items.push(item);
            actions.push(action);
        };
        if self.selected().is_some() {
            add(MenuItem::new("Open"), Action::Open);
            add(MenuItem::new("Rename"), Action::Rename);
            add(MenuItem::new("Copy").after_separator(), Action::Copy);
            add(MenuItem::new("Cut"), Action::Cut);
            add(MenuItem::new("Delete").after_separator(), Action::Delete);
        } else {
            let paste = MenuItem::new("Paste");
            add(if self.clipboard.is_some() { paste } else { paste.disabled() }, Action::Paste);
            add(MenuItem::new("New folder"), Action::NewFolder);
            add(MenuItem::new("Open a terminal here").after_separator(), Action::Terminal);
            let hidden = if self.order.show_hidden { "Hide hidden files" } else { "Show hidden files" };
            add(MenuItem::new(hidden), Action::ToggleHidden);
            add(MenuItem::new("Reload"), Action::Reload);
        }
        let bounds = Rect::new(0, 0, self.width, self.height);
        self.menu = Some((Menu::new(items, &mut self.font, x, y, bounds), actions));
    }

    fn sort_by(&mut self, column: Column) {
        if self.order.column == column {
            self.order.descending = !self.order.descending;
        } else {
            self.order.column = column;
            self.order.descending = false;
        }
        let selected = self.selected().map(|entry| entry.name.clone());
        folder::sort(&mut self.entries, self.order);
        if let Some(name) = selected {
            self.select_named(&name);
        }
    }

    // Input.

    /// Handles a key; returns whether the window should close.
    fn key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        if let Some((menu, actions)) = &mut self.menu {
            match menu.key(key) {
                MenuChoice::Chosen(index) => {
                    let action = actions[index];
                    self.menu = None;
                    self.act(action);
                }
                MenuChoice::Dismissed => self.menu = None,
                MenuChoice::Open => {}
            }
            return false;
        }
        let control = modifiers.control && !modifiers.alt;
        match key {
            Key::Char('w' | 'q') if control => return true,
            Key::Char('l') if control => {
                self.focus = Focus::Path;
                self.path_field.select_all();
                return false;
            }
            Key::Char('h') if control => {
                self.act(Action::ToggleHidden);
                return false;
            }
            Key::Char('t') if control => {
                self.act(Action::Terminal);
                return false;
            }
            Key::Char('r') if control => {
                self.reload();
                return false;
            }
            Key::Function(5) => {
                self.reload();
                return false;
            }
            Key::Left if modifiers.alt => {
                self.go_back();
                return false;
            }
            Key::Right if modifiers.alt => {
                self.go_forward();
                return false;
            }
            Key::Up if modifiers.alt => {
                self.go_up();
                return false;
            }
            _ => {}
        }
        match self.focus {
            Focus::Prompt => {
                if matches!(self.prompt, Some(Prompt::Delete(_))) {
                    match key {
                        Key::Enter | Key::Char('y') => self.answer(),
                        Key::Escape | Key::Char('n') => self.end_prompt(),
                        _ => {}
                    }
                    return false;
                }
                match self.prompt_field.key(key, modifiers) {
                    FieldChange::Submit => self.answer(),
                    FieldChange::Cancel => {
                        self.message = None;
                        self.end_prompt();
                    }
                    _ => {}
                }
            }
            Focus::Path => match self.path_field.key(key, modifiers) {
                FieldChange::Submit => {
                    let path = folder::resolve(self.path_field.text(), &self.path, &self.home);
                    if self.go(path, true) {
                        self.focus = Focus::Files;
                    }
                }
                FieldChange::Cancel => {
                    self.path_field.set_text(&self.path);
                    self.focus = Focus::Files;
                }
                _ => {
                    if key == Key::Tab {
                        self.focus = Focus::Files;
                    }
                }
            },
            Focus::Places => {
                let rect = self.sidebar_rect();
                match (key, self.place_list.key(key, rect)) {
                    (Key::Tab, _) => self.focus = Focus::Files,
                    (_, ListChange::Activated(index)) => {
                        let path = self.places[index].path.clone();
                        if self.go(path, true) {
                            self.focus = Focus::Files;
                        }
                    }
                    _ => {}
                }
            }
            Focus::Files => {
                let rect = self.list_rect();
                match key {
                    Key::Tab => self.focus = Focus::Places,
                    Key::Backspace => self.go_up(),
                    Key::Function(2) => self.act(Action::Rename),
                    Key::Delete => self.act(Action::Delete),
                    Key::Char('N') if control && modifiers.shift => self.act(Action::NewFolder),
                    Key::Char('c') if control => self.act(Action::Copy),
                    Key::Char('x') if control => self.act(Action::Cut),
                    Key::Char('v') if control => self.act(Action::Paste),
                    Key::Char(c) if !control && !modifiers.alt && !c.is_control() => self.type_to_select(c),
                    Key::Escape => self.list.selected = None,
                    _ => {
                        if let ListChange::Activated(index) = self.list.key(key, rect) {
                            self.open(index);
                        }
                    }
                }
            }
        }
        false
    }

    fn press(&mut self, button: Button, x: i32, y: i32, clicks: u32) {
        if let Some((menu, _)) = &mut self.menu {
            // A press outside dismisses (a right click then opens a new
            // menu where it is); choosing happens on release.
            if menu.click(&self.font, x, y) != MenuChoice::Dismissed {
                return;
            }
            self.menu = None;
            if button != Button::Right {
                return;
            }
        }
        match button {
            Button::Other(BTN_SIDE) => return self.go_back(),
            Button::Other(BTN_EXTRA) => return self.go_forward(),
            Button::Left | Button::Right => {}
            _ => return,
        }
        if self.prompt.is_some() {
            let field = self.prompt_field_rect();
            if field.contains(x, y) {
                self.focus = Focus::Prompt;
                self.prompt_field.click(&mut self.font, field, x);
                return;
            }
            // Clicking elsewhere gives up on the question.
            self.message = None;
            self.end_prompt();
        }
        let list = self.list_rect();
        if let Some(index) = (0..TOOLS.len()).find(|&index| self.tool_rect(index).contains(x, y)) {
            self.pressed = Some(TOOLS[index].0);
        } else if self.path_rect().contains(x, y) {
            let rect = self.path_rect();
            if self.focus != Focus::Path {
                self.focus = Focus::Path;
                self.path_field.select_all();
            } else {
                self.path_field.click(&mut self.font, rect, x);
            }
        } else if self.sidebar_rect().contains(x, y) {
            let rect = self.sidebar_rect();
            self.focus = Focus::Places;
            if self.place_list.press(rect, x, y, 1) != ListChange::Ignored
                && let Some(index) = self.place_list.row_at(rect, x, y)
            {
                let path = self.places[index].path.clone();
                if self.go(path, true) {
                    self.focus = Focus::Files;
                }
            }
        } else if self.header_rect().contains(x, y) && button == Button::Left {
            let header = self.header_rect();
            let columns = self.columns(header.x, self.list.rows_rect(list).width);
            if let Some(&(column, _, _)) = columns.iter().find(|&&(_, left, width)| x >= left && x < left + width) {
                self.sort_by(column);
            }
        } else if list.contains(x, y) {
            self.focus = Focus::Files;
            if self.path_field.text() != self.path {
                self.path_field.set_text(&self.path);
            }
            if button == Button::Right {
                // A right click selects what it's on (or nothing) first.
                self.list.selected = self.list.row_at(list, x, y);
                self.open_menu(x, y);
            } else if let ListChange::Activated(index) = self.list.press(list, x, y, clicks) {
                self.open(index);
            }
        }
    }

    fn release(&mut self, x: i32, y: i32) {
        if let Some((menu, actions)) = &mut self.menu {
            if let MenuChoice::Chosen(index) = menu.click(&self.font, x, y) {
                let action = actions[index];
                self.menu = None;
                self.act(action);
            }
            return;
        }
        self.list.release();
        self.place_list.release();
        if let Some(tool) = self.pressed.take()
            && let Some(index) = TOOLS.iter().position(|&(t, _)| t == tool)
            && self.tool_rect(index).contains(x, y)
        {
            match tool {
                Tool::Back => self.go_back(),
                Tool::Forward => self.go_forward(),
                Tool::Up => self.go_up(),
            }
        }
    }

    /// The pointer moved; returns whether anything needs drawing.
    fn moved(&mut self, x: i32, y: i32) -> bool {
        self.pointer = (x, y);
        if let Some((menu, _)) = &mut self.menu {
            return menu.hover(&self.font, x, y);
        }
        let hovered = (0..TOOLS.len()).find(|&index| self.tool_rect(index).contains(x, y)).map(|index| TOOLS[index].0);
        let changed = hovered != self.hovered;
        self.hovered = hovered;
        let (list, sidebar) = (self.list_rect(), self.sidebar_rect());
        let dragged = self.list.drag(list, y) | self.place_list.drag(sidebar, y);
        changed || dragged
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let (x, y) = self.pointer;
        let sidebar = self.sidebar_rect();
        if sidebar.contains(x, y) {
            self.place_list.scroll(lines, sidebar)
        } else {
            let list = self.list_rect();
            self.list.scroll(lines * 3, list)
        }
    }

    // Drawing.

    fn draw(&mut self, canvas: &mut Canvas) {
        canvas.fill(canvas.bounds(), theme::BACKGROUND);
        self.draw_toolbar(canvas);
        self.draw_sidebar(canvas);
        self.draw_header(canvas);
        self.draw_entries(canvas);
        self.draw_status(canvas);
        if let Some((menu, _)) = &self.menu {
            menu.draw(canvas, &mut self.font);
        }
    }

    fn draw_toolbar(&mut self, canvas: &mut Canvas) {
        canvas.fill(Rect::new(0, 0, self.width, TOOLBAR_HEIGHT), theme::PANEL);
        canvas.fill(Rect::new(0, TOOLBAR_HEIGHT - 1, self.width, 1), theme::BORDER);
        for (index, &(tool, label)) in TOOLS.iter().enumerate() {
            let usable = match tool {
                Tool::Back => !self.back.is_empty(),
                Tool::Forward => !self.forward.is_empty(),
                Tool::Up => self.path != "/",
            };
            let rect = self.tool_rect(index);
            let state = ButtonState {
                hovered: usable && self.hovered == Some(tool),
                pressed: usable && self.pressed == Some(tool),
                default: false,
            };
            widgets::button(canvas, &mut self.font, rect, if usable { label } else { "" }, state);
            if !usable {
                // A dimmed arrow: the button does nothing now.
                let x = rect.x + (rect.width - self.font.width(label)) / 2;
                let y = rect.y + (rect.height - self.font.line_height) / 2;
                canvas.text(&mut self.font, x, y, label, theme::BORDER);
            }
        }
        let rect = self.path_rect();
        self.path_field.draw(canvas, &mut self.font, rect, self.focus == Focus::Path);
    }

    fn draw_sidebar(&mut self, canvas: &mut Canvas) {
        let rect = self.sidebar_rect();
        // The place being looked at is highlighted (the deepest one that
        // holds it, so a folder on a USB stick highlights the stick).
        let current = self
            .places
            .iter()
            .enumerate()
            .filter(|(_, place)| folder::is_within(&self.path, &place.path))
            .max_by_key(|(_, place)| place.path.len())
            .map(|(index, _)| index);
        self.place_list.selected = current;
        let font = &mut self.font;
        let places = &self.places;
        canvas.fill(rect, theme::PANEL);
        let old = canvas.set_clip(rect.intersect(&canvas.clip()));
        self.place_list.draw(canvas, rect, self.focus == Focus::Places, |canvas, index, row, _| {
            let place = &places[index];
            let size = row.height - 12;
            widgets::icon(canvas, place.icon, Rect::new(row.x + 12, row.y + 6, size, size));
            let x = row.x + 12 + size + 10;
            let label = font.trim(&place.label, row.right() - x - 6);
            canvas.text(font, x, row.y + (row.height - font.line_height) / 2, &label, theme::TEXT);
        });
        canvas.set_clip(old);
        canvas.fill(Rect::new(rect.right(), rect.y, 1, rect.height), theme::BORDER);
    }

    fn draw_header(&mut self, canvas: &mut Canvas) {
        let header = self.header_rect();
        canvas.fill(header, theme::PANEL);
        canvas.fill(Rect::new(header.x, header.bottom() - 1, header.width, 1), theme::BORDER);
        let rows = self.list.rows_rect(self.list_rect());
        let y = header.y + (header.height - self.font.line_height) / 2;
        for (column, x, width) in self.columns(header.x, rows.width) {
            let mut label = String::from(match column {
                Column::Name => "Name",
                Column::Size => "Size",
                Column::Modified => "Modified",
            });
            if column == self.order.column {
                label.push_str(if self.order.descending { " ▼" } else { " ▲" });
            }
            let text_x = if column == Column::Size { x + width - 12 - self.font.width(&label) } else { x + 12 };
            canvas.text(&mut self.font, text_x, y, &label, theme::DIM_TEXT);
        }
    }

    fn draw_entries(&mut self, canvas: &mut Canvas) {
        let rect = self.list_rect();
        let columns = self.columns(rect.x, self.list.rows_rect(rect).width);
        let font = &mut self.font;
        let entries = &self.entries;
        let cut = self.clipboard.as_ref().filter(|(_, cut)| *cut).map(|(path, _)| path.as_str());
        let here = &self.path;
        self.list.draw(canvas, rect, self.focus == Focus::Files, |canvas, index, row, _| {
            let entry = &entries[index];
            // Hidden files, and what's been cut, are dimmed.
            let dim = entry.hidden() || cut.is_some_and(|path| folder::join(here, &entry.name) == path);
            let color = if dim { theme::DIM_TEXT } else { theme::TEXT };
            let y = row.y + (row.height - font.line_height) / 2;
            for &(column, x, width) in &columns {
                match column {
                    Column::Name => {
                        let size = row.height - 10;
                        widgets::icon(canvas, entry.icon(), Rect::new(x + 10, row.y + 5, size, size));
                        let text_x = x + 10 + size + 10;
                        let mut name = entry.name.clone();
                        if entry.link {
                            name.push_str(" →");
                        }
                        let shown = font.trim(&name, x + width - text_x - 8);
                        canvas.text(font, text_x, y, &shown, color);
                    }
                    Column::Size => {
                        if entry.kind == FileType::File {
                            let text = folder::human_size(entry.size);
                            let text_x = x + width - 12 - font.width(&text);
                            canvas.text(font, text_x, y, &text, theme::DIM_TEXT);
                        }
                    }
                    Column::Modified => {
                        canvas.text(font, x + 12, y, &folder::date(entry.modified), theme::DIM_TEXT);
                    }
                }
            }
        });
        if self.entries.is_empty() {
            let text = if self.hidden > 0 { "Only hidden files (Ctrl+H shows them)" } else { "This folder is empty" };
            let x = rect.x + (rect.width - self.font.width(text)) / 2;
            canvas.text(&mut self.font, x, rect.y + 40, text, theme::DIM_TEXT);
        }
    }

    fn draw_status(&mut self, canvas: &mut Canvas) {
        let status = self.status_rect();
        canvas.fill(status, theme::PANEL);
        canvas.fill(Rect::new(0, status.y, status.width, 1), theme::BORDER);
        let y = status.y + (status.height - self.font.line_height) / 2;
        if self.prompt.is_some() {
            let label = self.prompt_label();
            canvas.text(&mut self.font, 10, y, &label, theme::TEXT);
            let hint = if matches!(self.prompt, Some(Prompt::Delete(_))) {
                "Enter: delete   Esc: keep it"
            } else {
                let field = self.prompt_field_rect();
                self.prompt_field.draw(canvas, &mut self.font, field, self.focus == Focus::Prompt);
                "Enter: OK   Esc: cancel"
            };
            let x = status.right() - 10 - self.font.width(hint);
            canvas.text(&mut self.font, x, y, hint, theme::DIM_TEXT);
            if let Some(message) = self.message.clone() {
                // A problem with the answer (a name that's taken, say).
                let field = self.prompt_field_rect();
                let shown = self.font.trim(&message, x - field.right() - 30);
                canvas.text(&mut self.font, field.right() + 12, y, &shown, theme::DANGER);
            }
            return;
        }
        let (text, color) = match (&self.message, self.selected()) {
            (Some(message), _) => (message.clone(), theme::TEXT),
            (None, Some(entry)) if entry.kind == FileType::File => {
                (format!("{}: {}", entry.name, folder::human_size(entry.size)), theme::DIM_TEXT)
            }
            _ => {
                let mut text = match self.entries.len() {
                    1 => String::from("1 item"),
                    count => format!("{count} items"),
                };
                if self.hidden > 0 {
                    text.push_str(&format!(" ({} hidden)", self.hidden));
                }
                (text, theme::DIM_TEXT)
            }
        };
        let free = match eli::fs::space(&self.path) {
            Ok((block, total, free)) if total > 0 => format!("{} free", folder::human_size(block * free)),
            _ => String::new(),
        };
        let free_x = status.right() - 10 - self.font.width(&free);
        canvas.text(&mut self.font, free_x, y, &free, theme::DIM_TEXT);
        let shown = self.font.trim(&text, free_x - 30);
        canvas.text(&mut self.font, 10, y, &shown, color);
    }
}

fn home_folder() -> String {
    eli::env::var("HOME")
        .or_else(|| eli::users::user_with_id(eli::users::uid()).map(|user| user.home))
        .unwrap_or_else(|| String::from("/"))
}

fn main() -> i32 {
    let font = match Font::load(FONT_SIZE) {
        Ok(font) => font,
        Err(error) => {
            eli::eprintln!("files: {error}");
            return 1;
        }
    };
    let home = home_folder();
    let start = match eli::env::args().get(1) {
        Some(path) => folder::resolve(path, &eli::fs::current_dir().unwrap_or_else(|_| String::from("/")), &home),
        None => home.clone(),
    };
    let mut window = match Window::open("Files", "files", 900, 600) {
        Ok(window) => window,
        Err(error) => {
            eli::eprintln!("files: {error}");
            return 1;
        }
    };
    let row_height = font.line_height + 12;
    let mut files = Files {
        font,
        width: window.width,
        height: window.height,
        places: folder::places(&home),
        home,
        path: String::new(),
        entries: Vec::new(),
        hidden: 0,
        order: Order { column: Column::Name, descending: false, show_hidden: false },
        list: List::new(row_height),
        place_list: List::new(row_height + 4),
        path_field: TextField::new(""),
        prompt: None,
        prompt_field: TextField::new(""),
        focus: Focus::Files,
        menu: None,
        back: Vec::new(),
        forward: Vec::new(),
        clipboard: None,
        message: None,
        typed: String::new(),
        typed_at: 0,
        hovered: None,
        pressed: None,
        pointer: (0, 0),
    };
    files.place_list.len = files.places.len();
    if !files.go(start.clone(), false) {
        let message = files.message.take();
        files.go(String::from("/"), false);
        files.message = message;
    }
    let mut title = String::new();
    loop {
        let mut redraw = false;
        for event in window.wait(&[], -1) {
            files.width = window.width;
            files.height = window.height;
            redraw = true;
            match event {
                Event::Close => return 0,
                Event::Resize { .. } => {}
                Event::Focus(focused) => {
                    if focused && files.prompt.is_none() {
                        // Something may have changed while we were away.
                        files.places = folder::places(&files.home);
                        files.place_list.len = files.places.len();
                        files.reload();
                    }
                }
                Event::Key { key, modifiers } => {
                    if files.key(key, modifiers) {
                        return 0;
                    }
                }
                Event::PointerMove { x, y } => redraw = files.moved(x, y),
                Event::PointerLeave => files.hovered = None,
                Event::Button { button, pressed: true, x, y, clicks } => files.press(button, x, y, clicks),
                Event::Button { pressed: false, x, y, .. } => files.release(x, y),
                Event::Scroll { lines } => redraw = files.scroll(lines),
                Event::Readable(_) => redraw = false,
            }
        }
        let wanted = format!("{} — Files", folder::base_name(&files.path));
        if wanted != title {
            window.set_title(&wanted);
            title = wanted;
        }
        if redraw {
            let mut canvas = window.canvas();
            files.draw(&mut canvas);
            window.present_all();
        }
    }
}
