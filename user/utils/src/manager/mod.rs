//! `eli`: eliOS's system manager. The computer is described by one file,
//! /etc/system.conf (its name, accounts and packages); `eli apply` makes
//! the system match it, as a new *generation*, and `eli rollback` returns
//! to an earlier one. Packages come from Alpine Linux's repositories,
//! checked against Alpine's signatures (see the `apk` crate).
//!
//! Everything that should last lives under /data: the data partition on an
//! installed system, or memory on the live one.
//!
//! - /data/etc/system.conf, shadow (passwords), ids (user ids)
//! - /data/home: everyone's home (mounted on /home)
//! - /data/pkg/NAME-VERSION: the package store
//! - /data/generations/N: each generation; `current` points at one
//! - /data/cache: the repositories' package lists

mod config;
mod generations;
mod store;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::{eprint, eprintln, fs, print, println, time, users};

pub use self::config::User as ConfiguredUser;

const DATA: &str = "/data";
const DATA_CONFIG: &str = "/data/etc/system.conf";
const CONFIG: &str = "/etc/system.conf";
const IDS: &str = "/data/etc/ids";
const STORE: &str = "/data/pkg";
const GENERATIONS: &str = "/data/generations";
const CACHE: &str = "/data/cache";
const KEYS: &str = "/etc/eli/keys";
const RUN: &str = "/run/eli";

const USAGE: &str = "\
usage: eli COMMAND

  add PACKAGE...      install packages (adds them to /etc/system.conf)
  remove PACKAGE...   uninstall packages
  search WORD         find packages
  info PACKAGE        describe a package
  list                show what's installed
  update              fetch the latest package lists
  upgrade             update, then move to the newest versions
  apply               make the system match /etc/system.conf
  generations         list the system's generations
  rollback [N]        go back to the previous generation (or number N)
  clean               delete old generations and unused packages
";

fn fail(message: &str) -> i32 {
    eprintln!("eli: {message}");
    1
}

fn needs_root() -> bool {
    if users::euid() != 0 {
        eprintln!("eli: that changes the system: run it with sudo");
        return true;
    }
    false
}

fn read_config() -> Result<(String, config::Config), String> {
    let text = fs::read_to_string(CONFIG).map_err(|error| format!("{CONFIG}: {error}"))?;
    let parsed = config::parse(&text)?;
    Ok((text, parsed))
}

pub fn main(arguments: &[String]) -> i32 {
    let command = arguments.get(1).map(String::as_str).unwrap_or("");
    let rest = &arguments[arguments.len().min(2)..];
    let result = match command {
        "add" | "install" => change_packages(rest, true),
        "remove" | "uninstall" | "del" => change_packages(rest, false),
        "search" => search(rest),
        "info" | "show" => info(rest),
        "list" => list(),
        "update" => update(),
        "upgrade" => upgrade(),
        "apply" => apply("eli apply", false),
        "generations" => show_generations(),
        "rollback" => rollback(rest),
        "clean" => clean(),
        "boot" => generations::boot(),
        "" | "help" | "-h" | "--help" => {
            print!("{USAGE}");
            return 0;
        }
        _ => Err(format!("no command {command:?}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => 0,
        Err(message) => fail(&message),
    }
}

/// `eli add` and `eli remove`: change the package list, then apply.
fn change_packages(names: &[String], adding: bool) -> Result<(), String> {
    if needs_root() {
        return Err("permission denied".into());
    }
    if names.is_empty() {
        return Err(format!("which packages? (eli {} NAME...)", if adding { "add" } else { "remove" }));
    }
    let (text, config) = read_config()?;
    let mut packages = config.packages.clone();
    if adding {
        let index = store::load_or_update(&store::repositories(&config.repositories))?;
        for name in names {
            if packages.contains(name) {
                println!("{name} is already in the package list");
                continue;
            }
            if index.find(name).is_none() {
                let suggestions: Vec<String> = index.search(name).iter().take(5).map(|p| p.name.clone()).collect();
                let hint = if suggestions.is_empty() {
                    String::new()
                } else {
                    format!(" (maybe: {})", suggestions.join(", "))
                };
                return Err(format!("there's no package called {name}{hint}"));
            }
            packages.push(name.clone());
        }
    } else {
        for name in names {
            if !packages.contains(name) {
                return Err(format!(
                    "{name} isn't in the package list (it may be installed as another package's dependency)"
                ));
            }
            packages.retain(|package| package != name);
        }
    }
    let new_text = config::set(&text, "packages", &packages.join(" "));
    fs::write(CONFIG, new_text.as_bytes()).map_err(|error| format!("{CONFIG}: {error}"))?;
    let verb = if adding { "add" } else { "remove" };
    apply(&format!("eli {verb} {}", names.join(" ")), false)
}

/// `eli apply`: resolves system.conf's packages, fetches what's missing,
/// records a new generation and activates it. Nothing changed means no new
/// generation.
fn apply(reason: &str, newest: bool) -> Result<(), String> {
    if needs_root() {
        return Err("permission denied".into());
    }
    let (text, config) = read_config()?;
    let repositories = store::repositories(&config.repositories);
    let packages: Vec<String> = if config.packages.is_empty() {
        Vec::new()
    } else {
        let index = store::load_or_update(&repositories)?;
        let resolved = index.resolve(&config.packages)?;
        // Keep the versions already in use unless upgrading (or they've
        // left the repository).
        let previous = generations::current().and_then(|number| generations::load(number).ok());
        let in_use: Vec<String> = previous.as_ref().map(|g| g.packages.clone()).unwrap_or_default();
        let mut ids = Vec::new();
        let mut fetching = Vec::new();
        for package in resolved {
            let kept = (!newest)
                .then(|| {
                    in_use.iter().find(|id| apk::index::split_id(id).is_some_and(|(name, _)| name == package.name))
                })
                .flatten()
                .filter(|id| store::installed(id));
            match kept {
                Some(id) => ids.push(id.clone()),
                None => {
                    if !store::installed(&package.id()) {
                        fetching.push(package);
                    }
                    ids.push(package.id());
                }
            }
        }
        if !fetching.is_empty() {
            let bytes: u64 = fetching.iter().map(|package| package.size).sum();
            println!(
                "Downloading {} package{} ({}):",
                fetching.len(),
                if fetching.len() == 1 { "" } else { "s" },
                size(bytes)
            );
            // One line, rewritten as each downloads; the summary below
            // lists what was added.
            for (number, package) in fetching.iter().enumerate() {
                let label = format!("[{}/{}] {}", number + 1, fetching.len(), package.id());
                store::fetch(package, &repositories, &label)?;
            }
            eprint!("\r\x1b[K");
        }
        ids
    };

    let current = generations::current().and_then(|number| generations::load(number).ok());
    if let Some(current) = &current
        && current.config_text == text
        && current.packages == packages
    {
        println!("Nothing to do: the system already matches /etc/system.conf (generation {}).", current.number);
        return Ok(());
    }
    let number = generations::create(&text, &packages, reason)?;
    let generation = generations::load(number)?;
    generations::activate(&generation)?;
    generations::make_current(number)?;
    report_changes(current.as_ref().map(|g| g.packages.as_slice()).unwrap_or(&[]), &packages);
    println!("Generation {number} is active.");
    Ok(())
}

fn size(bytes: u64) -> String {
    crate::storage::human_size(bytes)
}

/// Shows which packages came and went between two lists.
fn report_changes(before: &[String], after: &[String]) {
    for id in after.iter().filter(|id| !before.contains(id)) {
        println!("  \x1b[32m+ {id}\x1b[0m");
    }
    for id in before.iter().filter(|id| !after.contains(id)) {
        println!("  \x1b[31m- {id}\x1b[0m");
    }
}

fn index_for_queries() -> Result<apk::Index, String> {
    let config = read_config().map(|(_, config)| config).unwrap_or_else(|_| config::parse("").unwrap());
    let repositories = store::repositories(&config.repositories);
    match store::load(&repositories) {
        Some(index) => Ok(index),
        None if users::euid() == 0 => store::load_or_update(&repositories),
        None => Err("there are no package lists yet: run sudo eli update".into()),
    }
}

fn search(words: &[String]) -> Result<(), String> {
    let Some(word) = words.first() else { return Err("search for what? (eli search WORD)".into()) };
    let index = index_for_queries()?;
    let found = index.search(word);
    if found.is_empty() {
        println!("Nothing matches {word:?}.");
    }
    for package in found.iter().take(40) {
        println!("\x1b[1m{:<24}\x1b[0m {}", package.name, package.description);
    }
    if found.len() > 40 {
        println!("... and {} more", found.len() - 40);
    }
    Ok(())
}

fn info(names: &[String]) -> Result<(), String> {
    let Some(name) = names.first() else { return Err("which package? (eli info NAME)".into()) };
    let index = index_for_queries()?;
    let package = index.find(name).ok_or_else(|| format!("there's no package called {name}"))?;
    println!("\x1b[1m{}\x1b[0m {}", package.name, package.version);
    println!("  {}", package.description);
    println!("  website: {}", package.url);
    println!("  license: {}", package.license);
    println!("  download: {}, installed: {}", size(package.size), size(package.installed_size));
    let needs = index.resolve(core::slice::from_ref(name)).map(|all| all.len().saturating_sub(1)).unwrap_or(0);
    println!("  needs {needs} other package{}", if needs == 1 { "" } else { "s" });
    Ok(())
}

fn list() -> Result<(), String> {
    let (_, config) = read_config()?;
    let Some(number) = generations::current() else {
        println!("No packages yet.");
        return Ok(());
    };
    let generation = generations::load(number)?;
    if generation.packages.is_empty() {
        println!("No packages installed. (eli add NAME installs one; eli search WORD finds them.)");
    }
    for id in &generation.packages {
        let name = apk::index::split_id(id).map_or(id.as_str(), |(name, _)| name);
        let mark = if config.packages.iter().any(|wanted| wanted == name) { "" } else { "  (needed by another)" };
        println!("{id}{mark}");
    }
    Ok(())
}

fn update() -> Result<(), String> {
    if needs_root() {
        return Err("permission denied".into());
    }
    let (_, config) = read_config()?;
    println!("Fetching the package lists:");
    store::update(&store::repositories(&config.repositories))
}

fn upgrade() -> Result<(), String> {
    update()?;
    apply("eli upgrade", true)
}

fn show_generations() -> Result<(), String> {
    let current = generations::current();
    let running: Option<u32> =
        fs::read_to_string(&format!("{RUN}/generation")).ok().and_then(|text| text.trim().parse().ok());
    for number in generations::list() {
        let Ok(generation) = generations::load(number) else { continue };
        let date = time::DateTime::from_unix(generation.created);
        let mut marks = Vec::new();
        if Some(number) == current {
            marks.push("current");
        }
        if Some(number) == running {
            marks.push("running");
        }
        let marks = if marks.is_empty() { String::new() } else { format!("  [{}]", marks.join(", ")) };
        println!(
            "{number:>4}  {:04}-{:02}-{:02} {:02}:{:02}  {:<28} {} packages{marks}",
            date.year,
            date.month,
            date.day,
            date.hour,
            date.minute,
            generation.reason,
            generation.packages.len()
        );
    }
    Ok(())
}

/// `eli rollback [N]`: makes an earlier generation current again, and its
/// configuration /etc/system.conf.
fn rollback(arguments: &[String]) -> Result<(), String> {
    if needs_root() {
        return Err("permission denied".into());
    }
    let current = generations::current().ok_or("there are no generations")?;
    let target = match arguments.first() {
        Some(number) => number.parse().map_err(|_| format!("{number:?} isn't a generation number"))?,
        None => {
            generations::list().into_iter().rfind(|&number| number < current).ok_or("there's no earlier generation")?
        }
    };
    let generation = generations::load(target)?;
    generations::activate(&generation)?;
    generations::make_current(target)?;
    fs::write(CONFIG, generation.config_text.as_bytes()).map_err(|error| format!("{CONFIG}: {error}"))?;
    let before = generations::load(current).map(|g| g.packages).unwrap_or_default();
    report_changes(&before, &generation.packages);
    println!("Generation {target} is active (and /etc/system.conf is as it was then).");
    Ok(())
}

/// `eli clean`: keeps the last five generations (and the current one), and
/// deletes packages none of them use.
fn clean() -> Result<(), String> {
    if needs_root() {
        return Err("permission denied".into());
    }
    let all = generations::list();
    let current = generations::current();
    let keep: Vec<u32> = all.iter().rev().take(5).copied().chain(current).collect();
    let mut removed_generations = 0;
    for &number in all.iter().filter(|number| !keep.contains(number)) {
        if crate::commands::remove_tree(&format!("{GENERATIONS}/{number}")).is_ok() {
            removed_generations += 1;
        }
    }
    let used: Vec<String> =
        keep.iter().filter_map(|&number| generations::load(number).ok()).flat_map(|g| g.packages).collect();
    let mut removed_packages = 0;
    for entry in fs::read_dir(STORE).unwrap_or_default() {
        if !used.contains(&entry.name) && crate::commands::remove_tree(&format!("{STORE}/{}", entry.name)).is_ok() {
            removed_packages += 1;
        }
    }
    println!("Removed {removed_generations} old generations and {removed_packages} unused packages.");
    Ok(())
}

/// For the shell's "command not found": which packages have the command.
pub fn suggest(command: &str) -> Option<String> {
    let (_, config) = read_config().ok()?;
    let index = store::load(&store::repositories(&config.repositories))?;
    let providers = index.providing_command(command);
    let first = providers.first()?;
    Some(format!("{command} isn't installed; it's in the {} package: sudo eli add {}", first.name, first.name))
}

/// For adduser and deluser on a system managed by system.conf: changes the
/// accounts there and applies it, so they last.
pub fn change_user(add: Option<ConfiguredUser>, remove: Option<&str>) -> Result<(), String> {
    let (mut text, _) = read_config()?;
    if let Some(user) = &add {
        text = config::add_user(&text, user);
    }
    if let Some(name) = remove {
        text = config::remove_user(&text, name);
    }
    config::parse(&text)?;
    fs::write(CONFIG, text.as_bytes()).map_err(|error| format!("{CONFIG}: {error}"))?;
    apply(
        &match (&add, remove) {
            (Some(user), _) => format!("adduser {}", user.name),
            (_, Some(name)) => format!("deluser {name}"),
            _ => "accounts".to_string(),
        },
        false,
    )
}

/// For the installer: system.conf's text with a setting changed.
pub fn set_setting(text: &str, key: &str, value: &str) -> String {
    config::set(text, key, value)
}

/// For the installer: system.conf's text with an account added.
pub fn add_user_block(text: &str, user: &ConfiguredUser) -> String {
    config::add_user(text, user)
}

/// The packages of the generation in use (that are really in the store).
pub fn running_packages() -> Vec<String> {
    generations::current()
        .and_then(|number| generations::load(number).ok())
        .map(|generation| generation.packages.into_iter().filter(|id| store::installed(id)).collect())
        .unwrap_or_default()
}

/// Who to log in without asking, at startup: system.conf's `autologin`,
/// or on the live system (no data partition), root.
pub fn autologin() -> Option<String> {
    if let Some(name) = read_config().ok().and_then(|(_, config)| config.autologin) {
        return Some(name);
    }
    let live = !fs::mounts().iter().any(|mount| mount.target == DATA);
    live.then(|| "root".into())
}

/// Whether this system is described by /etc/system.conf (it is once `eli
/// boot` has run).
pub fn managed() -> bool {
    fs::exists(CONFIG) && fs::exists(GENERATIONS)
}
