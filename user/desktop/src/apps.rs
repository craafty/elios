//! The programs the app menu offers: one per `.desktop` file in
//! `/usr/share/applications` (the freedesktop.org format, which Alpine's
//! packages install, so Firefox shows up once it's added), read again each
//! time the menu opens.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

const APPLICATIONS: &str = "/usr/share/applications";

pub struct App {
    pub name: String,
    /// The program and its arguments, from `Exec=` (field codes like `%U`
    /// dropped: we never pass files).
    pub command: Vec<String>,
    /// `Terminal=true`: runs inside `term`.
    pub terminal: bool,
}

/// Every app to list, sorted by name.
pub fn list() -> Vec<App> {
    let mut apps: Vec<App> = eli::fs::read_dir(APPLICATIONS)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.name.ends_with(".desktop"))
        .filter_map(|entry| {
            let text = eli::fs::read(&alloc::format!("{APPLICATIONS}/{}", entry.name)).ok()?;
            parse(&String::from_utf8_lossy(&text))
        })
        .collect();
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps.dedup_by(|a, b| a.name == b.name);
    apps
}

/// The `[Desktop Entry]` group of a `.desktop` file, if it's an app to
/// show (not hidden, and its program is installed).
fn parse(text: &str) -> Option<App> {
    let mut in_entry = false;
    let (mut name, mut exec, mut terminal, mut shown) = (None, None, false, true);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if !in_entry {
            continue;
        }
        match key.trim() {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            "Terminal" => terminal = value.trim() == "true",
            "NoDisplay" | "Hidden" => shown &= value.trim() != "true",
            "Type" => shown &= value.trim() == "Application",
            _ => {}
        }
    }
    let command = split_exec(&exec?);
    let program = command.first()?;
    if !shown || eli::process::find_program(program).is_none() {
        return None;
    }
    Some(App { name: name?, command, terminal })
}

/// Splits an `Exec=` line into words: double quotes group, backslashes
/// escape, `%%` is a percent sign and other field codes are dropped.
fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            // A field code (%f, %U...) stands for files we don't pass.
            '%' => {
                if chars.next() == Some('%') {
                    word.push('%');
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started || !word.is_empty() {
                    words.push(core::mem::take(&mut word));
                }
                started = false;
            }
            c => word.push(c),
        }
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    words
}

/// The bar's status area: the network, then the battery if there is one.
pub fn status() -> String {
    let network = network_status();
    match battery_status() {
        Some(battery) => alloc::format!("{network}   {battery}"),
        None => network,
    }
}

/// "Battery 83%", "Charging 83%" or "Plugged in 100%", from Linux's
/// `/sys/class/power_supply/BAT0`.
fn battery_status() -> Option<String> {
    let read = |name: &str| {
        let text = eli::fs::read(&alloc::format!("/sys/class/power_supply/BAT0/{name}")).ok()?;
        Some(String::from_utf8_lossy(&text).trim().to_string())
    };
    let capacity = read("capacity")?;
    let label = match read("status")?.as_str() {
        "Charging" => "Charging",
        "Discharging" => "Battery",
        _ => "Plugged in",
    };
    Some(alloc::format!("{label} {capacity}%"))
}

/// The network status: the Wi-Fi network's name, "Wired", or "Offline",
/// from the default route (`/proc/net/route`) and `/proc/wifi`.
fn network_status() -> String {
    let routes = eli::fs::read("/proc/net/route").unwrap_or_default();
    let routes = String::from_utf8_lossy(&routes);
    let default = routes.lines().skip(1).find_map(|line| {
        let mut fields = line.split('\t');
        let interface = fields.next()?;
        (fields.next()? == "00000000").then(|| interface.to_string())
    });
    let Some(interface) = default else { return String::from("Offline") };
    if interface.starts_with("wlan") {
        let wifi = eli::fs::read("/proc/wifi").unwrap_or_default();
        let wifi = String::from_utf8_lossy(&wifi);
        if let Some(network) = wifi.lines().find_map(|line| line.strip_prefix("network: ")) {
            return alloc::format!("Wi-Fi: {network}");
        }
        return String::from("Wi-Fi");
    }
    String::from("Wired")
}
