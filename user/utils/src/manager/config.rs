//! `/etc/system.conf`: what this computer should be.
//!
//! ```text
//! hostname = elios
//! packages = git vim
//!
//! user alice
//!     name = Alice Example
//!     admin = yes
//! ```
//!
//! `name = value` lines; `user NAME` starts an account, and the indented
//! lines after it describe it. `#` starts a comment. Commands that change
//! the file (`eli add`, `adduser`...) edit just the lines concerned, so
//! comments and layout stay as they were.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub full_name: String,
    /// May use sudo (a member of `wheel`).
    pub admin: bool,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub hostname: String,
    pub packages: Vec<String>,
    /// Package repositories; empty means the defaults in
    /// /etc/eli/repositories.
    pub repositories: Vec<String>,
    pub users: Vec<User>,
    /// An account to log in without asking at startup (a kiosk, say).
    pub autologin: Option<String>,
}

/// Parses a configuration. Errors say which line is wrong and why.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config {
        hostname: "elios".into(),
        packages: Vec::new(),
        repositories: Vec::new(),
        users: Vec::new(),
        autologin: None,
    };
    for (number, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        let line = line.trim();
        let problem = |what: &str| format!("system.conf line {}: {what}", number + 1);
        if let Some(name) = line.strip_prefix("user ") {
            let name = name.trim();
            if !eli::users::valid_name(name) || name == "root" {
                return Err(problem(&format!("{name:?} can't be a user name")));
            }
            if config.users.iter().any(|user| user.name == name) {
                return Err(problem(&format!("user {name} is listed twice")));
            }
            config.users.push(User {
                name: name.into(),
                full_name: String::new(),
                admin: false,
                shell: "/bin/sh".into(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(problem("expected `name = value` or `user NAME`"));
        };
        let (key, value) = (key.trim(), value.trim());
        if indented && let Some(user) = config.users.last_mut() {
            match key {
                "name" => user.full_name = value.into(),
                "admin" => user.admin = matches!(value, "yes" | "true" | "1"),
                "shell" => user.shell = value.into(),
                _ => return Err(problem(&format!("users have no setting {key:?} (try name, admin, shell)"))),
            }
            continue;
        }
        match key {
            "hostname" => {
                let valid = !value.is_empty()
                    && value.len() <= 63
                    && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                if !valid {
                    return Err(problem(&format!("{value:?} can't be a computer name")));
                }
                config.hostname = value.into();
            }
            "packages" => config.packages = value.split_whitespace().map(String::from).collect(),
            "repositories" => config.repositories = value.split_whitespace().map(String::from).collect(),
            "autologin" => config.autologin = (!value.is_empty()).then(|| value.into()),
            _ => {
                return Err(problem(&format!(
                    "unknown setting {key:?} (try hostname, packages, repositories, autologin)"
                )));
            }
        }
    }
    Ok(config)
}

/// Sets a top-level `key = value` line, adding it if there isn't one.
pub fn set(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let found = lines.iter().position(|line| {
        !line.starts_with([' ', '\t'])
            && line.split('=').next().is_some_and(|name| name.trim() == key)
            && line.contains('=')
    });
    let new = if value.is_empty() { format!("{key} =") } else { format!("{key} = {value}") };
    match found {
        Some(index) => lines[index] = new,
        None => {
            // Before the first user block, so it isn't taken for one of
            // its settings.
            let at = lines.iter().position(|line| line.starts_with("user ")).unwrap_or(lines.len());
            lines.insert(at, new);
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Adds an account block at the end.
pub fn add_user(text: &str, user: &User) -> String {
    let mut text = text.trim_end().to_string();
    text.push_str(&format!("\n\nuser {}\n", user.name));
    if !user.full_name.is_empty() {
        text.push_str(&format!("    name = {}\n", user.full_name));
    }
    if user.admin {
        text.push_str("    admin = yes\n");
    }
    if user.shell != "/bin/sh" {
        text.push_str(&format!("    shell = {}\n", user.shell));
    }
    text
}

/// Removes an account block (its `user` line and the indented lines after
/// it).
pub fn remove_user(text: &str, name: &str) -> String {
    let mut output = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == format!("user {name}") {
            skipping = true;
            continue;
        }
        if skipping && (line.starts_with([' ', '\t']) || line.trim().is_empty()) {
            if line.trim().is_empty() {
                skipping = false;
            }
            continue;
        }
        skipping = false;
        output.push(line);
    }
    let mut text = output.join("\n");
    text.push('\n');
    text
}
