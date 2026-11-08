//! A window on the desktop: the client end of Wayland.
//!
//! A program opens a [`Window`], draws into its [`Canvas`] whenever
//! something changes, says which part changed with [`Window::present`],
//! and handles the [`Event`]s [`Window::wait`] returns. The rest is done
//! here:
//!
//! - Drawing goes into ordinary memory, and only changed rectangles are
//!   copied to the shared-memory buffers the compositor reads. Each
//!   buffer remembers what changed since it was last shown, so a buffer
//!   coming back from the compositor is brought up to date with one copy.
//! - At most one frame is in flight: after a commit, further changes wait
//!   for the compositor's frame callback. A program that changes its
//!   window a thousand times a second (a terminal running `yes`) still
//!   only sends what the screen can show.
//! - Keys repeat here, at the rate the compositor asks for, as Wayland
//!   leaves that to clients.
//! - The clipboard is Wayland's: [`Window::set_clipboard`] offers text
//!   to every program (ours and others, like Firefox), and
//!   [`Window::clipboard`] reads whatever another program offers.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::draw::{Canvas, Rect};
use crate::keys::{self, Key, Modifiers};
use crate::sys;
use crate::wire::{Arguments, Broken, Connection, Message, Outgoing};

/// Something that happened to the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// The window is now this size (or was just opened): everything has
    /// to be drawn again.
    Resize {
        width: i32,
        height: i32,
    },
    /// The window was asked to close (or the desktop went away).
    Close,
    /// The window got or lost the keyboard.
    Focus(bool),
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerLeave,
    /// A mouse button went down or up; `clicks` counts quick presses in
    /// the same place (2 for a double click).
    Button {
        button: Button,
        pressed: bool,
        x: i32,
        y: i32,
        clicks: u32,
    },
    /// The wheel turned: positive is down, in lines.
    Scroll {
        lines: i32,
    },
    /// A descriptor passed to [`Window::wait`] is readable (or closed).
    Readable(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other(u32),
}

/// What each of our protocol objects is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Object {
    Display,
    Registry,
    /// A `wl_display.sync` callback (a round trip).
    Sync,
    Frame,
    Compositor,
    Shm,
    Buffer,
    Surface,
    WmBase,
    XdgSurface,
    Toplevel,
    Seat,
    Pointer,
    Keyboard,
    DataDeviceManager,
    DataDevice,
    /// The text we put on the clipboard.
    DataSource,
    /// What another program put there (made by the compositor).
    DataOffer,
    /// Anything whose events are ignored (pools, outputs).
    Other,
}

/// A shared-memory buffer the compositor reads the window from.
struct Buffer {
    id: u32,
    memory: *mut u32,
    len: usize,
    width: i32,
    height: i32,
    /// The compositor has it (until it says `release`).
    busy: bool,
    /// What changed in the window since this buffer was last filled.
    stale: Rect,
}

impl Buffer {
    fn unmap(&mut self) {
        // SAFETY: mapped by `Window::new_buffer` and not used again.
        unsafe { sys::unmap(self.memory.cast(), self.len) };
    }
}

pub struct Window {
    connection: Connection,
    objects: BTreeMap<u32, Object>,
    next_id: u32,
    free_ids: Vec<u32>,
    compositor: u32,
    shm: u32,
    wm_base: u32,
    seat: u32,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    pointer: u32,
    keyboard: u32,
    pub width: i32,
    pub height: i32,
    /// The size asked for when the compositor leaves it to us.
    preferred: (i32, i32),
    /// Size from the latest toplevel `configure`, applied with the
    /// `xdg_surface.configure` that follows.
    pending_size: (i32, i32),
    configured: bool,
    pixels: Vec<u32>,
    /// What's been drawn since the last commit.
    damage: Rect,
    buffers: Vec<Buffer>,
    frame_pending: bool,
    syncs_done: u32,
    events: VecDeque<Event>,
    modifiers: Modifiers,
    /// Key repeat: per second, and the delay before it starts (ms).
    repeat_rate: u32,
    repeat_delay: u32,
    /// The key being repeated, and when it next repeats.
    repeating: Option<(u32, u64)>,
    pointer_position: (f64, f64),
    /// The last press: (button, when, x, y, clicks).
    last_press: Option<(Button, u64, i32, i32, u32)>,
    scroll_remainder: f64,
    closed: bool,
    data_device_manager: u32,
    data_device: u32,
    /// The latest key or button press's serial: setting the clipboard
    /// quotes it, to show it was asked for.
    input_serial: u32,
    /// What we put on the clipboard, while it's still there.
    copied: Option<(u32, String)>,
    /// The clipboard's offer (from another program, or our own), with its
    /// MIME types; and offers still being described.
    selection: Option<u32>,
    offers: BTreeMap<u32, Vec<String>>,
}

/// The MIME types text is offered as (the last ones are X11's names, which
/// some programs still ask for); pasting takes the first one offered.
const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
/// How long to wait for a program to hand over what it copied.
const PASTE_TIMEOUT_MS: u64 = 2000;

/// How close in time and place presses must be to count as one click
/// more.
const MULTI_CLICK_NS: u64 = 400_000_000;
const MULTI_CLICK_DISTANCE: i32 = 4;
/// What the compositor sends per wheel step (and so one line).
const SCROLL_STEP: f64 = 15.0;

impl Window {
    /// Connects to the desktop and opens a window `width` by `height`
    /// (unless the desktop chooses the size, as ours does), called `title`.
    /// `app_id` names the program, as `.desktop` files do.
    pub fn open(title: &str, app_id: &str, width: i32, height: i32) -> Result<Self, String> {
        let display = eli::env::var("WAYLAND_DISPLAY").unwrap_or_else(|| String::from("wayland-0"));
        let path = if display.starts_with('/') {
            display
        } else {
            let directory = eli::env::var("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR isn't set")?;
            format!("{directory}/{display}")
        };
        let fd = sys::connect_unix(&path).map_err(|error| format!("can't connect to {path}: {error}"))?;
        let mut window = Window {
            connection: Connection::new(fd),
            objects: BTreeMap::from([(1, Object::Display)]),
            next_id: 2,
            free_ids: Vec::new(),
            compositor: 0,
            shm: 0,
            wm_base: 0,
            seat: 0,
            surface: 0,
            xdg_surface: 0,
            toplevel: 0,
            pointer: 0,
            keyboard: 0,
            width: 0,
            height: 0,
            preferred: (width.max(1), height.max(1)),
            pending_size: (0, 0),
            configured: false,
            pixels: Vec::new(),
            damage: Rect::default(),
            buffers: Vec::new(),
            frame_pending: false,
            syncs_done: 0,
            events: VecDeque::new(),
            modifiers: Modifiers::default(),
            repeat_rate: 25,
            repeat_delay: 400,
            repeating: None,
            pointer_position: (0.0, 0.0),
            last_press: None,
            scroll_remainder: 0.0,
            closed: false,
            data_device_manager: 0,
            data_device: 0,
            input_serial: 0,
            copied: None,
            selection: None,
            offers: BTreeMap::new(),
        };
        let registry = window.new_object(Object::Registry);
        window.send(Outgoing::new(1, 1).uint(registry));
        // The globals arrive before the round trip ends.
        window.round_trip()?;
        for (name, what) in
            [("wl_compositor", window.compositor), ("wl_shm", window.shm), ("xdg_wm_base", window.wm_base)]
        {
            if what == 0 {
                return Err(format!("the desktop has no {name}"));
            }
        }
        // The seat's capabilities, which bring the pointer and keyboard.
        window.round_trip()?;

        window.surface = window.new_object(Object::Surface);
        window.send(Outgoing::new(window.compositor, 0).uint(window.surface));
        window.xdg_surface = window.new_object(Object::XdgSurface);
        window.send(Outgoing::new(window.wm_base, 2).uint(window.xdg_surface).uint(window.surface));
        window.toplevel = window.new_object(Object::Toplevel);
        window.send(Outgoing::new(window.xdg_surface, 1).uint(window.toplevel));
        window.send(Outgoing::new(window.toplevel, 2).string(title));
        window.send(Outgoing::new(window.toplevel, 3).string(app_id));
        window.send(Outgoing::new(window.surface, 6));
        while !window.configured {
            window.round_trip()?;
        }
        Ok(window)
    }

    /// The window's pixels, to draw into. Say what changed with
    /// [`Window::present`].
    pub fn canvas(&mut self) -> Canvas<'_> {
        Canvas::new(&mut self.pixels, self.width, self.height)
    }

    /// Shows what was drawn in `rect` (as soon as the compositor is ready
    /// for another frame).
    pub fn present(&mut self, rect: Rect) {
        let rect = rect.intersect(&Rect::new(0, 0, self.width, self.height));
        if !rect.is_empty() {
            self.damage = self.damage.union(&rect);
            self.commit();
        }
    }

    pub fn present_all(&mut self) {
        self.present(Rect::new(0, 0, self.width, self.height));
    }

    pub fn set_title(&mut self, title: &str) {
        self.send(Outgoing::new(self.toplevel, 2).string(title));
    }

    /// Whether something drawn now would be shown straight away: no frame
    /// is waiting for the compositor. Programs that change often (a
    /// terminal under heavy output) draw only then; the frame callback
    /// wakes [`Window::wait`] when it's time.
    pub fn ready(&self) -> bool {
        self.configured && !self.frame_pending && !self.closed
    }

    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Waits for something to happen, for at most `timeout_ms` (-1:
    /// forever), also watching `watch` for input; returns what happened
    /// (nothing if the time ran out).
    pub fn wait(&mut self, watch: &[i32], timeout_ms: i32) -> Vec<Event> {
        if self.events.is_empty() && !self.closed {
            let _ = self.connection.flush();
            let mut timeout = timeout_ms;
            if let Some((_, at)) = self.repeating {
                let now = eli::time::monotonic_nanos();
                let until = (at.saturating_sub(now) / 1_000_000) as i32;
                timeout = if timeout < 0 { until } else { timeout.min(until) };
            }
            let mut fds =
                vec![(self.connection.fd, sys::POLLIN | if self.connection.has_output() { sys::POLLOUT } else { 0 })];
            fds.extend(watch.iter().map(|&fd| (fd, sys::POLLIN)));
            let ready = sys::poll(&fds, timeout);
            if ready[0] & (sys::POLLIN | sys::POLLHUP | sys::POLLERR) != 0 {
                self.receive();
            }
            if ready[0] & sys::POLLOUT != 0 {
                let _ = self.connection.flush();
            }
            for (&fd, &revents) in watch.iter().zip(&ready[1..]) {
                if revents != 0 {
                    self.events.push_back(Event::Readable(fd));
                }
            }
            self.repeat_keys();
        }
        if self.closed && !self.events.contains(&Event::Close) {
            self.events.push_back(Event::Close);
        }
        let _ = self.connection.flush();
        self.events.drain(..).collect()
    }

    fn repeat_keys(&mut self) {
        let Some((code, at)) = self.repeating else { return };
        let now = eli::time::monotonic_nanos();
        if now < at {
            return;
        }
        let key = keys::translate(code, self.modifiers);
        self.events.push_back(Event::Key { key, modifiers: self.modifiers });
        let period = 1_000_000_000 / u64::from(self.repeat_rate.max(1));
        // Behind (a slow redraw): don't send a burst to catch up.
        self.repeating = Some((code, (at + period).max(now)));
    }

    fn new_object(&mut self, kind: Object) -> u32 {
        let id = self.free_ids.pop().unwrap_or_else(|| {
            self.next_id += 1;
            self.next_id - 1
        });
        self.objects.insert(id, kind);
        id
    }

    fn send(&mut self, message: Outgoing) {
        self.connection.send(message);
    }

    /// Sends a `sync` and handles events until its answer comes back, so
    /// everything sent before has been dealt with.
    fn round_trip(&mut self) -> Result<(), String> {
        let callback = self.new_object(Object::Sync);
        self.send(Outgoing::new(1, 0).uint(callback));
        let target = self.syncs_done + 1;
        while self.syncs_done < target {
            if self.connection.flush().is_err() || self.closed {
                return Err(String::from("the desktop went away"));
            }
            sys::poll(&[(self.connection.fd, sys::POLLIN)], -1);
            self.receive();
        }
        Ok(())
    }

    /// Reads and handles whatever the compositor has sent.
    fn receive(&mut self) {
        if self.connection.read().is_err() {
            self.closed = true;
        }
        loop {
            match self.connection.next_message() {
                Ok(Some(message)) => {
                    if let Err(error) = self.handle(message) {
                        eli::eprintln!("{}: bad message from the desktop: {error:?}", eli::env::program_name());
                        self.closed = true;
                        return;
                    }
                }
                Ok(None) => return,
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
    }

    fn handle(&mut self, message: Message) -> Result<(), Broken> {
        let Some(&kind) = self.objects.get(&message.object) else { return Ok(()) };
        let mut args = Arguments::new(&message.arguments);
        match (kind, message.opcode) {
            (Object::Display, 0) => {
                let (object, code) = (args.uint()?, args.uint()?);
                let text = args.string()?.unwrap_or_default();
                eli::eprintln!("{}: the desktop says: object {object}, error {code}: {text}", eli::env::program_name());
                self.closed = true;
            }
            (Object::Display, 1) => {
                let id = args.uint()?;
                if self.objects.remove(&id).is_some() {
                    self.free_ids.push(id);
                }
            }
            (Object::Registry, 0) => {
                let (name, interface, version) = (args.uint()?, args.string()?.unwrap_or_default(), args.uint()?);
                let (kind, wanted) = match interface.as_str() {
                    "wl_compositor" if self.compositor == 0 => (Object::Compositor, 4),
                    "wl_shm" if self.shm == 0 => (Object::Shm, 1),
                    "xdg_wm_base" if self.wm_base == 0 => (Object::WmBase, 2),
                    "wl_seat" if self.seat == 0 => (Object::Seat, 5),
                    "wl_data_device_manager" if self.data_device_manager == 0 => (Object::DataDeviceManager, 3),
                    _ => return Ok(()),
                };
                let version = version.min(wanted);
                let id = self.new_object(kind);
                self.send(Outgoing::new(message.object, 0).uint(name).string(&interface).uint(version).uint(id));
                match kind {
                    Object::Compositor => self.compositor = id,
                    Object::Shm => self.shm = id,
                    Object::WmBase => self.wm_base = id,
                    Object::DataDeviceManager => self.data_device_manager = id,
                    _ => self.seat = id,
                }
                self.get_data_device();
            }
            (Object::Sync, 0) => self.syncs_done += 1,
            (Object::Frame, 0) => {
                self.frame_pending = false;
                self.commit();
            }
            (Object::Buffer, 0) => {
                if let Some(buffer) = self.buffers.iter_mut().find(|buffer| buffer.id == message.object) {
                    buffer.busy = false;
                }
                // Released buffers of an old size aren't needed again.
                let (width, height) = (self.width, self.height);
                let mut index = 0;
                while index < self.buffers.len() {
                    let buffer = &self.buffers[index];
                    if !buffer.busy && (buffer.width, buffer.height) != (width, height) {
                        let mut buffer = self.buffers.remove(index);
                        self.send(Outgoing::new(buffer.id, 0));
                        buffer.unmap();
                    } else {
                        index += 1;
                    }
                }
            }
            (Object::WmBase, 0) => {
                let serial = args.uint()?;
                self.send(Outgoing::new(message.object, 3).uint(serial));
            }
            (Object::Toplevel, 0) => {
                self.pending_size = (args.int()?, args.int()?);
            }
            (Object::Toplevel, 1) => self.events.push_back(Event::Close),
            (Object::XdgSurface, 0) => {
                let serial = args.uint()?;
                self.send(Outgoing::new(message.object, 4).uint(serial));
                let (mut width, mut height) = self.pending_size;
                if width <= 0 {
                    width = if self.configured { self.width } else { self.preferred.0 };
                }
                if height <= 0 {
                    height = if self.configured { self.height } else { self.preferred.1 };
                }
                if !self.configured || (width, height) != (self.width, self.height) {
                    self.configured = true;
                    self.resize(width, height);
                }
            }
            (Object::Seat, 0) => {
                let capabilities = args.uint()?;
                if capabilities & 1 != 0 && self.pointer == 0 {
                    self.pointer = self.new_object(Object::Pointer);
                    self.send(Outgoing::new(self.seat, 0).uint(self.pointer));
                }
                if capabilities & 2 != 0 && self.keyboard == 0 {
                    self.keyboard = self.new_object(Object::Keyboard);
                    self.send(Outgoing::new(self.seat, 1).uint(self.keyboard));
                }
                self.get_data_device();
            }
            // data_offer(id): a new offer, described by the events after.
            (Object::DataDevice, 0) => {
                let offer = args.uint()?;
                self.objects.insert(offer, Object::DataOffer);
                self.offers.insert(offer, Vec::new());
            }
            // selection(offer or null): what the clipboard holds now.
            (Object::DataDevice, 5) => {
                let offer = args.uint()?;
                if let Some(old) = self.selection.take()
                    && old != offer
                {
                    self.destroy_offer(old);
                }
                self.selection = (offer != 0).then_some(offer);
            }
            // offer(mime type)
            (Object::DataOffer, 0) => {
                let mime = args.string()?.unwrap_or_default();
                if let Some(mimes) = self.offers.get_mut(&message.object) {
                    mimes.push(mime);
                }
            }
            // send(mime type, fd): another program is pasting our text.
            (Object::DataSource, 1) => {
                let _ = args.string()?;
                if let Some(fd) = self.connection.take_fd() {
                    if let Some((_, text)) = &self.copied {
                        // A reader that gives up mustn't kill us with SIGPIPE.
                        eli::process::ignore_signal(13);
                        let _ = eli::io::write_all(fd, text.as_bytes());
                        eli::process::default_signal(13);
                    }
                    let _ = eli::io::close(fd);
                }
            }
            // cancelled: something else was copied.
            (Object::DataSource, 2) => {
                self.send(Outgoing::new(message.object, 1));
                if self.copied.as_ref().is_some_and(|(source, _)| *source == message.object) {
                    self.copied = None;
                }
            }
            (Object::Pointer, opcode) => self.pointer_event(opcode, &mut args)?,
            (Object::Keyboard, opcode) => self.keyboard_event(opcode, &mut args)?,
            _ => {}
        }
        Ok(())
    }

    fn pointer_event(&mut self, opcode: u16, args: &mut Arguments) -> Result<(), Broken> {
        match opcode {
            // enter(serial, surface, x, y), motion(time, x, y)
            0 | 2 => {
                if opcode == 0 {
                    args.uint()?;
                }
                args.uint()?;
                self.pointer_position = (args.fixed()?, args.fixed()?);
                let (x, y) = (self.pointer_position.0 as i32, self.pointer_position.1 as i32);
                // Only the latest position matters.
                if let Some(Event::PointerMove { .. }) = self.events.back() {
                    self.events.pop_back();
                }
                self.events.push_back(Event::PointerMove { x, y });
            }
            1 => self.events.push_back(Event::PointerLeave),
            // button(serial, time, button, state)
            3 => {
                self.input_serial = args.uint()?;
                args.uint()?;
                let (code, pressed) = (args.uint()?, args.uint()? == 1);
                let button = match code {
                    0x110 => Button::Left,
                    0x111 => Button::Right,
                    0x112 => Button::Middle,
                    other => Button::Other(other),
                };
                let (x, y) = (self.pointer_position.0 as i32, self.pointer_position.1 as i32);
                let mut clicks = 1;
                if pressed {
                    let now = eli::time::monotonic_nanos();
                    if let Some((last, at, last_x, last_y, count)) = self.last_press
                        && last == button
                        && now - at < MULTI_CLICK_NS
                        && (x - last_x).abs() <= MULTI_CLICK_DISTANCE
                        && (y - last_y).abs() <= MULTI_CLICK_DISTANCE
                    {
                        clicks = count + 1;
                    }
                    self.last_press = Some((button, now, x, y, clicks));
                } else if let Some((_, _, _, _, count)) = self.last_press {
                    clicks = count;
                }
                self.events.push_back(Event::Button { button, pressed, x, y, clicks });
            }
            // axis(time, axis, value): vertical only.
            4 => {
                let (_, axis, value) = (args.uint()?, args.uint()?, args.fixed()?);
                if axis == 0 {
                    self.scroll_remainder += value / SCROLL_STEP;
                    let lines = self.scroll_remainder as i32;
                    if lines != 0 {
                        self.scroll_remainder -= f64::from(lines);
                        self.events.push_back(Event::Scroll { lines });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn keyboard_event(&mut self, opcode: u16, args: &mut Arguments) -> Result<(), Broken> {
        match opcode {
            // keymap(format, fd, size): we know the map (see `keys`).
            0 => {
                if let Some(fd) = self.connection.take_fd() {
                    let _ = eli::io::close(fd);
                }
            }
            1 => self.events.push_back(Event::Focus(true)),
            2 => {
                self.repeating = None;
                self.events.push_back(Event::Focus(false));
            }
            // key(serial, time, key, state)
            3 => {
                self.input_serial = args.uint()?;
                args.uint()?;
                let (code, pressed) = (args.uint()?, args.uint()? == 1);
                if pressed {
                    let key = keys::translate(code, self.modifiers);
                    self.events.push_back(Event::Key { key, modifiers: self.modifiers });
                    if keys::repeats(code) && self.repeat_rate > 0 {
                        let at = eli::time::monotonic_nanos() + u64::from(self.repeat_delay) * 1_000_000;
                        self.repeating = Some((code, at));
                    }
                } else if self.repeating.is_some_and(|(repeating, _)| repeating == code) {
                    self.repeating = None;
                }
            }
            // modifiers(serial, depressed, latched, locked, group)
            4 => {
                let _ = args.uint()?;
                let (depressed, _, locked) = (args.uint()?, args.uint()?, args.uint()?);
                self.modifiers = Modifiers::from_xkb(depressed, locked);
            }
            5 => {
                self.repeat_rate = args.int()?.max(0) as u32;
                self.repeat_delay = args.int()?.max(0) as u32;
            }
            _ => {}
        }
        Ok(())
    }

    /// The seat's data device, once there's a seat and a data device
    /// manager (whichever the registry announces last).
    fn get_data_device(&mut self) {
        if self.data_device == 0 && self.seat != 0 && self.data_device_manager != 0 {
            self.data_device = self.new_object(Object::DataDevice);
            self.send(Outgoing::new(self.data_device_manager, 1).uint(self.data_device).uint(self.seat));
        }
    }

    fn destroy_offer(&mut self, offer: u32) {
        self.offers.remove(&offer);
        if self.objects.remove(&offer).is_some() {
            self.send(Outgoing::new(offer, 2));
        }
    }

    /// Puts `text` on the clipboard, for every program to paste. Call it
    /// while handling the key or click that asked for it.
    pub fn set_clipboard(&mut self, text: &str) {
        if self.data_device == 0 {
            return;
        }
        let source = self.new_object(Object::DataSource);
        self.send(Outgoing::new(self.data_device_manager, 0).uint(source));
        for mime in TEXT_TYPES {
            self.send(Outgoing::new(source, 0).string(mime));
        }
        self.send(Outgoing::new(self.data_device, 1).uint(source).uint(self.input_serial));
        if let Some((old, _)) = self.copied.replace((source, String::from(text))) {
            self.send(Outgoing::new(old, 1));
        }
        let _ = self.connection.flush();
    }

    /// The text on the clipboard, whichever program put it there (`None`
    /// if it's empty or not text).
    pub fn clipboard(&mut self) -> Option<String> {
        // Ours: no need to ask ourselves through a pipe (which would
        // deadlock on big texts).
        if let Some((_, text)) = &self.copied {
            return Some(text.clone());
        }
        let offer = self.selection?;
        let mimes = self.offers.get(&offer)?;
        let mime = TEXT_TYPES.iter().find(|mime| mimes.iter().any(|offered| offered == *mime))?;
        let (read_end, write_end) = eli::io::pipe().ok()?;
        // receive(mime type, fd); our copy of the write end is closed once
        // it's sent, so the read sees the end when the other program closes.
        self.send(Outgoing::new(offer, 1).string(mime).fd(write_end));
        if self.connection.flush().is_err() {
            let _ = eli::io::close(read_end);
            return None;
        }
        let deadline = eli::time::monotonic_nanos() + PASTE_TIMEOUT_MS * 1_000_000;
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let left = deadline.saturating_sub(eli::time::monotonic_nanos()) / 1_000_000;
            if left == 0 || sys::poll(&[(read_end, sys::POLLIN)], left as i32)[0] == 0 {
                break;
            }
            match eli::io::read(read_end, &mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(len) => data.extend_from_slice(&chunk[..len]),
            }
        }
        let _ = eli::io::close(read_end);
        Some(String::from_utf8_lossy(&data).into_owned())
    }

    fn resize(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.pixels = vec![0xFF00_0000; (self.width * self.height) as usize];
        self.damage = Rect::default();
        self.events.push_back(Event::Resize { width: self.width, height: self.height });
    }

    /// Sends what's been drawn, if the compositor is ready for it.
    fn commit(&mut self) {
        if !self.configured || self.frame_pending || self.damage.is_empty() || self.closed {
            return;
        }
        let index = match self
            .buffers
            .iter()
            .position(|buffer| !buffer.busy && (buffer.width, buffer.height) == (self.width, self.height))
        {
            Some(index) => index,
            None => match self.new_buffer() {
                Some(index) => index,
                None => return,
            },
        };
        let damage = self.damage;
        self.damage = Rect::default();
        for buffer in &mut self.buffers {
            buffer.stale = buffer.stale.union(&damage);
        }
        let buffer = &mut self.buffers[index];
        let copy = buffer.stale.intersect(&Rect::new(0, 0, buffer.width, buffer.height));
        buffer.stale = Rect::default();
        buffer.busy = true;
        let width = buffer.width as usize;
        for y in copy.y..copy.bottom() {
            let start = y as usize * width + copy.x as usize;
            let source = &self.pixels[start..start + copy.width as usize];
            // SAFETY: the row lies within the buffer's mapping, which is
            // `width * height` pixels.
            unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), buffer.memory.add(start), source.len()) };
        }
        let id = buffer.id;
        let callback = self.new_object(Object::Frame);
        self.send(Outgoing::new(self.surface, 1).uint(id).int(0).int(0));
        self.send(Outgoing::new(self.surface, 9).int(damage.x).int(damage.y).int(damage.width).int(damage.height));
        self.send(Outgoing::new(self.surface, 3).uint(callback));
        self.send(Outgoing::new(self.surface, 6));
        self.frame_pending = true;
        let _ = self.connection.flush();
    }

    /// Makes another buffer of the window's size; returns its index.
    fn new_buffer(&mut self) -> Option<usize> {
        const XRGB8888: u32 = 1;
        let (width, height) = (self.width, self.height);
        let len = (width * height * 4) as usize;
        let fd = sys::shared_memory("ui-buffer", len).ok()?;
        let Ok(memory) = sys::map(fd, len, true) else {
            let _ = eli::io::close(fd);
            return None;
        };
        let pool = self.new_object(Object::Other);
        // The descriptor is closed once it's been sent.
        self.send(Outgoing::new(self.shm, 0).uint(pool).fd(fd).int(len as i32));
        let id = self.new_object(Object::Buffer);
        self.send(Outgoing::new(pool, 0).uint(id).int(0).int(width).int(height).int(width * 4).uint(XRGB8888));
        // The buffer keeps the memory; the pool isn't needed any more.
        self.send(Outgoing::new(pool, 1));
        self.buffers.push(Buffer {
            id,
            memory: memory.cast(),
            len,
            width,
            height,
            busy: false,
            stale: Rect::new(0, 0, width, height),
        });
        Some(self.buffers.len() - 1)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        for buffer in &mut self.buffers {
            buffer.unmap();
        }
    }
}
