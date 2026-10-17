//! uidemo: every widget in one window, to try the toolkit out (and to
//! check it in `cargo xtask session`).
//!
//! The list on the left selects with the mouse or the arrow keys (Tab
//! moves between it and the text field), a right click opens a menu, and
//! the line at the bottom says what happened last.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ui::widgets::{
    self, ButtonState, FieldChange, Icon, List, ListChange, Menu, MenuChoice, MenuItem, TextField, theme,
};
use ui::{Button, Canvas, Event, Font, Key, Rect, Window};

eli::main!(main);

const ITEMS: [(Icon, &str); 6] = [
    (Icon::Home, "Home"),
    (Icon::Folder, "Documents"),
    (Icon::File, "notes.txt"),
    (Icon::Program, "hello"),
    (Icon::Disk, "USB stick"),
    (Icon::Up, "Up a level"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Field,
}

struct Demo {
    font: Font,
    list: List,
    field: TextField,
    focus: Focus,
    menu: Option<Menu>,
    hovered_button: bool,
    pressed_button: bool,
    status: String,
    rows: Vec<String>,
}

impl Demo {
    fn list_rect(&self, width: i32, height: i32) -> Rect {
        Rect::new(0, 0, 260.min(width / 2), height - 30)
    }

    fn field_rect(&self, width: i32) -> Rect {
        let left = 260.min(width / 2) + 20;
        Rect::new(left, 20, (width - left - 20).min(400), 32)
    }

    fn button_rect(&mut self, width: i32) -> Rect {
        let field = self.field_rect(width);
        Rect::new(field.x, field.bottom() + 12, widgets::button_width(&mut self.font, "Add to list"), 32)
    }

    fn draw(&mut self, canvas: &mut Canvas) {
        let (width, height) = (canvas.width, canvas.height);
        canvas.fill(canvas.bounds(), theme::BACKGROUND);
        let list_rect = self.list_rect(width, height);
        let font = &mut self.font;
        let rows = &self.rows;
        self.list.draw(canvas, list_rect, self.focus == Focus::List, |canvas, index, rect, _| {
            let (icon, _) = ITEMS[index % ITEMS.len()];
            widgets::icon(canvas, icon, Rect::new(rect.x + 8, rect.y + 4, rect.height - 8, rect.height - 8));
            let y = rect.y + (rect.height - font.line_height) / 2;
            canvas.text(font, rect.x + rect.height + 8, y, &rows[index], theme::TEXT);
        });
        canvas.fill(Rect::new(list_rect.right(), 0, 1, list_rect.height), theme::BORDER);
        let field = self.field_rect(width);
        self.field.draw(canvas, &mut self.font, field, self.focus == Focus::Field);
        let button = self.button_rect(width);
        let state = ButtonState { hovered: self.hovered_button, pressed: self.pressed_button, default: true };
        widgets::button(canvas, &mut self.font, button, "Add to list", state);
        let status = Rect::new(0, height - 30, width, 30);
        canvas.fill(status, theme::PANEL);
        canvas.text(&mut self.font, 10, status.y + 6, &self.status, theme::DIM_TEXT);
        if let Some(menu) = &self.menu {
            menu.draw(canvas, &mut self.font);
        }
    }

    fn add(&mut self) {
        let text = String::from(self.field.text().trim());
        if !text.is_empty() {
            self.rows.push(text.clone());
            self.list.len = self.rows.len();
            self.field.set_text("");
            self.status = format!("added {text:?}");
        }
    }
}

fn main() -> i32 {
    let font = match Font::load(15.0) {
        Ok(font) => font,
        Err(error) => {
            eli::eprintln!("uidemo: {error}");
            return 1;
        }
    };
    let mut window = match Window::open("Widgets", "uidemo", 800, 500) {
        Ok(window) => window,
        Err(error) => {
            eli::eprintln!("uidemo: {error}");
            return 1;
        }
    };
    let row_height = font.line_height + 12;
    let mut demo = Demo {
        font,
        list: List::new(row_height),
        field: TextField::new(""),
        focus: Focus::List,
        menu: None,
        hovered_button: false,
        pressed_button: false,
        status: String::from("ready"),
        rows: (0..40).map(|index| String::from(ITEMS[index % ITEMS.len()].1)).collect(),
    };
    demo.list.len = demo.rows.len();
    demo.field.placeholder = String::from("Type something");
    loop {
        let mut redraw = false;
        for event in window.wait(&[], -1) {
            let (width, height) = (window.width, window.height);
            let list_rect = demo.list_rect(width, height);
            redraw = true;
            match event {
                Event::Close => return 0,
                Event::Resize { .. } | Event::Focus(_) => {}
                Event::Key { key, modifiers } => {
                    if let Some(menu) = &mut demo.menu {
                        match menu.key(key) {
                            MenuChoice::Chosen(index) => {
                                demo.status = format!("menu: {}", menu.items[index].label);
                                demo.menu = None;
                            }
                            MenuChoice::Dismissed => demo.menu = None,
                            MenuChoice::Open => {}
                        }
                        continue;
                    }
                    if key == Key::Tab {
                        demo.focus = if demo.focus == Focus::List { Focus::Field } else { Focus::List };
                        continue;
                    }
                    match demo.focus {
                        Focus::List => {
                            if let ListChange::Activated(index) = demo.list.key(key, list_rect) {
                                demo.status = format!("opened {}", demo.rows[index]);
                            }
                        }
                        Focus::Field => match demo.field.key(key, modifiers) {
                            FieldChange::Submit => demo.add(),
                            FieldChange::Cancel => demo.focus = Focus::List,
                            _ => {}
                        },
                    }
                }
                Event::PointerMove { x, y } => {
                    if let Some(menu) = &mut demo.menu {
                        redraw = menu.hover(&demo.font, x, y);
                        continue;
                    }
                    let hovered = demo.button_rect(width).contains(x, y);
                    redraw = demo.list.drag(list_rect, y) || hovered != demo.hovered_button;
                    demo.hovered_button = hovered;
                }
                Event::PointerLeave => demo.hovered_button = false,
                Event::Button { button, pressed, x, y, clicks } => {
                    if let Some(menu) = &mut demo.menu {
                        // Choose on release; a press outside dismisses.
                        match menu.click(&demo.font, x, y) {
                            MenuChoice::Chosen(index) if !pressed => {
                                demo.status = format!("menu: {}", menu.items[index].label);
                                demo.menu = None;
                            }
                            MenuChoice::Dismissed if pressed => demo.menu = None,
                            _ => {}
                        }
                        continue;
                    }
                    match (button, pressed) {
                        (Button::Left, true) => {
                            if demo.field_rect(width).contains(x, y) {
                                demo.focus = Focus::Field;
                                let field = demo.field_rect(width);
                                demo.field.click(&mut demo.font, field, x);
                            } else if demo.button_rect(width).contains(x, y) {
                                demo.pressed_button = true;
                            } else if list_rect.contains(x, y) {
                                demo.focus = Focus::List;
                                if let ListChange::Activated(index) = demo.list.press(list_rect, x, y, clicks) {
                                    demo.status = format!("opened {}", demo.rows[index]);
                                }
                            }
                        }
                        (Button::Left, false) => {
                            demo.list.release();
                            if demo.pressed_button && demo.button_rect(width).contains(x, y) {
                                demo.add();
                            }
                            demo.pressed_button = false;
                        }
                        (Button::Right, true) => {
                            let items = alloc::vec![
                                MenuItem::new("Open"),
                                MenuItem::new("Rename"),
                                MenuItem::new("Copy").after_separator(),
                                MenuItem::new("Paste").disabled(),
                            ];
                            demo.menu = Some(Menu::new(items, &mut demo.font, x, y, Rect::new(0, 0, width, height)));
                        }
                        _ => redraw = false,
                    }
                }
                Event::Scroll { lines } => redraw = demo.list.scroll(lines * 3, list_rect),
                Event::Readable(_) => redraw = false,
            }
        }
        if redraw {
            let mut canvas = window.canvas();
            demo.draw(&mut canvas);
            window.present_all();
        }
    }
}
