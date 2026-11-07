//! Widgets: buttons, text fields, lists with scroll bars, menus, icons.
//!
//! They're deliberately plain. A widget holds only its own state (a text
//! field's text and cursor, a list's selection and scroll position); the
//! program decides where each one goes, passes it the events meant for
//! it, and draws it when it changes. Everything is drawn in the desktop's
//! colors, from [`theme`].

use alloc::string::String;
use alloc::vec::Vec;

use crate::draw::{Canvas, Font, Path, Rect, mix};
use crate::keys::{Key, Modifiers};

/// The desktop's colors.
pub mod theme {
    pub const BACKGROUND: u32 = 0xFF10_1630;
    /// Panels and sidebars: a little lighter.
    pub const PANEL: u32 = 0xFF16_1E3A;
    pub const BORDER: u32 = 0xFF2A_3466;
    pub const TEXT: u32 = 0xFFD8_DAE5;
    pub const DIM_TEXT: u32 = 0xFF8D_93B0;
    pub const BUTTON: u32 = 0xFF1E_2750;
    pub const BUTTON_HOVER: u32 = 0xFF28_3466;
    pub const ACCENT: u32 = 0xFF3A_4D9A;
    pub const SELECTION: u32 = 0xFF2F_3F80;
    pub const FIELD: u32 = 0xFF0B_1026;
    pub const FOLDER: u32 = 0xFFE0_B050;
    pub const FILE: u32 = 0xFFB8_C0D8;
    pub const DANGER: u32 = 0xFFC0_4050;
}

/// How a button looks right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ButtonState {
    pub hovered: bool,
    pub pressed: bool,
    /// The one Enter presses (drawn in the accent color).
    pub default: bool,
}

/// Draws a button with `label` centered in it.
pub fn button(canvas: &mut Canvas, font: &mut Font, rect: Rect, label: &str, state: ButtonState) {
    let mut color = if state.default { theme::ACCENT } else { theme::BUTTON };
    if state.pressed {
        color = mix(color, 0xFF00_0000, 60);
    } else if state.hovered {
        color = mix(color, 0xFFFF_FFFF, 24);
    }
    canvas.rounded(rect, 4.0, color);
    let label = font.trim(label, rect.width - 8);
    let x = rect.x + (rect.width - font.width(&label)) / 2;
    let y = rect.y + (rect.height - font.line_height) / 2;
    let old = canvas.set_clip(rect.intersect(&canvas.clip()));
    canvas.text(font, x, y, &label, theme::TEXT);
    canvas.set_clip(old);
}

/// The width a button needs for `label`.
pub fn button_width(font: &mut Font, label: &str) -> i32 {
    font.width(label) + 24
}

/// What a key or click did to a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldChange {
    /// Not for the field.
    Ignored,
    /// The cursor moved (redraw).
    Moved,
    /// The text changed.
    Edited,
    /// Enter.
    Submit,
    /// Escape.
    Cancel,
}

/// A one-line text field.
#[derive(Debug, Clone, Default)]
pub struct TextField {
    text: String,
    /// The cursor, as a byte offset into `text` (on a character boundary).
    cursor: usize,
    /// The other end of the selection, if any.
    anchor: Option<usize>,
    /// How far the text is scrolled left, in pixels, to keep the cursor
    /// in view.
    scroll: i32,
    /// Shown instead of the text, dimmed, while it's empty.
    pub placeholder: String,
}

impl TextField {
    pub fn new(text: &str) -> Self {
        Self { text: String::from(text), cursor: text.len(), ..Self::default() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replaces the text, with the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.text = String::from(text);
        self.cursor = text.len();
        self.anchor = None;
    }

    /// Selects everything (as a field does when it's first focused).
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Selects `range` (byte offsets), say a file's name without its
    /// extension.
    pub fn select(&mut self, start: usize, end: usize) {
        let end = end.min(self.text.len());
        if self.text.is_char_boundary(start) && self.text.is_char_boundary(end) && start <= end {
            self.anchor = Some(start);
            self.cursor = end;
        }
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else { return false };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.anchor = None;
        true
    }

    fn previous(&self, from: usize) -> usize {
        self.text[..from].char_indices().next_back().map_or(0, |(index, _)| index)
    }

    fn next(&self, from: usize) -> usize {
        self.text[from..].chars().next().map_or(from, |c| from + c.len_utf8())
    }

    /// The start of the word before `from` (Ctrl+Left, Ctrl+Backspace).
    fn previous_word(&self, from: usize) -> usize {
        let before = &self.text[..from];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed.rfind(|c: char| !c.is_alphanumeric()).map_or(0, |index| index + 1)
    }

    fn next_word(&self, from: usize) -> usize {
        let after = &self.text[from..];
        let skipped = after.len() - after.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
        let rest = &after[skipped..];
        from + skipped + rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len())
    }

    /// Moves the cursor to `to`, extending the selection with Shift.
    fn move_to(&mut self, to: usize, select: bool) {
        if select {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = to;
    }

    pub fn key(&mut self, key: Key, modifiers: Modifiers) -> FieldChange {
        let shift = modifiers.shift;
        match key {
            Key::Enter => return FieldChange::Submit,
            Key::Escape => return FieldChange::Cancel,
            Key::Char('a') if modifiers.control => self.select_all(),
            Key::Char(_) if modifiers.control || modifiers.alt || modifiers.logo => return FieldChange::Ignored,
            Key::Char(c) => {
                self.delete_selection();
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                return FieldChange::Edited;
            }
            Key::Backspace => {
                if !self.delete_selection() {
                    let start =
                        if modifiers.control { self.previous_word(self.cursor) } else { self.previous(self.cursor) };
                    if start == self.cursor {
                        return FieldChange::Ignored;
                    }
                    self.text.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
                return FieldChange::Edited;
            }
            Key::Delete => {
                if !self.delete_selection() {
                    let end = if modifiers.control { self.next_word(self.cursor) } else { self.next(self.cursor) };
                    if end == self.cursor {
                        return FieldChange::Ignored;
                    }
                    self.text.replace_range(self.cursor..end, "");
                }
                return FieldChange::Edited;
            }
            Key::Left => {
                let to = match self.selection() {
                    Some((start, _)) if !shift => start,
                    _ if modifiers.control => self.previous_word(self.cursor),
                    _ => self.previous(self.cursor),
                };
                self.move_to(to, shift);
            }
            Key::Right => {
                let to = match self.selection() {
                    Some((_, end)) if !shift => end,
                    _ if modifiers.control => self.next_word(self.cursor),
                    _ => self.next(self.cursor),
                };
                self.move_to(to, shift);
            }
            Key::Home => self.move_to(0, shift),
            Key::End => self.move_to(self.text.len(), shift),
            _ => return FieldChange::Ignored,
        }
        FieldChange::Moved
    }

    /// Puts the cursor where `x` (window coordinates) falls in the field
    /// drawn at `rect`.
    pub fn click(&mut self, font: &mut Font, rect: Rect, x: i32) {
        let target = x - (rect.x + FIELD_PADDING) + self.scroll;
        let mut pen = 0.0;
        let mut cursor = self.text.len();
        for (index, c) in self.text.char_indices() {
            let advance = font.advance(c);
            if pen + advance / 2.0 > target as f32 {
                cursor = index;
                break;
            }
            pen += advance;
        }
        self.cursor = cursor;
        self.anchor = None;
    }

    pub fn draw(&mut self, canvas: &mut Canvas, font: &mut Font, rect: Rect, focused: bool) {
        canvas.rounded(rect, 4.0, if focused { theme::ACCENT } else { theme::BORDER });
        canvas.rounded(rect.inset(1), 3.0, theme::FIELD);
        let inner = Rect::new(rect.x + FIELD_PADDING, rect.y, rect.width - 2 * FIELD_PADDING, rect.height);
        let old = canvas.set_clip(inner.intersect(&canvas.clip()));
        let y = rect.y + (rect.height - font.line_height) / 2;
        if self.text.is_empty() && !self.placeholder.is_empty() {
            canvas.text(font, inner.x, y, &self.placeholder.clone(), theme::DIM_TEXT);
        }
        // Keep the cursor in view.
        let cursor_x = font.width(&self.text[..self.cursor]);
        if cursor_x - self.scroll > inner.width - 2 {
            self.scroll = cursor_x - inner.width + 2;
        } else if cursor_x < self.scroll {
            self.scroll = cursor_x;
        }
        let left = inner.x - self.scroll;
        if let Some((start, end)) = self.selection().filter(|_| focused) {
            let x0 = left + font.width(&self.text[..start]);
            let x1 = left + font.width(&self.text[..end]);
            canvas.fill(Rect::new(x0, y, x1 - x0, font.line_height), theme::SELECTION);
        }
        let text = self.text.clone();
        canvas.text(font, left, y, &text, theme::TEXT);
        if focused {
            canvas.fill(Rect::new(left + cursor_x, y, 2, font.line_height), theme::TEXT);
        }
        canvas.set_clip(old);
    }
}

const FIELD_PADDING: i32 = 8;

/// A scrolling list of rows with an optional selection. The program draws
/// each row; the list keeps track of which are in view.
#[derive(Debug, Clone, Default)]
pub struct List {
    pub len: usize,
    pub selected: Option<usize>,
    /// The first row in view.
    pub top: usize,
    pub row_height: i32,
    /// The scroll bar is being dragged: where it was grabbed, relative to
    /// the thumb's top.
    dragging: Option<i32>,
}

/// What a click or key did to a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChange {
    Ignored,
    /// It scrolled or the selection moved (redraw).
    Moved,
    /// A row was opened: double-clicked, or Enter.
    Activated(usize),
}

pub const SCROLLBAR_WIDTH: i32 = 10;

impl List {
    pub fn new(row_height: i32) -> Self {
        Self { row_height: row_height.max(1), ..Self::default() }
    }

    /// Starts over with `len` rows, nothing selected, at the top.
    pub fn reset(&mut self, len: usize) {
        self.len = len;
        self.selected = None;
        self.top = 0;
    }

    /// How many whole rows fit in `rect`.
    pub fn visible(&self, rect: Rect) -> usize {
        (rect.height / self.row_height).max(1) as usize
    }

    fn max_top(&self, rect: Rect) -> usize {
        self.len.saturating_sub(self.visible(rect))
    }

    pub fn scroll(&mut self, lines: i32, rect: Rect) -> bool {
        let top = (self.top as i64 + i64::from(lines)).clamp(0, self.max_top(rect) as i64) as usize;
        let moved = top != self.top;
        self.top = top;
        moved
    }

    /// Selects `index`, scrolling it into view.
    pub fn select(&mut self, index: usize, rect: Rect) {
        if self.len == 0 {
            return;
        }
        let index = index.min(self.len - 1);
        self.selected = Some(index);
        let visible = self.visible(rect);
        if index < self.top {
            self.top = index;
        } else if index >= self.top + visible {
            self.top = index + 1 - visible;
        }
    }

    /// The row at window position `y`, if any.
    pub fn row_at(&self, rect: Rect, x: i32, y: i32) -> Option<usize> {
        if !rect.contains(x, y) || x >= self.rows_rect(rect).right() {
            return None;
        }
        let index = self.top + ((y - rect.y) / self.row_height) as usize;
        (index < self.len).then_some(index)
    }

    /// Where the rows go: `rect` less the scroll bar, if there is one.
    pub fn rows_rect(&self, rect: Rect) -> Rect {
        if self.len > self.visible(rect) {
            Rect::new(rect.x, rect.y, rect.width - SCROLLBAR_WIDTH, rect.height)
        } else {
            rect
        }
    }

    /// The rectangle of row `index` (which may be out of view).
    pub fn row_rect(&self, rect: Rect, index: usize) -> Rect {
        let rows = self.rows_rect(rect);
        let y = rect.y + (index as i32 - self.top as i32) * self.row_height;
        Rect::new(rows.x, y, rows.width, self.row_height)
    }

    pub fn key(&mut self, key: Key, rect: Rect) -> ListChange {
        if self.len == 0 {
            return ListChange::Ignored;
        }
        let page = self.visible(rect).saturating_sub(1).max(1);
        let current = self.selected;
        let to = match (key, current) {
            (Key::Enter, Some(index)) => return ListChange::Activated(index),
            (Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End, None) => 0,
            (Key::Up, Some(index)) => index.saturating_sub(1),
            (Key::Down, Some(index)) => index + 1,
            (Key::PageUp, Some(index)) => index.saturating_sub(page),
            (Key::PageDown, Some(index)) => index + page,
            (Key::Home, _) => 0,
            (Key::End, _) => self.len - 1,
            _ => return ListChange::Ignored,
        };
        self.select(to, rect);
        ListChange::Moved
    }

    /// A mouse button press at (x, y): selects (or opens, on a double
    /// click) a row, or grabs the scroll bar.
    pub fn press(&mut self, rect: Rect, x: i32, y: i32, clicks: u32) -> ListChange {
        if !rect.contains(x, y) {
            return ListChange::Ignored;
        }
        if x >= self.rows_rect(rect).right() {
            let thumb = self.thumb(rect);
            if thumb.contains(x, y) {
                self.dragging = Some(y - thumb.y);
            } else {
                // Page towards the click.
                let page = self.visible(rect) as i32;
                self.scroll(if y < thumb.y { -page } else { page }, rect);
            }
            return ListChange::Moved;
        }
        match self.row_at(rect, x, y) {
            Some(index) if clicks >= 2 && self.selected == Some(index) => ListChange::Activated(index),
            Some(index) => {
                self.selected = Some(index);
                ListChange::Moved
            }
            None => {
                let had = self.selected.take().is_some();
                if had { ListChange::Moved } else { ListChange::Ignored }
            }
        }
    }

    /// The pointer moved: drags the scroll bar if it's held.
    pub fn drag(&mut self, rect: Rect, y: i32) -> bool {
        let Some(grab) = self.dragging else { return false };
        let track = rect.height - self.thumb(rect).height;
        if track <= 0 {
            return false;
        }
        let fraction = (y - grab - rect.y) as f32 / track as f32;
        let top = (fraction * self.max_top(rect) as f32 + 0.5).clamp(0.0, self.max_top(rect) as f32) as usize;
        let moved = top != self.top;
        self.top = top;
        moved
    }

    /// The mouse button went up.
    pub fn release(&mut self) {
        self.dragging = None;
    }

    fn thumb(&self, rect: Rect) -> Rect {
        scrollbar_thumb(
            Rect::new(rect.right() - SCROLLBAR_WIDTH, rect.y, SCROLLBAR_WIDTH, rect.height),
            self.len,
            self.visible(rect),
            self.top,
        )
    }

    /// Draws the list's background, each row in view (with `row`, given
    /// the canvas, the row's index and rectangle, and whether it's
    /// selected; the selection highlight is already drawn), and the
    /// scroll bar.
    pub fn draw(
        &self,
        canvas: &mut Canvas,
        rect: Rect,
        focused: bool,
        mut row: impl FnMut(&mut Canvas, usize, Rect, bool),
    ) {
        canvas.fill(rect, theme::BACKGROUND);
        let rows = self.rows_rect(rect);
        let old = canvas.set_clip(rows.intersect(&canvas.clip()));
        let end = (self.top + self.visible(rect) + 1).min(self.len);
        for index in self.top..end {
            let row_rect = self.row_rect(rect, index);
            let selected = self.selected == Some(index);
            if selected {
                canvas.fill(row_rect, if focused { theme::SELECTION } else { theme::BUTTON });
            }
            row(canvas, index, row_rect, selected);
        }
        canvas.set_clip(old);
        if rows != rect {
            let bar = Rect::new(rows.right(), rect.y, SCROLLBAR_WIDTH, rect.height);
            scrollbar(canvas, bar, self.len, self.visible(rect), self.top);
        }
    }
}

/// The thumb of a scroll bar in `track` showing `visible` of `total`
/// things from `first` on.
pub fn scrollbar_thumb(track: Rect, total: usize, visible: usize, first: usize) -> Rect {
    if total == 0 || visible >= total {
        return track;
    }
    let height = ((track.height as f32 * visible as f32 / total as f32) as i32).max(20).min(track.height);
    let room = track.height - height;
    let max_first = total - visible;
    let y = track.y + (room as f32 * first.min(max_first) as f32 / max_first as f32) as i32;
    Rect::new(track.x, y, track.width, height)
}

pub fn scrollbar(canvas: &mut Canvas, track: Rect, total: usize, visible: usize, first: usize) {
    canvas.fill(track, theme::PANEL);
    let thumb = scrollbar_thumb(track, total, visible, first);
    canvas.rounded(Rect::new(thumb.x + 2, thumb.y + 2, thumb.width - 4, thumb.height - 4), 3.0, theme::BORDER);
}

/// A menu drawn inside the window (a context menu, say), over everything
/// else, until something is chosen or it's dismissed.
#[derive(Debug, Clone)]
pub struct Menu {
    pub items: Vec<MenuItem>,
    /// Where it's drawn, set by [`Menu::place`].
    pub rect: Rect,
    pub hovered: Option<usize>,
    /// Where the click that opened it was: its release there doesn't
    /// choose the item that happens to be under the pointer.
    origin: (i32, i32),
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub enabled: bool,
    /// A line above this item, to group items.
    pub separator: bool,
}

impl MenuItem {
    pub fn new(label: &str) -> Self {
        Self { label: String::from(label), enabled: true, separator: false }
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    pub fn after_separator(mut self) -> Self {
        self.separator = true;
        self
    }
}

/// What a click or key did to a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// Still open (redraw if hovering changed).
    Open,
    /// Item chosen; the menu should close.
    Chosen(usize),
    /// Clicked outside or Escape; the menu should close.
    Dismissed,
}

const MENU_PADDING: i32 = 4;

impl Menu {
    /// A menu of `items` with its top-left corner at (x, y) (moved to fit
    /// inside `bounds`, the window).
    pub fn new(items: Vec<MenuItem>, font: &mut Font, x: i32, y: i32, bounds: Rect) -> Self {
        let mut menu = Menu { items, rect: Rect::default(), hovered: None, origin: (x, y) };
        menu.place(font, x, y, bounds);
        menu
    }

    fn item_height(font: &Font) -> i32 {
        font.line_height + 8
    }

    pub fn place(&mut self, font: &mut Font, x: i32, y: i32, bounds: Rect) {
        let width = self.items.iter().map(|item| font.width(&item.label)).max().unwrap_or(0) + 32;
        let height = self.items.len() as i32 * Self::item_height(font) + 2 * MENU_PADDING;
        let x = x.min(bounds.right() - width).max(bounds.x);
        let y = y.min(bounds.bottom() - height).max(bounds.y);
        self.rect = Rect::new(x, y, width, height);
    }

    fn item_at(&self, font: &Font, x: i32, y: i32) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        let index = ((y - self.rect.y - MENU_PADDING).max(0) / Self::item_height(font)) as usize;
        (index < self.items.len() && self.items[index].enabled).then_some(index)
    }

    /// The pointer moved; returns whether the highlight changed.
    pub fn hover(&mut self, font: &Font, x: i32, y: i32) -> bool {
        let hovered = self.item_at(font, x, y);
        let changed = hovered != self.hovered;
        self.hovered = hovered;
        changed
    }

    /// A button was released (menus choose on release, so press, drag and
    /// let go works) or pressed outside.
    pub fn click(&mut self, font: &Font, x: i32, y: i32) -> MenuChoice {
        if !self.rect.contains(x, y) {
            return MenuChoice::Dismissed;
        }
        if (x, y) == self.origin {
            // The release of the click that opened the menu, which opens
            // with an item under the pointer.
            return MenuChoice::Open;
        }
        self.item_at(font, x, y).map_or(MenuChoice::Open, MenuChoice::Chosen)
    }

    pub fn key(&mut self, key: Key) -> MenuChoice {
        let enabled: Vec<usize> = (0..self.items.len()).filter(|&index| self.items[index].enabled).collect();
        let position = self.hovered.and_then(|hovered| enabled.iter().position(|&index| index == hovered));
        match key {
            Key::Escape => return MenuChoice::Dismissed,
            Key::Enter => return self.hovered.map_or(MenuChoice::Dismissed, MenuChoice::Chosen),
            Key::Down => {
                self.hovered = enabled.get(position.map_or(0, |p| (p + 1) % enabled.len().max(1))).copied();
            }
            Key::Up => {
                let previous =
                    position.map_or(enabled.len().saturating_sub(1), |p| (p + enabled.len() - 1) % enabled.len());
                self.hovered = enabled.get(previous).copied();
            }
            _ => {}
        }
        MenuChoice::Open
    }

    pub fn draw(&self, canvas: &mut Canvas, font: &mut Font) {
        let rect = self.rect;
        // A soft shadow, then the menu.
        canvas.rounded(Rect::new(rect.x + 2, rect.y + 3, rect.width, rect.height), 6.0, 0x6000_0000);
        canvas.rounded(rect, 6.0, theme::BORDER);
        canvas.rounded(rect.inset(1), 5.0, theme::PANEL);
        let height = Self::item_height(font);
        for (index, item) in self.items.iter().enumerate() {
            let y = rect.y + MENU_PADDING + index as i32 * height;
            if item.separator && index > 0 {
                canvas.fill(Rect::new(rect.x + 8, y, rect.width - 16, 1), theme::BORDER);
            }
            if self.hovered == Some(index) {
                canvas.rounded(
                    Rect::new(rect.x + MENU_PADDING, y, rect.width - 2 * MENU_PADDING, height),
                    4.0,
                    theme::ACCENT,
                );
            }
            let color = if item.enabled { theme::TEXT } else { theme::DIM_TEXT };
            canvas.text(font, rect.x + 16, y + 4, &item.label, color);
        }
    }
}

/// Small pictures for lists and sidebars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    File,
    /// A program (an executable file).
    Program,
    Home,
    /// A disk or USB stick.
    Disk,
    /// The parent folder.
    Up,
}

/// Draws `icon` to fill the square `rect`.
pub fn icon(canvas: &mut Canvas, icon: Icon, rect: Rect) {
    let size = rect.width.min(rect.height) as f32;
    let (x, y) = (rect.x as f32 + (rect.width as f32 - size) / 2.0, rect.y as f32 + (rect.height as f32 - size) / 2.0);
    // Shapes are designed on a 16 by 16 grid.
    let s = size / 16.0;
    let p = |px: f32, py: f32| (x + px * s, y + py * s);
    let square = |canvas: &mut Canvas, x0: f32, y0: f32, x1: f32, y1: f32, color: u32| {
        let (a, b) = (p(x0, y0), p(x1, y1));
        canvas.fill_path(&Path::polygon(&[a, (b.0, a.1), b, (a.0, b.1)]), color);
    };
    match icon {
        Icon::Folder => {
            canvas.fill_path(
                &Path::polygon(&[p(1.0, 3.0), p(6.0, 3.0), p(7.5, 4.5), p(15.0, 4.5), p(15.0, 13.5), p(1.0, 13.5)]),
                mix(theme::FOLDER, 0xFF00_0000, 50),
            );
            square(canvas, 1.0, 6.0, 15.0, 13.5, theme::FOLDER);
        }
        Icon::File | Icon::Program => {
            let color = if icon == Icon::Program { 0xFF7F_D1A0 } else { theme::FILE };
            canvas.fill_path(
                &Path::polygon(&[p(3.0, 1.0), p(10.0, 1.0), p(13.5, 4.5), p(13.5, 15.0), p(3.0, 15.0)]),
                color,
            );
            canvas.fill_path(&Path::polygon(&[p(10.0, 1.0), p(13.5, 4.5), p(10.0, 4.5)]), mix(color, 0xFF00_0000, 70));
            if icon == Icon::Program {
                let dark = mix(color, 0xFF00_0000, 140);
                canvas.fill_path(
                    &Path::polygon(&[p(5.0, 7.0), p(8.0, 9.5), p(5.0, 12.0), p(5.0, 10.8), p(6.6, 9.5), p(5.0, 8.2)]),
                    dark,
                );
                square(canvas, 8.5, 11.0, 11.5, 12.0, dark);
            }
        }
        Icon::Home => {
            canvas.fill_path(
                &Path::polygon(&[
                    p(8.0, 1.5),
                    p(15.0, 8.0),
                    p(13.0, 8.0),
                    p(13.0, 14.5),
                    p(3.0, 14.5),
                    p(3.0, 8.0),
                    p(1.0, 8.0),
                ]),
                theme::FILE,
            );
            square(canvas, 6.5, 10.0, 9.5, 14.5, theme::PANEL);
        }
        Icon::Disk => {
            square(canvas, 1.5, 4.5, 14.5, 11.5, theme::FILE);
            square(canvas, 11.0, 7.0, 13.0, 9.0, 0xFF7F_D1A0);
        }
        Icon::Up => {
            canvas.fill_path(
                &Path::polygon(&[
                    p(8.0, 2.0),
                    p(14.0, 8.0),
                    p(10.0, 8.0),
                    p(10.0, 14.0),
                    p(6.0, 14.0),
                    p(6.0, 8.0),
                    p(2.0, 8.0),
                ]),
                theme::FILE,
            );
        }
    }
}
