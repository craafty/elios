//! settings: Wi-Fi, Bluetooth, the screen's brightness and the battery,
//! without commands.
//!
//! It drives the same kernel files the `wifi` and `bluetooth` commands do:
//! it reads `/proc/wifi` and `/proc/bluetooth` twice a second for what the
//! hardware is doing, and writes requests ("scan", "connect\nNAME\n...",
//! "pair ADDRESS") to them. The kernel does the slow work (scanning,
//! signing in, pairing) in the background, so the window never waits; the
//! next reading shows how it went.
//!
//! The Power page reads Linux's `/sys/class/backlight` and
//! `/sys/class/power_supply`, and writes the brightness there; the
//! brightness is also saved in `~/.config/brightness`, which the desktop
//! sets again when it starts.
//!
//! `settings bluetooth` opens on the Bluetooth page, `settings power` on
//! the Power page.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use ui::widgets::{self, ButtonState, FieldChange, List, ListChange, TextField, theme};
use ui::{Button, Canvas, Event, Font, Key, Rect, Window};

eli::main!(main);

const WIFI: &str = "/proc/wifi";
const BLUETOOTH: &str = "/proc/bluetooth";
const BACKLIGHT: &str = "/sys/class/backlight/intel_backlight";
const BATTERY: &str = "/sys/class/power_supply/BAT0";
const SIDEBAR_WIDTH: i32 = 170;
const MARGIN: i32 = 20;
const BUTTON_HEIGHT: i32 = 32;
/// How often the kernel's files are read again, in milliseconds.
const REFRESH_MS: i32 = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Wifi,
    Bluetooth,
    Power,
}

const PAGES: [(Page, &str); 3] = [(Page::Wifi, "Wi-Fi"), (Page::Bluetooth, "Bluetooth"), (Page::Power, "Power")];

/// What a button does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Page(Page),
    Scan,
    /// Join the selected network (asking for a password first if it has one).
    Connect,
    Disconnect,
    /// Forget the selected Wi-Fi network's saved password.
    ForgetNetwork,
    /// Send the password form.
    Join,
    CancelJoin,
    Pair,
    ConnectDevice,
    DisconnectDevice,
    Forget,
    Dimmer,
    Brighter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    User,
    Password,
}

/// A network from the last scan (the strongest access point of each name).
struct Network {
    name: String,
    signal: i32,
    /// "open", "WPA2" or "WPA2-EAP" (a user name and password).
    security: String,
}

impl Network {
    fn enterprise(&self) -> bool {
        self.security.ends_with("EAP")
    }

    fn open(&self) -> bool {
        self.security == "open"
    }

    fn supported(&self) -> bool {
        matches!(self.security.as_str(), "open" | "WPA2" | "WPA2-EAP")
    }
}

/// A Bluetooth device: paired, or found by the last scan.
struct Device {
    address: String,
    name: String,
    /// "connected, keyboard, 12 reports", or what kind of device it is.
    detail: String,
    paired: bool,
    connected: bool,
}

/// From /sys/class/power_supply/BAT0.
#[derive(PartialEq, Eq)]
struct Battery {
    percent: u32,
    status: String,
    /// What's left and full, and the power drawn or given, in mWh and mW
    /// (or mAh and mA: `unit` is "Wh" or "Ah").
    now: u64,
    full: u64,
    design: u64,
    rate: u64,
    unit: &'static str,
}

fn read_number(path: &str) -> Option<u64> {
    eli::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_battery() -> Option<Battery> {
    let file = |name: &str| format!("{BATTERY}/{name}");
    let percent = read_number(&file("capacity"))? as u32;
    let status = eli::fs::read_to_string(&file("status")).ok()?.trim().to_string();
    let (amount, flow, unit) =
        if eli::fs::exists(&file("energy_now")) { ("energy", "power", "Wh") } else { ("charge", "current", "Ah") };
    let value = |name: String| read_number(&file(&name)).unwrap_or(0) / 1000;
    Some(Battery {
        percent,
        status,
        now: value(format!("{amount}_now")),
        full: value(format!("{amount}_full")),
        design: value(format!("{amount}_full_design")),
        rate: value(format!("{flow}_now")),
        unit,
    })
}

fn read_brightness() -> Option<(u32, u32)> {
    let brightness = read_number(&format!("{BACKLIGHT}/brightness"))?;
    let most = read_number(&format!("{BACKLIGHT}/max_brightness"))?;
    (most > 0).then_some((brightness as u32, most as u32))
}

/// Thousandths as "12.3".
fn decimal(thousandths: u64) -> String {
    format!("{}.{}", thousandths / 1000, thousandths % 1000 / 100)
}

/// The password form for the network being joined.
struct Join {
    network: String,
    enterprise: bool,
}

struct Settings {
    font: Font,
    page: Page,
    list: List,
    focus: Focus,
    /// The last readings of /proc/wifi and /proc/bluetooth (`None` if the
    /// file isn't there: no card).
    wifi: Option<String>,
    bluetooth: Option<String>,
    networks: Vec<Network>,
    devices: Vec<Device>,
    /// The backlight's (brightness, most), and the battery's files.
    brightness: Option<(u32, u32)>,
    battery: Option<Battery>,
    join: Option<Join>,
    user: TextField,
    password: TextField,
    /// What the last request did, and whether that's a problem.
    message: String,
    message_is_error: bool,
    /// The buttons as last drawn, for clicks.
    buttons: Vec<(Rect, Action)>,
    hovered: Option<Action>,
    pressed: Option<Action>,
}

/// The value of the `name` line ("state:") of a /proc file.
fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| line.strip_prefix(name)).map(str::trim)
}

/// The lines after `heading` in /proc/bluetooth, up to the next heading.
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let Some((_, rest)) = text.split_once(heading) else { return Vec::new() };
    rest.lines().skip(1).take_while(|line| !line.ends_with(':') && !line.starts_with("pairing:")).collect()
}

/// What the kernel calls networks' security (`ieee80211.rs`), longest
/// first. Only "open", "WPA2" and "WPA2-EAP" can be joined.
const SECURITY_NAMES: [&str; 11] = [
    "protected management frames required",
    "WPA2 with a TKIP group key",
    "unknown cipher",
    "WPA2-EAP",
    "bad RSN",
    "WPA2",
    "WPA1",
    "WPA3",
    "TKIP",
    "open",
    "WEP",
];

/// The networks in /proc/wifi, whose lines read
/// `" -52 dBm  channel   6  WPA2      Name"`; strongest first.
fn parse_networks(text: &str) -> Vec<Network> {
    let mut networks: Vec<Network> = Vec::new();
    let Some((_, list)) = text.split_once("networks:\n") else { return networks };
    for line in list.lines() {
        let Some((signal, rest)) = line.trim_start().split_once(" dBm  channel ") else { continue };
        let Some((_, rest)) = rest.trim_start().split_once("  ") else { continue };
        // Longest first: some names have spaces, and "WPA2" starts others.
        let security = SECURITY_NAMES
            .iter()
            .copied()
            .find(|name| rest.strip_prefix(name).is_some_and(|after| after.starts_with(' ')))
            .unwrap_or_else(|| rest.split(' ').next().unwrap_or_default());
        // The security column is 8 wide, then two spaces.
        let name = rest.get(security.len().max(8) + 2..).unwrap_or_default();
        let signal = signal.trim().parse().unwrap_or(-100);
        if name.is_empty() {
            continue; // hidden networks
        }
        match networks.iter_mut().find(|network| network.name == name) {
            Some(known) if known.signal >= signal => {}
            Some(known) => known.signal = signal,
            None => networks.push(Network { name: name.to_string(), signal, security: security.to_string() }),
        }
    }
    networks.sort_by_key(|network| core::cmp::Reverse(network.signal));
    networks
}

/// What a Bluetooth class of device says the device is.
fn kind_of(class: u32) -> &'static str {
    match (class >> 8) & 0x1F {
        1 => "computer",
        2 => "phone",
        4 => match (class >> 2) & 0x3F {
            1 | 2 => "headset",
            6 => "headphones",
            5 => "speaker",
            _ => "audio",
        },
        5 => match (class >> 6) & 3 {
            1 => "keyboard",
            2 => "mouse",
            3 => "keyboard and mouse",
            _ => "input device",
        },
        _ => "device",
    }
}

/// The paired devices then the others found, from /proc/bluetooth.
fn parse_devices(text: &str) -> Vec<Device> {
    let mut devices = Vec::new();
    // `ADDRESS  NAME  STATE`
    for line in section(text, "\npaired:") {
        let Some((address, rest)) = line.split_once("  ") else { continue };
        let (name, state) = rest.rsplit_once("  ").unwrap_or((rest, ""));
        devices.push(Device {
            address: address.to_string(),
            name: name.to_string(),
            detail: state.to_string(),
            paired: true,
            connected: state.starts_with("connected"),
        });
    }
    // `ADDRESS  class CCCCCC[  R dBm]  NAME`
    for line in section(text, "\ndevices:") {
        let fields: Vec<&str> = line.split("  ").collect();
        let (Some(&address), Some(&name)) = (fields.first(), fields.last()) else { continue };
        if fields.len() < 3 || devices.iter().any(|device: &Device| device.address == address) {
            continue;
        }
        let class = fields[1].strip_prefix("class ").and_then(|class| u32::from_str_radix(class, 16).ok());
        devices.push(Device {
            address: address.to_string(),
            name: name.to_string(),
            detail: kind_of(class.unwrap_or(0)).to_string(),
            paired: false,
            connected: false,
        });
    }
    devices
}

/// Writes `request` to a /proc control file; the error to show if it
/// was refused.
fn send(path: &str, request: &str) -> Result<(), String> {
    let written = eli::fs::File::open_read_write(path).and_then(|file| file.write_all(request.as_bytes()));
    written.map_err(|error| match error {
        eli::sys::Error::BUSY => String::from("Busy: wait for the scan or pairing to finish."),
        eli::sys::Error(19) => String::from("The adapter isn't ready yet."),
        eli::sys::Error::ACCESS | eli::sys::Error::NOT_PERMITTED => {
            String::from("Not allowed (you need the netdev group).")
        }
        eli::sys::Error::INVALID => String::from("The kernel refused that request."),
        error => format!("{error}"),
    })
}

/// Signal strength as 1 to 4 bars.
fn bars(signal: i32) -> i32 {
    match signal {
        -55.. => 4,
        -65.. => 3,
        -75.. => 2,
        _ => 1,
    }
}

fn draw_bars(canvas: &mut Canvas, rect: Rect, count: i32) {
    for bar in 0..4 {
        let height = rect.height * (bar + 1) / 4;
        let x = rect.x + bar * (rect.width / 4);
        let color = if bar < count { theme::TEXT } else { theme::BORDER };
        canvas.fill(Rect::new(x, rect.bottom() - height, rect.width / 4 - 2, height), color);
    }
}

impl Settings {
    fn content_x(&self) -> i32 {
        SIDEBAR_WIDTH + MARGIN
    }

    /// Reads the kernel's files again; whether anything changed.
    fn refresh(&mut self) -> bool {
        let wifi = eli::fs::read_to_string(WIFI).ok();
        let bluetooth = eli::fs::read_to_string(BLUETOOTH).ok();
        let (brightness, battery) = (read_brightness(), read_battery());
        let power_changed = brightness != self.brightness || battery != self.battery;
        self.brightness = brightness;
        self.battery = battery;
        if wifi == self.wifi && bluetooth == self.bluetooth {
            return power_changed;
        }
        // A note like "Looking for devices..." is over when the state moves on.
        let state = |text: &Option<String>| {
            let text = text.as_deref().unwrap_or_default();
            (field(text, "state:").map(String::from), field(text, "pairing:").is_some())
        };
        if (state(&wifi) != state(&self.wifi) || state(&bluetooth) != state(&self.bluetooth)) && !self.message_is_error
        {
            self.message.clear();
        }
        let selected = self.selected_key();
        self.networks = wifi.as_deref().map(parse_networks).unwrap_or_default();
        self.devices = bluetooth.as_deref().map(parse_devices).unwrap_or_default();
        self.wifi = wifi;
        self.bluetooth = bluetooth;
        self.update_list(selected);
        true
    }

    /// The selected row's network name or device address, to find it
    /// again after the list changes.
    fn selected_key(&self) -> Option<String> {
        let index = self.list.selected?;
        match self.page {
            Page::Wifi => self.networks.get(index).map(|network| network.name.clone()),
            Page::Bluetooth => self.devices.get(index).map(|device| device.address.clone()),
            Page::Power => None,
        }
    }

    fn update_list(&mut self, selected: Option<String>) {
        let len = match self.page {
            Page::Wifi => self.networks.len(),
            Page::Bluetooth => self.devices.len(),
            Page::Power => 0,
        };
        self.list.len = len;
        self.list.selected = selected.and_then(|key| match self.page {
            Page::Wifi => self.networks.iter().position(|network| network.name == key),
            Page::Bluetooth => self.devices.iter().position(|device| device.address == key),
            Page::Power => None,
        });
        self.list.top = self.list.top.min(len.saturating_sub(1));
    }

    fn show_page(&mut self, page: Page) {
        if self.page != page {
            self.page = page;
            self.join = None;
            self.focus = Focus::List;
            self.message.clear();
            self.list.reset(0);
            self.update_list(None);
        }
    }

    fn report(&mut self, result: Result<(), String>, done: &str) {
        match result {
            Ok(()) => {
                self.message = String::from(done);
                self.message_is_error = false;
            }
            Err(error) => {
                self.message = error;
                self.message_is_error = true;
            }
        }
    }

    /// Whether the kernel has a password saved for a Wi-Fi network.
    fn saved(&self, name: &str) -> bool {
        let saved = self.wifi.as_deref().and_then(|text| field(text, "saved:")).unwrap_or_default();
        saved.split(", ").any(|saved| saved == name)
    }

    fn selected_network(&self) -> Option<&Network> {
        self.networks.get(self.list.selected?)
    }

    fn selected_device(&self) -> Option<&Device> {
        self.devices.get(self.list.selected?)
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Page(page) => self.show_page(page),
            Action::Scan if self.page == Page::Wifi => {
                let result = send(WIFI, "scan");
                self.report(result, "Looking for networks...");
            }
            Action::Scan => {
                let result = send(BLUETOOTH, "scan");
                self.report(result, "Looking for devices (about 10 seconds)...");
            }
            Action::Connect => {
                let Some(network) = self.selected_network() else { return };
                let (name, open, enterprise) = (network.name.clone(), network.open(), network.enterprise());
                if !network.supported() {
                    let problem = format!("{name} uses {}, which eliOS can't join yet.", network.security);
                    self.report(Err(problem), "");
                } else if open || self.saved(&name) {
                    // A saved network's password is the kernel's to fill in.
                    let result = send(WIFI, &format!("connect\n{name}"));
                    self.report(result, &format!("Joining {name}..."));
                } else {
                    self.join = Some(Join { network: name, enterprise });
                    self.password.set_text("");
                    self.focus = if enterprise { Focus::User } else { Focus::Password };
                    self.message.clear();
                }
            }
            Action::Join => self.join(),
            Action::CancelJoin => {
                self.join = None;
                self.focus = Focus::List;
            }
            Action::Disconnect => {
                let result = send(WIFI, "disconnect");
                self.report(result, "Disconnected.");
            }
            Action::ForgetNetwork => {
                let Some(network) = self.selected_network() else { return };
                let name = network.name.clone();
                let result = send(WIFI, &format!("forget\n{name}"));
                self.report(result, &format!("Forgot {name}."));
            }
            Action::Dimmer | Action::Brighter => {
                let Some((brightness, most)) = self.brightness else { return };
                // Tenths of the range, landing on whole tenths.
                let step = most.div_ceil(10).max(1);
                let tenth = brightness.div_ceil(step);
                let wanted =
                    if action == Action::Brighter { (tenth + 1) * step } else { tenth.saturating_sub(1) * step };
                let wanted = wanted.min(most);
                match eli::fs::write(&format!("{BACKLIGHT}/brightness"), format!("{wanted}").as_bytes()) {
                    Ok(()) => {
                        self.message.clear();
                        if let Some(home) = eli::env::var("HOME") {
                            let _ = eli::fs::create_dir(&format!("{home}/.config"));
                            let percent = read_brightness().map_or(0, |(now, most)| now * 100 / most);
                            let _ = eli::fs::write(
                                &format!("{home}/.config/brightness"),
                                format!("{percent}\n").as_bytes(),
                            );
                        }
                    }
                    Err(error) => self.report(Err(format!("Couldn't change the brightness: {error}")), ""),
                }
            }
            Action::Pair | Action::ConnectDevice | Action::DisconnectDevice | Action::Forget => {
                let Some(device) = self.selected_device() else { return };
                let (verb, done) = match action {
                    Action::Pair => ("pair", "Pairing with"),
                    Action::ConnectDevice => ("connect", "Connecting to"),
                    Action::DisconnectDevice => ("disconnect", "Disconnecting"),
                    _ => ("forget", "Forgot"),
                };
                let request = format!("{verb} {}", device.address);
                let done = format!("{done} {}.", device.name);
                let result = send(BLUETOOTH, &request);
                self.report(result, &done);
            }
        }
    }

    /// Sends the password form.
    fn join(&mut self) {
        let Some(join) = &self.join else { return };
        let password = self.password.text();
        let user = self.user.text().trim();
        let problem = if join.enterprise && user.is_empty() {
            Some("Type your user name.")
        } else if join.enterprise && password.is_empty() {
            Some("Type your password.")
        } else if !join.enterprise && !(8..=63).contains(&password.len()) {
            Some("A WPA2 password has 8 to 63 characters.")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.report(Err(String::from(problem)), "");
            return;
        }
        let mut request = format!("connect\n{}\n{password}", join.network);
        if join.enterprise {
            request.push('\n');
            request.push_str(user);
        }
        let done = format!("Joining {}...", join.network);
        let result = send(WIFI, &request);
        if result.is_ok() {
            self.join = None;
            self.password.set_text("");
            self.focus = Focus::List;
        }
        self.report(result, &done);
    }

    fn list_rect(&self, width: i32, height: i32) -> Rect {
        let x = self.content_x();
        let top = 150;
        Rect::new(x, top, width - x - MARGIN, (height - top - self.bottom_height()).max(40))
    }

    /// The room under the list for the selected row's buttons, or the
    /// password form.
    fn bottom_height(&self) -> i32 {
        match &self.join {
            Some(join) if join.enterprise => 190,
            Some(_) => 140,
            None => 80,
        }
    }

    /// The text fields of the password form: (user, password).
    fn field_rects(&self, width: i32, height: i32) -> (Option<Rect>, Rect) {
        let list = self.list_rect(width, height);
        let x = self.content_x() + 100;
        let field_width = (list.right() - x).min(360);
        let mut y = list.bottom() + 44;
        let user = self.join.as_ref().filter(|join| join.enterprise).map(|_| {
            let rect = Rect::new(x, y, field_width, BUTTON_HEIGHT);
            y += BUTTON_HEIGHT + 10;
            rect
        });
        (user, Rect::new(x, y, field_width, BUTTON_HEIGHT))
    }

    /// Draws a button at (x, y), remembering it for clicks; its width.
    fn button(&mut self, canvas: &mut Canvas, x: i32, y: i32, label: &str, action: Action, default: bool) -> i32 {
        let width = widgets::button_width(&mut self.font, label);
        let rect = Rect::new(x, y, width, BUTTON_HEIGHT);
        let state =
            ButtonState { hovered: self.hovered == Some(action), pressed: self.pressed == Some(action), default };
        widgets::button(canvas, &mut self.font, rect, label, state);
        self.buttons.push((rect, action));
        width
    }

    /// A row of buttons from (x, y), 10 pixels apart.
    fn buttons_row(&mut self, canvas: &mut Canvas, x: i32, y: i32, buttons: &[(&str, Action)]) {
        let mut x = x;
        for (index, &(label, action)) in buttons.iter().enumerate() {
            x += self.button(canvas, x, y, label, action, index == 0) + 10;
        }
    }

    fn draw(&mut self, canvas: &mut Canvas) {
        let (width, height) = (canvas.width, canvas.height);
        self.buttons.clear();
        canvas.fill(canvas.bounds(), theme::BACKGROUND);

        // The pages, down the left.
        canvas.fill(Rect::new(0, 0, SIDEBAR_WIDTH, height), theme::PANEL);
        canvas.fill(Rect::new(SIDEBAR_WIDTH, 0, 1, height), theme::BORDER);
        let row_height = self.font.line_height + 16;
        for (index, (page, label)) in PAGES.iter().enumerate() {
            let rect = Rect::new(8, 12 + index as i32 * (row_height + 4), SIDEBAR_WIDTH - 16, row_height);
            let action = Action::Page(*page);
            if *page == self.page {
                canvas.rounded(rect, 4.0, theme::SELECTION);
            } else if self.hovered == Some(action) {
                canvas.rounded(rect, 4.0, theme::BUTTON_HOVER);
            }
            let y = rect.y + (rect.height - self.font.line_height) / 2;
            canvas.text(&mut self.font, rect.x + 12, y, label, theme::TEXT);
            self.buttons.push((rect, action));
        }

        let x = self.content_x();
        let title = PAGES.iter().find(|(page, _)| *page == self.page).map_or("", |(_, label)| label);
        canvas.text(&mut self.font, x, MARGIN, title, theme::TEXT);
        let (status, error, present) = self.status();
        canvas.text(&mut self.font, x, 50, &status, theme::DIM_TEXT);
        let (note, color) = match error {
            Some(error) => (error, theme::DANGER),
            None if self.message_is_error => (self.message.clone(), theme::DANGER),
            None => (self.message.clone(), theme::DIM_TEXT),
        };
        let note = self.font.trim(&note, width - x - MARGIN);
        canvas.text(&mut self.font, x, 76, &note, color);
        if !present {
            return;
        }
        match self.page {
            Page::Wifi => {
                let connected = self.wifi.as_deref().and_then(|text| field(text, "network:")).is_some();
                if connected {
                    self.buttons_row(canvas, x, 104, &[("Scan", Action::Scan), ("Disconnect", Action::Disconnect)]);
                } else {
                    self.buttons_row(canvas, x, 104, &[("Scan", Action::Scan)]);
                }
            }
            Page::Bluetooth => self.buttons_row(canvas, x, 104, &[("Scan", Action::Scan)]),
            Page::Power => {
                self.draw_power(canvas, x, 104);
                return;
            }
        }

        let list_rect = self.list_rect(width, height);
        canvas.fill(
            Rect::new(list_rect.x - 1, list_rect.y - 1, list_rect.width + 2, list_rect.height + 2),
            theme::BORDER,
        );
        let focused = self.focus == Focus::List;
        let font = &mut self.font;
        match self.page {
            Page::Wifi => {
                let networks = &self.networks;
                let current = self.wifi.as_deref().and_then(|text| field(text, "network:"));
                let saved: Vec<&str> = self
                    .wifi
                    .as_deref()
                    .and_then(|text| field(text, "saved:"))
                    .map(|names| names.split(", ").collect())
                    .unwrap_or_default();
                self.list.draw(canvas, list_rect, focused, |canvas, index, rect, _| {
                    let network = &networks[index];
                    let y = rect.y + (rect.height - font.line_height) / 2;
                    draw_bars(canvas, Rect::new(rect.x + 10, rect.y + 8, 20, rect.height - 16), bars(network.signal));
                    canvas.text(font, rect.x + 42, y, &network.name, theme::TEXT);
                    let security = match network.security.as_str() {
                        "open" => "open",
                        "WPA2-EAP" => "user name and password",
                        "WPA2" => "password",
                        _ => "not supported",
                    };
                    let mut right = rect.right() - 12;
                    right -= font.width(security);
                    canvas.text(font, right, y, security, theme::DIM_TEXT);
                    if current == Some(network.name.as_str()) {
                        right -= font.width("connected") + 20;
                        canvas.text(font, right, y, "connected", theme::TEXT);
                    } else if saved.contains(&network.name.as_str()) {
                        right -= font.width("saved") + 20;
                        canvas.text(font, right, y, "saved", theme::DIM_TEXT);
                    }
                });
            }
            Page::Bluetooth => {
                let devices = &self.devices;
                self.list.draw(canvas, list_rect, focused, |canvas, index, rect, _| {
                    let device = &devices[index];
                    let y = rect.y + (rect.height - font.line_height) / 2;
                    canvas.text(font, rect.x + 12, y, &device.name, theme::TEXT);
                    let detail =
                        if device.paired { device.detail.clone() } else { format!("{}, not paired", device.detail) };
                    let room = rect.width / 2;
                    let detail = font.trim(&detail, room);
                    let color = if device.connected { theme::TEXT } else { theme::DIM_TEXT };
                    let detail_x = rect.right() - 12 - font.width(&detail);
                    canvas.text(font, detail_x, y, &detail, color);
                });
            }
            Page::Power => {}
        }
        if self.list.len == 0 {
            let empty = match self.page {
                Page::Wifi => "No networks yet: press Scan.",
                Page::Bluetooth => "No devices yet: press Scan with the device in pairing mode.",
                Page::Power => "",
            };
            canvas.text(&mut self.font, list_rect.x + 12, list_rect.y + 12, empty, theme::DIM_TEXT);
        }

        let y = list_rect.bottom() + 16;
        if let Some(join) = &self.join {
            let heading = format!("Join {}", join.network);
            canvas.text(&mut self.font, x, y + 4, &heading, theme::TEXT);
            let (user_rect, password_rect) = self.field_rects(width, height);
            let line_height = self.font.line_height;
            let label_y = |rect: Rect| rect.y + (rect.height - line_height) / 2;
            if let Some(rect) = user_rect {
                let label_y = label_y(rect);
                canvas.text(&mut self.font, x, label_y, "User name", theme::DIM_TEXT);
                self.user.draw(canvas, &mut self.font, rect, self.focus == Focus::User);
            }
            let label_y = label_y(password_rect);
            canvas.text(&mut self.font, x, label_y, "Password", theme::DIM_TEXT);
            self.password.draw(canvas, &mut self.font, password_rect, self.focus == Focus::Password);
            let buttons_y = password_rect.bottom() + 12;
            self.buttons_row(canvas, x + 100, buttons_y, &[("Join", Action::Join), ("Cancel", Action::CancelJoin)]);
            return;
        }
        match self.page {
            Page::Wifi => {
                let Some(network) = self.selected_network() else { return };
                if self.saved(&network.name.clone()) {
                    self.buttons_row(canvas, x, y, &[("Connect", Action::Connect), ("Forget", Action::ForgetNetwork)]);
                } else {
                    self.buttons_row(canvas, x, y, &[("Connect", Action::Connect)]);
                }
            }
            Page::Bluetooth => {
                let passkey = self.bluetooth.as_deref().and_then(|text| field(text, "pairing:"));
                if let Some(passkey) = passkey.and_then(|pairing| pairing.split_once("passkey ")) {
                    let note = format!("On the keyboard, type {} and press Enter.", passkey.1);
                    canvas.text(&mut self.font, x, y + 6, &note, theme::TEXT);
                    return;
                }
                let Some(device) = self.selected_device() else { return };
                match (device.paired, device.connected) {
                    (false, _) => self.buttons_row(canvas, x, y, &[("Pair", Action::Pair)]),
                    (true, false) => self.buttons_row(
                        canvas,
                        x,
                        y,
                        &[("Connect", Action::ConnectDevice), ("Forget", Action::Forget)],
                    ),
                    (true, true) => self.buttons_row(
                        canvas,
                        x,
                        y,
                        &[("Disconnect", Action::DisconnectDevice), ("Forget", Action::Forget)],
                    ),
                }
            }
            Page::Power => {}
        }
    }

    /// The Power page under its status line: brightness and the battery.
    fn draw_power(&mut self, canvas: &mut Canvas, x: i32, y: i32) {
        let mut y = y;
        let line = self.font.line_height + 8;
        match self.brightness {
            Some((brightness, most)) => {
                let label = format!("Screen brightness: {}%", (brightness * 100 + most / 2) / most);
                canvas.text(&mut self.font, x, y + 6, &label, theme::TEXT);
                let buttons_x = x + self.font.width("Screen brightness: 100%") + 24;
                self.buttons_row(canvas, buttons_x, y, &[("Dimmer", Action::Dimmer), ("Brighter", Action::Brighter)]);
                y += BUTTON_HEIGHT + 20;
            }
            None => {
                canvas.text(&mut self.font, x, y, "The screen's brightness can't be changed here.", theme::DIM_TEXT);
                y += line + 12;
            }
        }
        let Some(battery) = &self.battery else {
            canvas.text(&mut self.font, x, y, "No battery found.", theme::DIM_TEXT);
            return;
        };
        let mut lines = Vec::new();
        lines.push(format!("Battery: {}%, {}", battery.percent, battery.status.to_lowercase()));
        let power = if battery.unit == "Wh" { "W" } else { "A" };
        if battery.rate > 0 && battery.status == "Discharging" {
            let minutes = battery.now * 60 / battery.rate;
            lines.push(format!(
                "Using {} {power}: about {}:{:02} left",
                decimal(battery.rate),
                minutes / 60,
                minutes % 60
            ));
        } else if battery.rate > 0 && battery.status == "Charging" {
            let minutes = battery.full.saturating_sub(battery.now) * 60 / battery.rate;
            lines.push(format!(
                "Charging at {} {power}: full in about {}:{:02}",
                decimal(battery.rate),
                minutes / 60,
                minutes % 60
            ));
        }
        let unit = battery.unit;
        lines.push(format!("Charge: {} of {} {unit}", decimal(battery.now), decimal(battery.full)));
        if let Some(health) = (battery.full * 100).checked_div(battery.design) {
            lines.push(format!("Health: {health}% of the {} {unit} it was made for", decimal(battery.design)));
        }
        for (index, text) in lines.iter().enumerate() {
            let color = if index == 0 { theme::TEXT } else { theme::DIM_TEXT };
            canvas.text(&mut self.font, x, y, text, color);
            y += line;
        }
    }

    /// The page's status line, the hardware's error if any, and whether
    /// there's hardware at all.
    fn status(&self) -> (String, Option<String>, bool) {
        let (text, absent) = match self.page {
            Page::Wifi => (self.wifi.as_deref(), "No Wi-Fi card found."),
            Page::Bluetooth => (self.bluetooth.as_deref(), "No Bluetooth adapter found."),
            Page::Power => {
                let status = match &self.battery {
                    Some(battery) if battery.status == "Discharging" => String::from("On battery"),
                    Some(_) => String::from("Plugged in"),
                    None => String::new(),
                };
                return (status, None, true);
            }
        };
        let Some(text) = text else { return (String::from(absent), None, false) };
        let state = field(text, "state:").unwrap_or_default();
        let error = field(text, "error:").map(String::from);
        let status = match self.page {
            Page::Power => String::new(),
            Page::Wifi => match (state, field(text, "network:")) {
                ("connected", Some(network)) => format!("Connected to {network}"),
                ("scanning", _) => String::from("Scanning..."),
                ("starting" | "working", _) => String::from("Working..."),
                ("failed", _) => String::from("The Wi-Fi card failed to start."),
                _ => String::from("Not connected"),
            },
            Page::Bluetooth => match state {
                "absent" => return (String::from(absent), error, false),
                "starting" | "patching" => String::from("Starting..."),
                "scanning" => String::from("Scanning..."),
                "failed" => String::from("The Bluetooth adapter failed to start."),
                _ => {
                    let connected = self.devices.iter().filter(|device| device.connected).count();
                    match connected {
                        0 => String::from("On"),
                        1 => String::from("On, 1 device connected"),
                        count => format!("On, {count} devices connected"),
                    }
                }
            },
        };
        (status, error, true)
    }

    fn button_at(&self, x: i32, y: i32) -> Option<Action> {
        self.buttons.iter().find(|(rect, _)| rect.contains(x, y)).map(|&(_, action)| action)
    }

    /// A key; whether to redraw.
    fn key(&mut self, key: Key, modifiers: ui::keys::Modifiers, list_rect: Rect) -> bool {
        if key == Key::Tab {
            let enterprise = self.join.as_ref().is_some_and(|join| join.enterprise);
            self.focus = match (self.focus, self.join.is_some()) {
                (_, false) => Focus::List,
                (Focus::List, true) if enterprise => Focus::User,
                (Focus::List | Focus::User, true) => Focus::Password,
                (Focus::Password, true) => Focus::List,
            };
            return true;
        }
        let field = match self.focus {
            Focus::List => None,
            Focus::User => Some(&mut self.user),
            Focus::Password => Some(&mut self.password),
        };
        if let Some(field) = field {
            match field.key(key, modifiers) {
                FieldChange::Submit if self.focus == Focus::User => self.focus = Focus::Password,
                FieldChange::Submit => self.join(),
                FieldChange::Cancel => self.act(Action::CancelJoin),
                FieldChange::Ignored => return false,
                _ => {}
            }
            return true;
        }
        match self.list.key(key, list_rect) {
            ListChange::Activated(_) => self.activate(),
            ListChange::Moved => {}
            ListChange::Ignored => return false,
        }
        true
    }

    /// Enter or a double click on a row.
    fn activate(&mut self) {
        match self.page {
            Page::Wifi => self.act(Action::Connect),
            Page::Bluetooth => match self.selected_device().map(|device| (device.paired, device.connected)) {
                Some((false, _)) => self.act(Action::Pair),
                Some((true, false)) => self.act(Action::ConnectDevice),
                _ => {}
            },
            Page::Power => {}
        }
    }
}

fn main() -> i32 {
    let font = match Font::load(15.0) {
        Ok(font) => font,
        Err(error) => {
            eli::eprintln!("settings: {error}");
            return 1;
        }
    };
    let mut window = match Window::open("Settings", "settings", 820, 560) {
        Ok(window) => window,
        Err(error) => {
            eli::eprintln!("settings: {error}");
            return 1;
        }
    };
    let row_height = font.line_height + 16;
    let mut user = TextField::new("");
    user.placeholder = String::from("for example, your university login");
    let mut password = TextField::new("");
    password.secret = true;
    let page = match eli::env::args().get(1).map(String::as_str) {
        Some("bluetooth") => Page::Bluetooth,
        Some("power") => Page::Power,
        _ => Page::Wifi,
    };
    let mut settings = Settings {
        font,
        page,
        list: List::new(row_height),
        focus: Focus::List,
        wifi: None,
        bluetooth: None,
        networks: Vec::new(),
        devices: Vec::new(),
        brightness: None,
        battery: None,
        join: None,
        user,
        password,
        message: String::new(),
        message_is_error: false,
        buttons: Vec::new(),
        hovered: None,
        pressed: None,
    };
    settings.refresh();
    loop {
        let events = window.wait(&[], REFRESH_MS);
        let mut redraw = events.is_empty();
        if events.is_empty() {
            redraw = settings.refresh();
        }
        for event in events {
            let (width, height) = (window.width, window.height);
            let list_rect = settings.list_rect(width, height);
            redraw = true;
            match event {
                Event::Close => return 0,
                Event::Resize { .. } | Event::Focus(_) => {}
                Event::Key { key, modifiers } => redraw = settings.key(key, modifiers, list_rect),
                Event::PointerMove { x, y } => {
                    let hovered = settings.button_at(x, y);
                    redraw = settings.list.drag(list_rect, y) || hovered != settings.hovered;
                    settings.hovered = hovered;
                }
                Event::PointerLeave => settings.hovered = None,
                Event::Button { button: Button::Left, pressed: true, x, y, clicks } => {
                    let (user_rect, password_rect) = settings.field_rects(width, height);
                    if let Some(action) = settings.button_at(x, y) {
                        settings.pressed = Some(action);
                    } else if settings.join.is_some() && password_rect.contains(x, y) {
                        settings.focus = Focus::Password;
                        settings.password.click(&mut settings.font, password_rect, x);
                    } else if let Some(rect) = user_rect.filter(|rect| settings.join.is_some() && rect.contains(x, y)) {
                        settings.focus = Focus::User;
                        settings.user.click(&mut settings.font, rect, x);
                    } else if list_rect.contains(x, y) {
                        settings.focus = Focus::List;
                        if let ListChange::Activated(_) = settings.list.press(list_rect, x, y, clicks) {
                            settings.activate();
                        }
                    }
                }
                Event::Button { button: Button::Left, pressed: false, x, y, .. } => {
                    settings.list.release();
                    if let Some(action) =
                        settings.pressed.take().filter(|&action| settings.button_at(x, y) == Some(action))
                    {
                        settings.act(action);
                        settings.refresh();
                    }
                }
                Event::Button { .. } => redraw = false,
                Event::Scroll { lines } => redraw = settings.list.scroll(lines * 3, list_rect),
                Event::Readable(_) => redraw = false,
            }
        }
        if redraw {
            let mut canvas = window.canvas();
            settings.draw(&mut canvas);
            window.present_all();
        }
    }
}
