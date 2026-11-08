//! The desktop's shell: how windows are arranged, drawn and given input.
//!
//! It's deliberately simple. A bar along the top has the app menu (every
//! installed program with a `.desktop` file, see `apps.rs`), buttons to
//! start a terminal, the file explorer and (once it's added) Firefox, a
//! tab per window, and a clock. Every window fills
//! the rest of the screen (maximized, so toolkits leave out their shadows
//! and rounded corners), and the focused one is on top; dialogs (windows
//! with a parent) float, centered, at the size they choose. Popup menus go
//! where their positioner says, moved to stay on screen.
//!
//! Shortcuts: Super alone opens the app menu, Super+Enter a terminal, Super+E files, Super+F Firefox, Super+Tab the next
//! window, Super+Q closes a window, Ctrl+Alt+Backspace quits the desktop.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apps::{self, App};
use crate::keymap;
use crate::render::{self, Font, Rect, Screen};
use crate::server::{Action, Client, Kind, SurfaceRef};
use crate::surface::Role;
use crate::sys;
use crate::wire::Outgoing;

pub const BAR_HEIGHT: i32 = 28;
/// The size of the bar's text, in pixels per em.
pub const FONT_SIZE: f32 = 15.0;
const BACKGROUND_TOP: u32 = 0xFF16_1E3A;
const BACKGROUND_BOTTOM: u32 = 0xFF0B_1026;
const BAR_COLOR: u32 = 0xFF0B_1026;
const BAR_LINE: u32 = 0xFF2A_3466;
const TEXT: u32 = 0xFFD8_DAE5;
const DIM_TEXT: u32 = 0xFF8D_93B0;
const HIGHLIGHT: u32 = 0xFF3A_4D9A;
const BUTTON: u32 = 0xFF1E_2750;

/// What a bar button starts.
#[derive(Clone, Copy)]
enum Launcher {
    Terminal,
    Files,
    Browser,
}

/// Something clickable on the bar.
#[derive(Clone, Copy)]
enum BarItem {
    Apps,
    Launch(Launcher),
    Window(SurfaceRef),
    Close(SurfaceRef),
}

pub struct Desktop {
    pub screen: Screen,
    font: Font,
    pub clients: BTreeMap<u32, Client>,
    /// Mapped windows, bottom to top.
    windows: Vec<SurfaceRef>,
    /// Mapped popups, bottom to top.
    popups: Vec<SurfaceRef>,
    /// Toplevels and popups made but not yet configured.
    awaiting_configure: Vec<SurfaceRef>,
    focus: Option<SurfaceRef>,
    pointer: (f64, f64),
    pointer_focus: Option<SurfaceRef>,
    /// The pointer image a client set (surface and hotspot), or `None`
    /// for the arrow; `Some(None)` hides it.
    cursor: Option<Option<(SurfaceRef, i32, i32)>>,
    arrow: (i32, i32, Vec<u32>),
    pub serial: u32,
    start_ns: u64,
    keymap: String,
    depressed: u32,
    locked: u32,
    keys_down: Vec<u32>,
    /// Keys whose presses were shortcuts, so their releases aren't sent.
    swallowed: Vec<u32>,
    buttons_down: u32,
    bar: Vec<(Rect, BarItem)>,
    pub running: bool,
    children: Vec<u32>,
    clock: String,
    /// The data source on the clipboard: (client, source id).
    selection: Option<(u32, u32)>,
    menu: Option<AppMenu>,
    /// Super is down and nothing else was pressed with it (releasing it
    /// then opens the menu).
    super_alone: bool,
    /// Whether Firefox is installed (it gets a bar button), checked every
    /// few seconds.
    browser_installed: bool,
    ticks: u32,
    /// The status area left of the clock (the network, for now).
    status: String,
}

/// The open app menu, below its bar button.
struct AppMenu {
    apps: Vec<App>,
    rect: Rect,
    /// The row under the pointer or chosen with the arrow keys.
    selected: Option<usize>,
}

const MENU_ROW: i32 = 30;
const MENU_PADDING: i32 = 6;

impl Desktop {
    pub fn new(screen: Screen, font: Font) -> Self {
        let pointer = (f64::from(screen.width) / 2.0, f64::from(screen.height) / 2.0);
        Self {
            screen,
            font,
            clients: BTreeMap::new(),
            windows: Vec::new(),
            popups: Vec::new(),
            awaiting_configure: Vec::new(),
            focus: None,
            selection: None,
            menu: None,
            super_alone: false,
            browser_installed: browser_path().is_some(),
            ticks: 0,
            status: apps::network_status(),
            pointer,
            pointer_focus: None,
            cursor: None,
            arrow: render::arrow(),
            serial: 1,
            start_ns: eli::time::monotonic_nanos(),
            keymap: keymap::text(),
            depressed: 0,
            locked: 0,
            keys_down: Vec::new(),
            swallowed: Vec::new(),
            buttons_down: 0,
            bar: Vec::new(),
            running: true,
            children: Vec::new(),
            clock: String::new(),
        }
    }

    /// Milliseconds since the desktop started, for event timestamps.
    fn time(&self) -> u32 {
        ((eli::time::monotonic_nanos() - self.start_ns) / 1_000_000) as u32
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    fn send(&mut self, client: u32, event: Outgoing) {
        if let Some(client) = self.clients.get_mut(&client) {
            client.send(event);
        }
    }

    fn surface(&self, (client, id): SurfaceRef) -> Option<&crate::surface::Surface> {
        self.clients.get(&client)?.surfaces.get(&id)
    }

    // ----- Acting on requests -----

    pub fn act(&mut self, client: u32, action: Action) {
        match action {
            Action::None => {}
            Action::Committed(surface) => self.committed((client, surface)),
            Action::NewToplevel(surface) | Action::NewPopup(surface) => self.awaiting_configure.push((client, surface)),
            Action::ToplevelChanged(surface) => {
                if self.windows.contains(&(client, surface)) {
                    self.configure_toplevel((client, surface));
                    self.place_window((client, surface));
                }
                self.screen.damage_all();
            }
            Action::SurfaceDestroyed(surface) => self.forget((client, surface)),
            Action::SetCursor(surface, x, y) => {
                if self.pointer_focus.is_some_and(|(owner, _)| owner == client) {
                    self.damage_cursor();
                    self.cursor = Some(surface.map(|surface| ((client, surface), x, y)));
                    self.damage_cursor();
                }
            }
            Action::PopupGrab(surface) => {
                if let Some(Role::Popup(popup)) =
                    self.clients.get_mut(&client).and_then(|c| c.surfaces.get_mut(&surface)).map(|s| &mut s.role)
                {
                    popup.grab = true;
                }
                self.set_keyboard_focus(Some((client, surface)));
            }
            Action::NewKeyboard(keyboard) => self.new_keyboard(client, keyboard),
            Action::NewPointer => self.update_pointer_focus(),
            Action::NewOutput(output, version) => self.describe_output(client, output, version),
            Action::SetSelection(source) => self.set_selection(source.map(|source| (client, source))),
            Action::SourceDestroyed(source) => {
                if self.selection == Some((client, source)) {
                    self.set_selection(None);
                }
            }
            Action::Receive(mime, fd) => self.receive(&mime, fd),
            Action::NewDataDevice => {
                if self.focus.is_some_and(|(owner, _)| owner == client) {
                    self.offer_selection(client);
                }
            }
        }
    }

    // ----- The clipboard -----

    /// Puts a data source on the clipboard (the old one is told it was
    /// replaced) and tells the focused client.
    fn set_selection(&mut self, selection: Option<(u32, u32)>) {
        if self.selection == selection {
            return;
        }
        if let Some((client, source)) = self.selection {
            // cancelled
            self.send(client, Outgoing::new(source, 2));
        }
        self.selection = selection;
        if let Some((client, _)) = self.focus {
            self.offer_selection(client);
        }
    }

    /// Tells a client what the clipboard holds: a fresh data offer for each
    /// of its data devices, listing the MIME types, then `selection`.
    fn offer_selection(&mut self, client: u32) {
        let mimes = self.selection.and_then(|(owner, source)| self.clients.get(&owner)?.sources.get(&source).cloned());
        let Some(target) = self.clients.get_mut(&client) else { return };
        for (device, version) in target.objects_of(Kind::DataDevice) {
            let Some(mimes) = &mimes else {
                target.send(Outgoing::new(device, 5).uint(0));
                continue;
            };
            let offer = target.new_server_object(Kind::DataOffer, version);
            // data_offer(id), then the offer's MIME types, then selection.
            target.send(Outgoing::new(device, 0).uint(offer));
            for mime in mimes {
                target.send(Outgoing::new(offer, 0).string(mime));
            }
            target.send(Outgoing::new(device, 5).uint(offer));
        }
    }

    /// Asks the clipboard's owner to write its data into `fd`.
    fn receive(&mut self, mime: &str, fd: i32) {
        match self.selection {
            // send(mime type, fd); the descriptor is closed once it's sent.
            Some((client, source)) if self.clients.contains_key(&client) => {
                self.send(client, Outgoing::new(source, 1).string(mime).fd(fd));
            }
            _ => {
                let _ = eli::io::close(fd);
            }
        }
    }

    fn describe_output(&mut self, client: u32, output: u32, version: u32) {
        let (width, height) = (self.screen.width, self.screen.height);
        // A 15.6-inch screen's millimetres, for toolkits that care.
        self.send(
            client,
            Outgoing::new(output, 0).int(0).int(0).int(344).int(194).int(0).string("eliOS").string("Screen").int(0),
        );
        self.send(client, Outgoing::new(output, 1).uint(3).int(width).int(height).int(60_000));
        if version >= 2 {
            self.send(client, Outgoing::new(output, 3).int(1));
            self.send(client, Outgoing::new(output, 2));
        }
    }

    fn new_keyboard(&mut self, client: u32, keyboard: u32) {
        let mut text = self.keymap.clone().into_bytes();
        text.push(0);
        if let Ok(fd) = sys::memory_file("keymap", &text) {
            self.send(client, Outgoing::new(keyboard, 0).uint(1).fd(fd).uint(text.len() as u32));
        }
        let version = self.clients.get(&client).and_then(|c| c.objects.get(&keyboard)).map_or(1, |o| o.version);
        if version >= 4 {
            // 25 repeats a second, after 400 ms.
            self.send(client, Outgoing::new(keyboard, 5).int(25).int(400));
        }
        if let Some((owner, surface)) = self.focus
            && owner == client
        {
            let serial = self.next_serial();
            self.send(client, Outgoing::new(keyboard, 1).uint(serial).uint(surface).array(&[]));
            let (depressed, locked) = (self.depressed, self.locked);
            self.send(client, Outgoing::new(keyboard, 4).uint(serial).uint(depressed).uint(0).uint(locked).uint(0));
        }
    }

    fn committed(&mut self, surface: SurfaceRef) {
        // An xdg surface's first commit: it can now be configured.
        if let Some(index) = self.awaiting_configure.iter().position(|&waiting| waiting == surface)
            && self.surface(surface).and_then(|s| s.xdg.as_ref()).is_some_and(|xdg| xdg.initial_commit)
        {
            self.awaiting_configure.remove(index);
            match self.surface(surface).map(|s| &s.role) {
                Some(Role::Toplevel(_)) => self.configure_toplevel(surface),
                Some(Role::Popup(_)) => self.configure_popup(surface),
                _ => {}
            }
        }
        let Some(s) = self.surface(surface) else { return };
        let has_content = s.has_content;
        match s.role {
            Role::Toplevel(_) => {
                let mapped = self.windows.contains(&surface);
                if has_content && !mapped {
                    self.windows.push(surface);
                    self.place_window(surface);
                    self.enter_outputs(surface);
                    self.set_keyboard_focus(Some(surface));
                    self.screen.damage_all();
                } else if !has_content && mapped {
                    self.forget(surface);
                } else {
                    let before = self.surface_rect(surface);
                    self.place_window(surface);
                    if self.surface_rect(surface) != before {
                        // Moved or resized.
                        before.inspect(|&rect| self.screen.damage(rect));
                        self.damage_surface(surface);
                    }
                    self.damage_applied(surface.0);
                }
            }
            Role::Popup(_) => {
                let mapped = self.popups.contains(&surface);
                if has_content && !mapped {
                    self.popups.push(surface);
                    self.enter_outputs(surface);
                } else if !has_content && mapped {
                    self.forget(surface);
                }
                self.damage_surface(surface);
            }
            Role::Cursor => self.damage_cursor(),
            _ => self.damage_applied(surface.0),
        }
        // Whatever wasn't redrawn above is now in the past.
        if let Some(client) = self.clients.get_mut(&surface.0) {
            client.applied.clear();
        }
    }

    /// Redraws what a client's last commit changed: the damaged parts of
    /// each surface it applied (or all of a surface whose size changed),
    /// or everything if a subsurface moved.
    fn damage_applied(&mut self, client: u32) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let applied = core::mem::take(&mut c.applied);
        if core::mem::take(&mut c.moved) {
            self.screen.damage_all();
            return;
        }
        for (id, changed) in applied {
            let surface = (client, id);
            match changed {
                None => self.damage_surface(surface),
                Some(rect) if !rect.is_empty() => {
                    let Some((x, y)) = self.origin(surface) else { continue };
                    self.screen.damage(Rect::new(x + rect.x, y + rect.y, rect.width, rect.height));
                }
                Some(_) => {}
            }
        }
    }

    /// Tells a newly shown surface which output it's on (toolkits use this
    /// to pick their scale).
    fn enter_outputs(&mut self, (client, surface): SurfaceRef) {
        let outputs = self.clients.get(&client).map(|c| c.objects_of(Kind::Output)).unwrap_or_default();
        for (output, _) in outputs {
            self.send(client, Outgoing::new(surface, 0).uint(output));
        }
    }

    /// A surface (or its window) went away.
    fn forget(&mut self, surface: SurfaceRef) {
        self.windows.retain(|&window| window != surface);
        self.popups.retain(|&popup| popup != surface);
        self.awaiting_configure.retain(|&waiting| waiting != surface);
        if self.pointer_focus == Some(surface) {
            self.pointer_focus = None;
        }
        if self.cursor.is_some_and(|cursor| cursor.is_some_and(|(owner, _, _)| owner == surface)) {
            self.cursor = None;
        }
        if self.focus == Some(surface) {
            self.focus = None;
            let next = self.top_focus();
            self.set_keyboard_focus(next);
        }
        self.screen.damage_all();
    }

    /// Everything of a client that disconnected.
    pub fn client_gone(&mut self, client: u32) {
        if self.selection.is_some_and(|(owner, _)| owner == client) {
            // Nobody to tell it was cancelled.
            self.selection = None;
            if let Some((focused, _)) = self.focus
                && focused != client
            {
                self.offer_selection(focused);
            }
        }
        self.windows.retain(|&(owner, _)| owner != client);
        self.popups.retain(|&(owner, _)| owner != client);
        self.awaiting_configure.retain(|&(owner, _)| owner != client);
        if self.pointer_focus.is_some_and(|(owner, _)| owner == client) {
            self.pointer_focus = None;
            self.cursor = None;
        }
        if self.focus.is_some_and(|(owner, _)| owner == client) {
            self.focus = None;
            let next = self.top_focus();
            self.set_keyboard_focus(next);
        }
        self.screen.damage_all();
    }

    // ----- Layout -----

    fn is_dialog(&self, surface: SurfaceRef) -> bool {
        self.surface(surface).and_then(|s| s.toplevel()).is_some_and(|t| {
            t.parent.is_some() || (t.max_size.0 > 0 && t.max_size.0 == t.min_size.0 && t.max_size.1 == t.min_size.1)
        })
    }

    fn configure_toplevel(&mut self, surface: SurfaceRef) {
        let (width, height) = (self.screen.width, self.screen.height);
        let dialog = self.is_dialog(surface);
        let focused = self.focus == Some(surface);
        let (client, id) = surface;
        let Some(c) = self.clients.get_mut(&client) else { return };
        let Some(s) = c.surfaces.get_mut(&id) else { return };
        let xdg_object = s.xdg.as_ref().map_or(0, |xdg| xdg.object);
        let Some(toplevel) = s.toplevel_mut() else { return };
        let size = if toplevel.fullscreen {
            (width, height)
        } else if dialog {
            (0, 0)
        } else {
            (width, height - BAR_HEIGHT)
        };
        toplevel.configured = size;
        toplevel.activated = focused;
        let object = toplevel.object;
        let fullscreen = toplevel.fullscreen;
        let version = c.objects.get(&object).map_or(1, |o| o.version);
        // States: 1 maximized, 2 fullscreen, 4 activated, 5-8 tiled.
        let mut states: Vec<u32> = Vec::new();
        if fullscreen {
            states.push(2);
        } else if !dialog {
            states.push(1);
            if version >= 2 {
                states.extend([5, 6, 7, 8]);
            }
        }
        if focused {
            states.push(4);
        }
        let bytes: Vec<u8> = states.iter().flat_map(|state| state.to_le_bytes()).collect();
        c.send(Outgoing::new(object, 0).int(size.0).int(size.1).array(&bytes));
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        if let Some(c) = self.clients.get_mut(&client) {
            c.send(Outgoing::new(xdg_object, 0).uint(serial));
        }
    }

    /// Puts a window where it belongs: fullscreen, filling the space below
    /// the bar, or (a dialog) centered.
    fn place_window(&mut self, surface: SurfaceRef) {
        let (width, height) = (self.screen.width, self.screen.height);
        let dialog = self.is_dialog(surface);
        let (client, id) = surface;
        let Some(s) = self.clients.get_mut(&client).and_then(|c| c.surfaces.get_mut(&id)) else { return };
        let geometry = s.geometry();
        let Some(toplevel) = s.toplevel_mut() else { return };
        (toplevel.x, toplevel.y) = if toplevel.fullscreen {
            (0, 0)
        } else if dialog {
            let area = height - BAR_HEIGHT;
            ((width - geometry.width) / 2, (BAR_HEIGHT + (area - geometry.height) / 2).max(BAR_HEIGHT))
        } else {
            (0, BAR_HEIGHT)
        };
    }

    /// Sends a popup its position, moved to stay on screen.
    fn configure_popup(&mut self, surface: SurfaceRef) {
        let (client, id) = surface;
        let Some(Role::Popup(popup)) = self.surface(surface).map(|s| &s.role) else { return };
        let (parent, mut rect, object) = (popup.parent, popup.rect, popup.object);
        let Some((origin_x, origin_y)) = self.geometry_origin((client, parent)) else { return };
        // On screen: slide it back inside the edges.
        let screen = self.screen.bounds();
        let mut x = origin_x + rect.x;
        let mut y = origin_y + rect.y;
        if x + rect.width > screen.width {
            x = screen.width - rect.width;
        }
        if y + rect.height > screen.height {
            y = screen.height - rect.height;
        }
        x = x.max(0);
        y = y.max(0);
        rect.x = x - origin_x;
        rect.y = y - origin_y;
        let xdg_object = self.surface(surface).and_then(|s| s.xdg.as_ref()).map_or(0, |xdg| xdg.object);
        if let Some(Role::Popup(popup)) =
            self.clients.get_mut(&client).and_then(|c| c.surfaces.get_mut(&id)).map(|s| &mut s.role)
        {
            popup.rect = rect;
        }
        self.send(client, Outgoing::new(object, 0).int(rect.x).int(rect.y).int(rect.width).int(rect.height));
        let serial = self.next_serial();
        self.send(client, Outgoing::new(xdg_object, 0).uint(serial));
    }

    /// Where a surface's (0, 0) is on screen.
    fn origin(&self, surface: SurfaceRef) -> Option<(i32, i32)> {
        let s = self.surface(surface)?;
        match &s.role {
            Role::Toplevel(toplevel) => {
                let geometry = s.geometry();
                Some((toplevel.x - geometry.x, toplevel.y - geometry.y))
            }
            Role::Subsurface(sub) => {
                let (x, y) = self.origin((surface.0, sub.parent))?;
                Some((x + sub.x, y + sub.y))
            }
            Role::Popup(popup) => {
                let (x, y) = self.geometry_origin((surface.0, popup.parent))?;
                let geometry = s.geometry();
                Some((x + popup.rect.x - geometry.x, y + popup.rect.y - geometry.y))
            }
            _ => None,
        }
    }

    /// Where a surface's window geometry starts on screen.
    fn geometry_origin(&self, surface: SurfaceRef) -> Option<(i32, i32)> {
        let (x, y) = self.origin(surface)?;
        let geometry = self.surface(surface)?.geometry();
        Some((x + geometry.x, y + geometry.y))
    }

    fn surface_rect(&self, surface: SurfaceRef) -> Option<Rect> {
        let (x, y) = self.origin(surface)?;
        let s = self.surface(surface)?;
        Some(Rect::new(x, y, s.width, s.height))
    }

    fn damage_surface(&mut self, surface: SurfaceRef) {
        if let Some(rect) = self.surface_rect(surface) {
            self.screen.damage(rect);
        }
    }

    // ----- Focus -----

    /// What should have the keyboard: the topmost grabbing popup, or the
    /// top window.
    fn top_focus(&self) -> Option<SurfaceRef> {
        self.popups
            .iter()
            .rev()
            .find(|&&popup| matches!(self.surface(popup).map(|s| &s.role), Some(Role::Popup(p)) if p.grab))
            .or(self.windows.last())
            .copied()
    }

    fn set_keyboard_focus(&mut self, focus: Option<SurfaceRef>) {
        if self.focus == focus {
            return;
        }
        let old = self.focus;
        self.focus = focus;
        if let Some((client, surface)) = old
            && self.surface((client, surface)).is_some()
        {
            let serial = self.next_serial();
            let keyboards = self.clients.get(&client).map(|c| c.objects_of(Kind::Keyboard)).unwrap_or_default();
            for (keyboard, _) in keyboards {
                self.send(client, Outgoing::new(keyboard, 2).uint(serial).uint(surface));
            }
            if self.windows.contains(&(client, surface)) {
                self.configure_toplevel((client, surface));
            }
        }
        if let Some((client, surface)) = focus {
            // A client that gets the keyboard learns what the clipboard holds.
            if old.is_none_or(|(old_client, _)| old_client != client) {
                self.offer_selection(client);
            }
            // A window that gets the keyboard comes to the front.
            if let Some(index) = self.windows.iter().position(|&window| window == (client, surface)) {
                let window = self.windows.remove(index);
                self.windows.push(window);
                self.configure_toplevel(window);
            }
            let serial = self.next_serial();
            let keys: Vec<u8> = self.keys_down.iter().flat_map(|key| key.to_le_bytes()).collect();
            let keyboards = self.clients.get(&client).map(|c| c.objects_of(Kind::Keyboard)).unwrap_or_default();
            let (depressed, locked) = (self.depressed, self.locked);
            for (keyboard, _) in keyboards {
                self.send(client, Outgoing::new(keyboard, 1).uint(serial).uint(surface).array(&keys));
                self.send(client, Outgoing::new(keyboard, 4).uint(serial).uint(depressed).uint(0).uint(locked).uint(0));
            }
        }
        self.screen.damage_all();
    }

    // ----- Drawing -----

    /// Redraws what changed and copies it to the screen.
    pub fn repaint(&mut self) {
        let area = self.screen.damage.intersect(&self.screen.bounds());
        if area.is_empty() {
            return;
        }
        // The background: a vertical gradient.
        let height = self.screen.height.max(1);
        for y in area.y..area.y + area.height {
            let mix = |shift: u32| {
                let (top, bottom) = (BACKGROUND_TOP >> shift & 0xFF, BACKGROUND_BOTTOM >> shift & 0xFF);
                (top * (height - y) as u32 + bottom * y as u32) / height as u32
            };
            let color = 0xFF00_0000 | mix(16) << 16 | mix(8) << 8 | mix(0);
            self.screen.fill(Rect::new(area.x, y, area.width, 1), color);
        }
        let fullscreen_top = self
            .windows
            .last()
            .and_then(|&window| self.surface(window))
            .and_then(|s| s.toplevel())
            .is_some_and(|t| t.fullscreen);
        // Windows bottom to top; only from the last full-screen-sized one,
        // since the ones under it can't show.
        let first = self.windows.iter().rposition(|&window| !self.is_dialog(window)).unwrap_or(0);
        let windows: Vec<SurfaceRef> = self.windows[first..].to_vec();
        for window in windows {
            self.draw_tree(window, area);
        }
        for popup in self.popups.clone() {
            self.draw_tree(popup, area);
        }
        if !fullscreen_top {
            self.draw_bar(area);
        }
        self.draw_menu(area);
        self.draw_cursor(area);
        self.screen.present();
    }

    /// Draws a surface and its subsurfaces.
    fn draw_tree(&mut self, surface: SurfaceRef, clip: Rect) {
        let Some((x, y)) = self.origin(surface) else { return };
        let (client, id) = surface;
        let Some(s) = self.clients.get(&client).and_then(|c| c.surfaces.get(&id)) else { return };
        if s.has_content {
            let height = s.pixels.len() as i32 / s.buffer_width.max(1);
            self.screen.blit(&s.pixels, s.buffer_width, height, s.buffer_width as usize, x, y, s.alpha, clip);
        }
        let children = s.children.clone();
        for child in children {
            self.draw_tree((client, child), clip);
        }
    }

    fn draw_bar(&mut self, clip: Rect) {
        let width = self.screen.width;
        let bar = Rect::new(0, 0, width, BAR_HEIGHT);
        if bar.intersect(&clip).is_empty() {
            return;
        }
        self.screen.fill(bar, BAR_COLOR);
        self.screen.fill(Rect::new(0, BAR_HEIGHT - 1, width, 1), BAR_LINE);
        self.bar.clear();
        let text_y = (BAR_HEIGHT - self.font.line_height) / 2;
        let apps = Rect::new(4, 4, self.font.width("eliOS") + 16, BAR_HEIGHT - 8);
        self.screen.fill(apps, if self.menu.is_some() { HIGHLIGHT } else { BAR_COLOR });
        self.screen.text(&mut self.font, apps.x + 8, text_y, "eliOS", TEXT, clip);
        self.bar.push((apps, BarItem::Apps));
        let mut x = apps.x + apps.width + 8;
        let mut launchers = alloc::vec![("Terminal", Launcher::Terminal), ("Files", Launcher::Files)];
        if self.browser_installed {
            launchers.push(("Firefox", Launcher::Browser));
        }
        for (label, launcher) in launchers {
            let rect = Rect::new(x, 4, self.font.width(label) + 16, BAR_HEIGHT - 8);
            self.screen.fill(rect, BUTTON);
            self.screen.text(&mut self.font, x + 8, text_y, label, TEXT, clip);
            self.bar.push((rect, BarItem::Launch(launcher)));
            x += rect.width + 8;
        }
        x += 16;
        // A tab per window (dialogs belong to their parents).
        let windows: Vec<SurfaceRef> = self.windows.iter().copied().filter(|&w| !self.is_dialog(w)).collect();
        let clock_width = self.font.width("00:00") + 24;
        let mut status = self.status.clone();
        while self.font.width(&status) > 240 {
            status.pop();
        }
        let status_width = if status.is_empty() { 0 } else { self.font.width(&status) + 24 };
        let space = (width - x - clock_width - status_width).max(0);
        let tab_width = if windows.is_empty() { 0 } else { (space / windows.len() as i32).min(240) };
        for window in windows {
            let title = self
                .surface(window)
                .and_then(|s| s.toplevel())
                .map_or(String::new(), |t| if t.title.is_empty() { t.app_id.clone() } else { t.title.clone() });
            let focused = self.focus.is_some_and(|(client, _)| client == window.0)
                && self.windows.last().is_some_and(|&top| top == window || self.is_dialog(top));
            let rect = Rect::new(x, 4, tab_width - 6, BAR_HEIGHT - 8);
            self.screen.fill(rect, if focused { HIGHLIGHT } else { BUTTON });
            let room = rect.width - 32;
            let mut shown = title;
            while self.font.width(&shown) > room {
                shown.pop();
            }
            self.screen.text(&mut self.font, x + 8, text_y, &shown, if focused { TEXT } else { DIM_TEXT }, clip);
            let close = Rect::new(rect.x + rect.width - 20, 4, 18, BAR_HEIGHT - 8);
            self.screen.text(&mut self.font, close.x + 5, text_y, "x", DIM_TEXT, clip);
            self.bar.push((close, BarItem::Close(window)));
            self.bar.push((rect, BarItem::Window(window)));
            x += tab_width;
        }
        let status_x = width - clock_width - status_width + 8;
        self.screen.text(&mut self.font, status_x, text_y, &status, DIM_TEXT, clip);
        let clock = self.clock.clone();
        self.screen.text(&mut self.font, width - clock_width + 8, text_y, &clock, TEXT, clip);
    }

    fn draw_menu(&mut self, clip: Rect) {
        let Some(menu) = &self.menu else { return };
        let rect = menu.rect;
        if rect.intersect(&clip).is_empty() {
            return;
        }
        self.screen.fill(rect, BAR_LINE);
        let inside = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
        self.screen.fill(inside, BAR_COLOR);
        let names: Vec<String> = menu.apps.iter().map(|app| app.name.clone()).collect();
        let selected = menu.selected;
        if names.is_empty() {
            let y = rect.y + MENU_PADDING + (MENU_ROW - self.font.line_height) / 2;
            self.screen.text(&mut self.font, rect.x + 14, y, "No apps installed", DIM_TEXT, clip);
        }
        for (index, name) in names.iter().enumerate() {
            let row = Rect::new(rect.x + 4, rect.y + MENU_PADDING + index as i32 * MENU_ROW, rect.width - 8, MENU_ROW);
            if selected == Some(index) {
                self.screen.fill(row, HIGHLIGHT);
            }
            let y = row.y + (MENU_ROW - self.font.line_height) / 2;
            self.screen.text(&mut self.font, row.x + 10, y, name, TEXT, clip);
        }
    }

    /// Opens the app menu (or closes it if it's open).
    fn toggle_menu(&mut self) {
        if let Some(menu) = self.menu.take() {
            self.screen.damage(menu.rect);
            self.screen.damage(Rect::new(0, 0, self.screen.width, BAR_HEIGHT));
            return;
        }
        let apps = apps::list();
        let widest = apps.iter().map(|app| self.font.width(&app.name)).max().unwrap_or(0);
        let width = (widest + 40).max(self.font.width("No apps installed") + 28).max(220);
        let rows = apps.len().max(1) as i32;
        let height = (rows * MENU_ROW + 2 * MENU_PADDING).min(self.screen.height - BAR_HEIGHT - 8);
        let rect = Rect::new(4, BAR_HEIGHT, width, height);
        self.menu = Some(AppMenu { apps, rect, selected: None });
        self.screen.damage(rect);
        self.screen.damage(Rect::new(0, 0, self.screen.width, BAR_HEIGHT));
        self.browser_installed = browser_path().is_some();
    }

    /// The menu row at a point on screen.
    fn menu_row(&self, x: i32, y: i32) -> Option<usize> {
        let menu = self.menu.as_ref()?;
        if !menu.rect.contains(x, y) || y < menu.rect.y + MENU_PADDING {
            return None;
        }
        let index = ((y - menu.rect.y - MENU_PADDING) / MENU_ROW) as usize;
        (index < menu.apps.len()).then_some(index)
    }

    fn select_in_menu(&mut self, selected: Option<usize>) {
        if let Some(menu) = &mut self.menu
            && menu.selected != selected
        {
            menu.selected = selected;
            let rect = menu.rect;
            self.screen.damage(rect);
        }
    }

    /// Starts the menu's chosen app and closes the menu.
    fn choose_in_menu(&mut self, index: usize) {
        let Some(menu) = self.menu.as_ref() else { return };
        let Some(app) = menu.apps.get(index) else { return };
        let (command, terminal) = (app.command.clone(), app.terminal);
        self.toggle_menu();
        let (program, arguments) = if terminal {
            let mut arguments = alloc::vec![String::from("-e")];
            arguments.extend(command);
            (String::from("term"), arguments)
        } else {
            (command[0].clone(), command[1..].to_vec())
        };
        match eli::process::find_program(&program) {
            Some(path) => self.run(&path, &arguments),
            None => eli::eprintln!("desktop: {program} isn't installed"),
        }
    }

    /// Keys while the menu is open: arrows choose, Enter starts, Escape
    /// closes; others are swallowed.
    fn menu_key(&mut self, code: u32) {
        let Some(menu) = &self.menu else { return };
        let count = menu.apps.len();
        let selected = menu.selected;
        match code {
            1 => self.toggle_menu(),
            28 | 96 => {
                if let Some(index) = selected {
                    self.choose_in_menu(index);
                }
            }
            103 if count > 0 => self.select_in_menu(Some(selected.map_or(count - 1, |i| (i + count - 1) % count))),
            108 if count > 0 => self.select_in_menu(Some(selected.map_or(0, |i| (i + 1) % count))),
            _ => {}
        }
    }

    fn cursor_rect(&self) -> Rect {
        let (x, y) = (self.pointer.0 as i32, self.pointer.1 as i32);
        match self.cursor {
            Some(Some((surface, hx, hy))) => {
                let size = self.surface(surface).map_or((32, 32), |s| (s.width, s.height));
                Rect::new(x - hx, y - hy, size.0.max(1), size.1.max(1))
            }
            Some(None) => Rect::new(x, y, 0, 0),
            None => Rect::new(x, y, self.arrow.0, self.arrow.1),
        }
    }

    fn damage_cursor(&mut self) {
        let rect = self.cursor_rect();
        self.screen.damage(rect);
    }

    fn draw_cursor(&mut self, clip: Rect) {
        let rect = self.cursor_rect();
        match self.cursor {
            Some(Some(((client, id), _, _))) => {
                if let Some(s) = self.clients.get(&client).and_then(|c| c.surfaces.get(&id))
                    && s.has_content
                {
                    let height = s.pixels.len() as i32 / s.buffer_width.max(1);
                    self.screen.blit(
                        &s.pixels,
                        s.buffer_width,
                        height,
                        s.buffer_width as usize,
                        rect.x,
                        rect.y,
                        true,
                        clip,
                    );
                }
            }
            Some(None) => {}
            None => {
                let (width, height, pixels) = (self.arrow.0, self.arrow.1, core::mem::take(&mut self.arrow.2));
                self.screen.blit(&pixels, width, height, width as usize, rect.x, rect.y, true, clip);
                self.arrow.2 = pixels;
            }
        }
    }

    /// Sends frame callbacks: clients draw their next frame now.
    pub fn frame_done(&mut self) {
        let time = self.time();
        for client in self.clients.values_mut() {
            let callbacks: Vec<u32> =
                client.surfaces.values_mut().flat_map(|s| core::mem::take(&mut s.frame_callbacks)).collect();
            for callback in callbacks {
                client.send(Outgoing::new(callback, 0).uint(time));
                client.remove(callback);
            }
        }
    }

    /// Called ~60 times a second.
    pub fn tick(&mut self) {
        let now = eli::time::unix_time();
        let clock = format!("{:02}:{:02}", now / 3600 % 24, now / 60 % 60);
        if clock != self.clock {
            self.clock = clock;
            self.screen.damage(Rect::new(self.screen.width - 80, 0, 80, BAR_HEIGHT));
        }
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(120) {
            let installed = browser_path().is_some();
            let status = apps::network_status();
            if installed != self.browser_installed || status != self.status {
                self.browser_installed = installed;
                self.status = status;
                self.screen.damage(Rect::new(0, 0, self.screen.width, BAR_HEIGHT));
            }
        }
        self.repaint();
        self.frame_done();
        // Programs that ended.
        while let Ok(Some((pid, _))) = eli::process::wait(-1, true) {
            self.children.retain(|&child| child != pid);
        }
    }

    // ----- Input -----

    /// The client surface under the pointer, and the pointer's position in
    /// it.
    fn surface_at(&self, x: i32, y: i32) -> Option<(SurfaceRef, f64, f64)> {
        let fullscreen_top =
            self.windows.last().and_then(|&w| self.surface(w)).and_then(|s| s.toplevel()).is_some_and(|t| t.fullscreen);
        if y < BAR_HEIGHT && !fullscreen_top && self.popups.is_empty() {
            return None;
        }
        if self.menu.as_ref().is_some_and(|menu| menu.rect.contains(x, y)) {
            return None;
        }
        let stack: Vec<SurfaceRef> = self.windows.iter().chain(self.popups.iter()).copied().collect();
        for root in stack.into_iter().rev() {
            if let Some(hit) = self.hit_tree(root, x, y) {
                return Some(hit);
            }
        }
        None
    }

    fn hit_tree(&self, surface: SurfaceRef, x: i32, y: i32) -> Option<(SurfaceRef, f64, f64)> {
        let s = self.surface(surface)?;
        for &child in s.children.iter().rev() {
            if let Some(hit) = self.hit_tree((surface.0, child), x, y) {
                return Some(hit);
            }
        }
        let (origin_x, origin_y) = self.origin(surface)?;
        let (local_x, local_y) = (x - origin_x, y - origin_y);
        (s.has_content && s.accepts_input(local_x, local_y)).then(|| {
            let fraction = (self.pointer.0 - f64::from(x), self.pointer.1 - f64::from(y));
            (surface, f64::from(local_x) + fraction.0, f64::from(local_y) + fraction.1)
        })
    }

    fn pointers(&self, client: u32) -> Vec<(u32, u32)> {
        self.clients.get(&client).map(|c| c.objects_of(Kind::Pointer)).unwrap_or_default()
    }

    /// Sends a pointer event and, for new enough pointers, a frame.
    fn pointer_event(&mut self, client: u32, make: impl Fn(u32) -> Outgoing) {
        for (pointer, version) in self.pointers(client) {
            self.send(client, make(pointer));
            if version >= 5 {
                self.send(client, Outgoing::new(pointer, 5));
            }
        }
    }

    /// Works out which surface the pointer is over, sending leave and
    /// enter events if that changed.
    fn update_pointer_focus(&mut self) {
        let (x, y) = (self.pointer.0 as i32, self.pointer.1 as i32);
        let hit = self.surface_at(x, y);
        let target = hit.map(|(surface, _, _)| surface);
        if target == self.pointer_focus {
            return;
        }
        if let Some((client, surface)) = self.pointer_focus
            && self.surface((client, surface)).is_some()
        {
            let serial = self.next_serial();
            self.pointer_event(client, |pointer| Outgoing::new(pointer, 1).uint(serial).uint(surface));
        }
        self.damage_cursor();
        self.cursor = None;
        self.pointer_focus = target;
        if let Some(((client, surface), local_x, local_y)) = hit {
            let serial = self.next_serial();
            self.pointer_event(client, |pointer| {
                Outgoing::new(pointer, 0).uint(serial).uint(surface).fixed(local_x).fixed(local_y)
            });
        }
        self.damage_cursor();
    }

    /// Moves the pointer by (dx, dy), with some acceleration.
    pub fn pointer_motion(&mut self, dx: f64, dy: f64) {
        let speed = libm_hypot(dx, dy);
        let gain = (1.0 + speed / 8.0).min(3.0);
        self.pointer_to(self.pointer.0 + dx * gain, self.pointer.1 + dy * gain);
    }

    /// Moves the pointer to a place on screen.
    pub fn pointer_to(&mut self, x: f64, y: f64) {
        self.damage_cursor();
        let width = f64::from(self.screen.width - 1);
        let height = f64::from(self.screen.height - 1);
        self.pointer = (x.clamp(0.0, width), y.clamp(0.0, height));
        self.damage_cursor();
        self.update_pointer_focus();
        let (px, py) = (self.pointer.0 as i32, self.pointer.1 as i32);
        if self.menu.is_some() {
            let row = self.menu_row(px, py);
            self.select_in_menu(row);
        }
        if let Some(((client, _), local_x, local_y)) = self.surface_at(px, py)
            && self.pointer_focus.is_some_and(|(owner, _)| owner == client)
        {
            let time = self.time();
            self.pointer_event(client, |pointer| Outgoing::new(pointer, 2).uint(time).fixed(local_x).fixed(local_y));
        }
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        if pressed {
            self.buttons_down += 1;
        } else {
            self.buttons_down = self.buttons_down.saturating_sub(1);
        }
        let (x, y) = (self.pointer.0 as i32, self.pointer.1 as i32);
        self.super_alone = false;
        if pressed && self.buttons_down == 1 && self.menu.is_some() {
            let in_menu = self.menu.as_ref().is_some_and(|menu| menu.rect.contains(x, y));
            if in_menu {
                if let Some(index) = self.menu_row(x, y) {
                    self.choose_in_menu(index);
                }
                return;
            }
            // A click elsewhere closes it (and on the eliOS button, only that).
            self.toggle_menu();
            if self.bar.iter().any(|(rect, item)| matches!(item, BarItem::Apps) && rect.contains(x, y)) {
                return;
            }
        }
        if !pressed && self.menu.as_ref().is_some_and(|menu| menu.rect.contains(x, y)) {
            return;
        }
        if pressed && self.buttons_down == 1 {
            // A click outside a client's popups closes them.
            if !self.popups.is_empty() {
                let over_popup = self.surface_at(x, y).is_some_and(|((client, _), _, _)| {
                    self.popups.iter().any(|&(owner, _)| owner == client)
                        && self.popups.iter().any(|&popup| self.surface_rect(popup).is_some_and(|r| r.contains(x, y)))
                });
                if !over_popup {
                    self.dismiss_popups();
                    return;
                }
            }
            if self.pointer_focus.is_none() {
                self.click_bar(x, y);
                return;
            }
            // Clicking a window focuses it.
            if let Some(((client, surface), _, _)) = self.surface_at(x, y) {
                let window = self.windows.iter().copied().find(|&w| w.0 == client && self.contains_surface(w, surface));
                if let Some(window) = window
                    && self.popups.is_empty()
                {
                    self.set_keyboard_focus(Some(window));
                }
            }
        }
        if let Some((client, _)) = self.pointer_focus {
            let serial = self.next_serial();
            let time = self.time();
            let state = u32::from(pressed);
            self.pointer_event(client, |pointer| {
                Outgoing::new(pointer, 3).uint(serial).uint(time).uint(button).uint(state)
            });
        }
    }

    /// Whether `surface` is `root` or one of its subsurfaces.
    fn contains_surface(&self, root: SurfaceRef, surface: u32) -> bool {
        if root.1 == surface {
            return true;
        }
        self.surface(root)
            .is_some_and(|s| s.children.iter().any(|&child| self.contains_surface((root.0, child), surface)))
    }

    /// Scrolling: `steps` notches of the wheel (positive: down, as Wayland
    /// counts).
    pub fn pointer_axis(&mut self, vertical: bool, steps: i32) {
        let Some((client, _)) = self.pointer_focus else { return };
        let time = self.time();
        let axis = if vertical { 0 } else { 1 };
        for (pointer, version) in self.pointers(client) {
            if version >= 5 {
                self.send(client, Outgoing::new(pointer, 6).uint(0)); // source: wheel
                self.send(client, Outgoing::new(pointer, 8).uint(axis).int(steps));
            }
            self.send(client, Outgoing::new(pointer, 4).uint(time).uint(axis).fixed(f64::from(steps) * 15.0));
            if version >= 5 {
                self.send(client, Outgoing::new(pointer, 5));
            }
        }
    }

    fn dismiss_popups(&mut self) {
        let popups = core::mem::take(&mut self.popups);
        for popup in popups.into_iter().rev() {
            if let Some(Role::Popup(p)) = self.surface(popup).map(|s| &s.role) {
                let object = p.object;
                self.send(popup.0, Outgoing::new(object, 1));
            }
        }
        let next = self.windows.last().copied();
        self.set_keyboard_focus(next);
        self.screen.damage_all();
    }

    fn click_bar(&mut self, x: i32, y: i32) {
        let item = self.bar.iter().find(|(rect, _)| rect.contains(x, y)).map(|&(_, item)| item);
        match item {
            Some(BarItem::Apps) => self.toggle_menu(),
            Some(BarItem::Launch(launcher)) => self.launch(launcher),
            Some(BarItem::Window(window)) => self.set_keyboard_focus(Some(window)),
            Some(BarItem::Close(window)) => self.close(window),
            None => {}
        }
    }

    /// Asks a window to close.
    fn close(&mut self, window: SurfaceRef) {
        if let Some(toplevel) = self.surface(window).and_then(|s| s.toplevel()) {
            let object = toplevel.object;
            self.send(window.0, Outgoing::new(object, 1));
        }
    }

    pub fn key(&mut self, code: u32, pressed: bool) {
        let bit = match code {
            42 | 54 => keymap::SHIFT,
            29 | 97 => keymap::CONTROL,
            56 | 100 => keymap::ALT,
            125 | 126 => keymap::SUPER,
            _ => 0,
        };
        if bit != 0 {
            if pressed {
                self.depressed |= bit;
            } else {
                self.depressed &= !bit;
            }
        }
        if code == 58 && pressed {
            self.locked ^= keymap::LOCK;
        }
        if code == 125 || code == 126 {
            if pressed {
                self.super_alone = true;
            } else if core::mem::take(&mut self.super_alone) {
                self.toggle_menu();
            }
        } else if pressed {
            self.super_alone = false;
        }
        if pressed && bit == 0 && self.menu.is_some() {
            self.menu_key(code);
            self.swallowed.push(code);
            return;
        }
        if pressed {
            if !self.keys_down.contains(&code) {
                self.keys_down.push(code);
            }
            if self.shortcut(code) {
                self.swallowed.push(code);
                return;
            }
        } else {
            self.keys_down.retain(|&key| key != code);
            if let Some(index) = self.swallowed.iter().position(|&key| key == code) {
                self.swallowed.remove(index);
                return;
            }
        }
        let Some((client, _)) = self.focus else { return };
        let serial = self.next_serial();
        let time = self.time();
        let keyboards = self.clients.get(&client).map(|c| c.objects_of(Kind::Keyboard)).unwrap_or_default();
        let (depressed, locked) = (self.depressed, self.locked);
        for (keyboard, _) in keyboards {
            self.send(client, Outgoing::new(keyboard, 3).uint(serial).uint(time).uint(code).uint(u32::from(pressed)));
            if bit != 0 || code == 58 {
                self.send(client, Outgoing::new(keyboard, 4).uint(serial).uint(depressed).uint(0).uint(locked).uint(0));
            }
        }
    }

    /// The desktop's own key combinations; returns whether `code` was one.
    fn shortcut(&mut self, code: u32) -> bool {
        let modifiers = self.depressed;
        if modifiers & (keymap::CONTROL | keymap::ALT) == keymap::CONTROL | keymap::ALT && code == 14 {
            self.running = false;
            return true;
        }
        if modifiers & keymap::SUPER == 0 {
            return false;
        }
        match code {
            28 => self.launch(Launcher::Terminal),
            18 => self.launch(Launcher::Files),
            33 => self.launch(Launcher::Browser),
            16 => {
                if let Some(window) = self.windows.last().copied() {
                    self.close(window);
                }
            }
            15 => {
                if self.windows.len() > 1 {
                    let bottom = self.windows[0];
                    self.set_keyboard_focus(Some(bottom));
                }
            }
            _ => return false,
        }
        true
    }

    fn launch(&mut self, launcher: Launcher) {
        let candidates: &[&str] = match launcher {
            Launcher::Terminal => &["term", "foot"],
            Launcher::Files => &["files"],
            Launcher::Browser => &BROWSERS,
        };
        let Some(path) = candidates.iter().find_map(|name| eli::process::find_program(name)) else {
            eli::eprintln!("desktop: none of {} is installed (try: eli add {})", candidates.join(", "), candidates[0]);
            return;
        };
        self.run(&path, &[]);
    }

    /// Starts a program, in a process group of its own.
    pub fn run(&mut self, path: &str, arguments: &[String]) {
        let mut argv = alloc::vec![path.rsplit('/').next().unwrap_or(path).to_string()];
        argv.extend_from_slice(arguments);
        match eli::process::fork() {
            Ok(0) => {
                let _ = eli::process::set_group(0, 0);
                let error = eli::process::exec(path, &argv, &eli::env::vars());
                eli::eprintln!("desktop: can't run {path}: {error}");
                eli::process::exit(127);
            }
            Ok(pid) => self.children.push(pid),
            Err(error) => eli::eprintln!("desktop: can't start {path}: {error}"),
        }
    }
}

const BROWSERS: [&str; 2] = ["firefox", "firefox-esr"];

fn browser_path() -> Option<String> {
    BROWSERS.iter().find_map(|name| eli::process::find_program(name))
}

/// sqrt(x² + y²), without a maths library.
fn libm_hypot(x: f64, y: f64) -> f64 {
    let square = x * x + y * y;
    if square == 0.0 {
        return 0.0;
    }
    // Newton's method from a rough start.
    let mut root = square.max(1.0);
    for _ in 0..20 {
        root = (root + square / root) / 2.0;
    }
    root
}
