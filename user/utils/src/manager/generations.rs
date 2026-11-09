//! Generations, and making the running system match one ("activation").
//!
//! A generation is a numbered directory under /data/generations holding a
//! copy of system.conf and the exact packages it resolved to. `current`
//! points at the one in use. Generations are never changed: applying a new
//! configuration makes a new one, and rolling back just points `current`
//! at an older one, so going back always gets exactly what was there.
//!
//! The base system (the image eliOS boots from) is rebuilt in memory at
//! every boot, so activating means adding to it: the generation's packages
//! are linked into place (`/usr/bin/git` becomes a link into the store),
//! and /etc/passwd, /etc/group and the hostname are written from the
//! configuration. Activating another generation undoes the first one's
//! links (it records them in /run/eli/links) before making its own.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::{self, FileType};
use eli::users::{self, Group, User};
use eli::{eprintln, process, system, time};

use super::config::{self, Config};
use super::store;
use super::{DATA, DATA_CONFIG, GENERATIONS, IDS, RUN};

const LINKS: &str = "/run/eli/links";

/// One generation: its number, configuration and packages (`name-version`).
pub struct Generation {
    pub number: u32,
    pub config: Config,
    pub config_text: String,
    pub packages: Vec<String>,
    /// When it was made (seconds since 1970) and what made it.
    pub created: u64,
    pub reason: String,
}

fn directory(number: u32) -> String {
    format!("{GENERATIONS}/{number}")
}

/// The generation numbers there are, oldest first.
pub fn list() -> Vec<u32> {
    let mut numbers: Vec<u32> =
        fs::read_dir(GENERATIONS).unwrap_or_default().iter().filter_map(|entry| entry.name.parse().ok()).collect();
    numbers.sort_unstable();
    numbers
}

/// The generation `current` points at.
pub fn current() -> Option<u32> {
    fs::read_link(&format!("{GENERATIONS}/current")).ok()?.parse().ok()
}

pub fn load(number: u32) -> Result<Generation, String> {
    let base = directory(number);
    let config_text =
        fs::read_to_string(&format!("{base}/system.conf")).map_err(|_| format!("there's no generation {number}"))?;
    let config = config::parse(&config_text)?;
    let packages = fs::read_to_string(&format!("{base}/packages"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();
    let info = fs::read_to_string(&format!("{base}/info")).unwrap_or_default();
    let field = |name: &str| info.lines().find_map(|line| line.strip_prefix(name)).unwrap_or("").trim().to_string();
    Ok(Generation {
        number,
        config,
        config_text,
        packages,
        created: field("created =").parse().unwrap_or(0),
        reason: field("reason ="),
    })
}

/// Records a new generation. Returns its number.
pub fn create(config_text: &str, packages: &[String], reason: &str) -> Result<u32, String> {
    let _ = fs::create_dir(GENERATIONS);
    let number = list().last().map_or(1, |last| last + 1);
    let base = directory(number);
    let write = |name: &str, text: &str| {
        fs::write(&format!("{base}/{name}"), text.as_bytes()).map_err(|error| format!("{base}/{name}: {error}"))
    };
    fs::create_dir(&base).map_err(|error| format!("{base}: {error}"))?;
    write("system.conf", config_text)?;
    write("packages", &(packages.join("\n") + "\n"))?;
    write("info", &format!("created = {}\nreason = {reason}\n", time::unix_time()))?;
    Ok(number)
}

/// Makes `number` the current generation (for this boot and later ones).
pub fn make_current(number: u32) -> Result<(), String> {
    // A new link renamed over the old one: the switch is all or nothing.
    let temporary = format!("{GENERATIONS}/current+");
    let _ = fs::remove_file(&temporary);
    fs::symlink(&number.to_string(), &temporary).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &format!("{GENERATIONS}/current")).map_err(|error| error.to_string())?;
    write_boot_menu();
    Ok(())
}

/// Lists the generations for eliboot's menu, on the boot partition (which
/// eliboot can read; the data partition is ext2, which it can't).
fn write_boot_menu() {
    const MENU: &str = "/disk/EFI/eliOS/generations.txt";
    if !fs::exists("/disk/EFI/eliOS") {
        return;
    }
    let current = current();
    let mut text = String::new();
    for number in list().into_iter().rev().take(9) {
        let Ok(generation) = load(number) else { continue };
        let date = time::DateTime::local(generation.created as i64);
        let mark = if Some(number) == current { " (current)" } else { "" };
        text.push_str(&format!(
            "{number}\t{:04}-{:02}-{:02} {:02}:{:02}  {}{mark}\n",
            date.year, date.month, date.day, date.hour, date.minute, generation.reason
        ));
    }
    let _ = fs::write(MENU, text.as_bytes());
}

/// The generation chosen in eliboot's menu for this boot, if any.
pub fn chosen_at_boot() -> Option<u32> {
    let line = fs::read_to_string("/proc/cmdline").ok()?;
    line.split_whitespace().find_map(|word| word.strip_prefix("generation=")?.parse().ok())
}

/// Makes the running system match a generation.
pub fn activate(generation: &Generation) -> Result<(), String> {
    let _ = fs::create_dir_with(RUN, 0o755);
    apply_accounts(&generation.config)?;
    let name = &generation.config.hostname;
    let _ = fs::write("/etc/hostname", format!("{name}\n").as_bytes());
    let _ = system::set_hostname(name);
    link_packages(&generation.packages)?;
    set_timezone(&generation.config.timezone);
    run_triggers(&generation.packages);
    let _ = fs::write(&format!("{RUN}/generation"), format!("{}\n", generation.number).as_bytes());
    Ok(())
}

/// Writes /etc/localtime for a zone: Alpine's file for it if `tzdata` is
/// installed (linked into /usr by now), otherwise one made from the rule
/// libeli knows for it.
fn set_timezone(setting: &str) {
    let installed = format!("/usr/share/zoneinfo/{setting}");
    let file = fs::read(&installed).ok().filter(|file| eli::zone::rule_of_tzif(file).is_some()).or_else(|| {
        let rule = eli::zone::rule_for(setting)?;
        Some(eli::zone::Zone::parse(rule)?.tzif(rule))
    });
    let file = file.unwrap_or_else(|| {
        eprintln!("eli: unknown time zone {setting} (add the tzdata package for every zone); using UTC");
        eli::zone::Zone::utc().tzif("UTC0")
    });
    let _ = fs::write("/etc/localtime", &file);
}

/// Runs the packages' triggers: scripts that rebuild caches from what
/// other packages put in the directories they watch (image loaders for
/// GTK, GLib's settings schemas, the font and MIME databases). Alpine runs
/// them after installing; here the system is assembled anew at every
/// activation, so they run then. They're BusyBox shell scripts; what they
/// print goes to /run/eli/triggers.log.
fn run_triggers(packages: &[String]) {
    let shell: &[&str] = if fs::exists("/bin/busybox") { &["/bin/busybox", "sh"] } else { &["/bin/sh"] };
    let log = format!("{RUN}/triggers.log");
    let _ = fs::write(&log, b"");
    for id in packages {
        let scripts = format!("{}/{}", store::path(id), store::SCRIPTS);
        let script = format!("{scripts}/.trigger");
        let Ok(patterns) = fs::read_to_string(&format!("{scripts}/triggers")) else { continue };
        if !fs::exists(&script) {
            continue;
        }
        let directories: Vec<String> = patterns.split_whitespace().flat_map(expand_pattern).collect();
        if directories.is_empty() {
            continue;
        }
        let mut arguments: Vec<String> = shell.iter().map(|part| String::from(*part)).collect();
        arguments.push(script.clone());
        arguments.extend(directories);
        match process::fork() {
            Ok(0) => {
                if let Ok(file) = fs::File::append(&log) {
                    let _ = eli::io::dup2(file.fd(), 1);
                    let _ = eli::io::dup2(file.fd(), 2);
                }
                let error = process::exec(shell[0], &arguments, &eli::env::vars());
                eprintln!("eli: {script}: {error}");
                process::exit(127);
            }
            Ok(pid) => {
                if let Ok(Some((_, status))) = process::wait(pid as i32, false)
                    && !status.success()
                {
                    eprintln!("eli: {id}'s trigger failed ({status}); see {log}");
                }
            }
            Err(error) => eprintln!("eli: {id}'s trigger: {error}"),
        }
    }
}

/// The existing directories a trigger pattern (like
/// `/usr/lib/gdk-pixbuf-2.0/*/loaders`) names: `*` matches any one path
/// component.
fn expand_pattern(pattern: &str) -> Vec<String> {
    let mut paths = alloc::vec![String::new()];
    for component in pattern.split('/').filter(|component| !component.is_empty()) {
        let mut next = Vec::new();
        for path in paths {
            if component.contains('*') {
                let directory = if path.is_empty() { String::from("/") } else { path.clone() };
                for entry in fs::read_dir(&directory).unwrap_or_default() {
                    if glob_matches(component, &entry.name) {
                        next.push(format!("{path}/{}", entry.name));
                    }
                }
            } else {
                next.push(format!("{path}/{component}"));
            }
        }
        paths = next;
    }
    paths.into_iter().filter(|path| fs::metadata(path).is_ok_and(|m| m.kind == FileType::Directory)).collect()
}

/// Whether `name` matches a pattern with `*` wildcards.
fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, rest)) => {
            let Some(after) = name.strip_prefix(prefix) else { return false };
            (0..=after.len()).any(|skip| after.is_char_boundary(skip) && glob_matches(rest, &after[skip..]))
        }
    }
}

/// Account ids, remembered so a user keeps theirs (and owns their files)
/// however system.conf is reordered.
fn load_ids() -> Vec<(String, u32)> {
    fs::read_to_string(IDS)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (name, id) = line.split_once(' ')?;
            Some((name.into(), id.trim().parse().ok()?))
        })
        .collect()
}

/// Writes /etc/passwd and /etc/group: the system's own accounts (from the
/// image) plus the configured users, each with a group of the same name.
fn apply_accounts(config: &Config) -> Result<(), String> {
    let mut ids = load_ids();
    let base_users: Vec<User> =
        users::users().into_iter().filter(|user| user.uid < users::FIRST_USER_ID || user.uid >= 60_000).collect();
    let mut groups: Vec<Group> = users::all_groups()
        .into_iter()
        .filter(|group| group.gid < users::FIRST_USER_ID || group.gid >= 60_000)
        .collect();
    let mut accounts = base_users;
    for wanted in &config.users {
        let id = match ids.iter().find(|(name, _)| *name == wanted.name) {
            Some((_, id)) => *id,
            None => {
                let taken = |id: u32| ids.iter().any(|(_, used)| *used == id);
                let id = (users::FIRST_USER_ID..60_000).find(|&id| !taken(id)).ok_or("no free user ids")?;
                ids.push((wanted.name.clone(), id));
                id
            }
        };
        accounts.push(User {
            name: wanted.name.clone(),
            uid: id,
            gid: id,
            full_name: wanted.full_name.clone(),
            home: format!("/home/{}", wanted.name),
            shell: wanted.shell.clone(),
        });
        groups.push(Group { name: wanted.name.clone(), gid: id, members: Vec::new() });
    }
    // wheel: root and the administrators.
    let admins: Vec<String> = config.users.iter().filter(|user| user.admin).map(|user| user.name.clone()).collect();
    match groups.iter_mut().find(|group| group.name == users::ADMIN_GROUP) {
        Some(wheel) => {
            wheel.members.retain(|member| member == "root");
            wheel.members.extend(admins);
        }
        None => groups.push(Group { name: users::ADMIN_GROUP.into(), gid: 10, members: admins }),
    }
    // The devices of the person at the computer: everyone configured.
    for group in groups.iter_mut().filter(|group| users::SEAT_GROUPS.contains(&group.name.as_str())) {
        group.members = config.users.iter().map(|user| user.name.clone()).collect();
    }
    let id_text: String = ids.iter().map(|(name, id)| format!("{name} {id}\n")).collect();
    fs::write(IDS, id_text.as_bytes()).map_err(|error| format!("{IDS}: {error}"))?;
    users::write_users(&accounts).map_err(|error| format!("/etc/passwd: {error}"))?;
    users::write_groups(&groups).map_err(|error| format!("/etc/group: {error}"))?;

    // Everyone needs a password entry; new ones start locked until a
    // password is set.
    let mut shadow = users::shadow_entries().unwrap_or_default();
    for account in accounts.iter().filter(|account| account.uid >= users::FIRST_USER_ID && account.uid < 60_000) {
        if !shadow.iter().any(|entry| entry.name == account.name) {
            shadow.push(users::Shadow::new(&account.name, "!".into()));
        }
    }
    users::write_shadow(&shadow).map_err(|error| format!("/etc/shadow: {error}"))?;

    for account in accounts.iter().filter(|account| account.uid >= users::FIRST_USER_ID && account.uid < 60_000) {
        if !fs::exists(&account.home) {
            make_home(account);
        }
    }
    Ok(())
}

/// A new user's home, copied from /etc/skel.
pub fn make_home(account: &User) {
    if fs::exists("/etc/skel") {
        let _ = fs::copy_tree("/etc/skel", &account.home);
    } else {
        let _ = fs::create_dir(&account.home);
    }
    let _ = give_tree(&account.home, account.uid, account.gid);
    let _ = fs::set_permissions(&account.home, 0o750);
}

fn give_tree(path: &str, uid: u32, gid: u32) -> eli::Result<()> {
    fs::set_owner(path, Some(uid), Some(gid))?;
    if fs::symlink_metadata(path)?.kind == FileType::Directory {
        for entry in fs::read_dir(path)? {
            give_tree(&format!("{path}/{}", entry.name), uid, gid)?;
        }
    }
    Ok(())
}

/// What an activation added to the running system, to undo later:
/// `dir PATH`, `link PATH`, or `replaced PATH TARGET` (a base link a
/// package's file took the place of).
fn undo_links() {
    let record = fs::read_to_string(LINKS).unwrap_or_default();
    let lines: Vec<&str> = record.lines().collect();
    // Newest first, so directories are emptied before they're removed.
    for line in lines.iter().rev() {
        let mut fields = line.splitn(3, ' ');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("link"), Some(path), _) => {
                let _ = fs::remove_file(path);
            }
            (Some("dir"), Some(path), _) => {
                let _ = fs::remove_dir(path);
            }
            (Some("replaced"), Some(path), Some(target)) => {
                let _ = fs::remove_file(path);
                let _ = fs::symlink(target, path);
            }
            _ => {}
        }
    }
    let _ = fs::remove_file(LINKS);
}

/// Whether a base-system entry may be covered by a package's file: only
/// BusyBox's command links (a real package of the same command is what the
/// user wants), never eliOS's own files.
fn replaceable(path: &str) -> Option<String> {
    let target = fs::read_link(path).ok()?;
    (target == "busybox" || target.ends_with("/busybox")).then_some(target)
}

/// Links every file of the packages into the running system, after
/// undoing the previous activation's links.
fn link_packages(packages: &[String]) -> Result<(), String> {
    undo_links();
    let mut record = String::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for id in packages {
        if !store::installed(id) {
            eprintln!("eli: {id} isn't in the store (its files are missing); run eli apply");
            continue;
        }
        let store = store::path(id);
        for (relative, is_directory) in store::files(id) {
            let target = format!("/{relative}");
            let source = format!("{store}/{relative}");
            match fs::symlink_metadata(&target) {
                // Directories merge; files go in as links.
                Ok(metadata) if is_directory && metadata.kind == FileType::Directory => continue,
                Ok(_) if is_directory => {
                    eprintln!("eli: {id}: {target} is in the way of a directory");
                    continue;
                }
                Err(_) if is_directory => {
                    if fs::create_dir(&target).is_ok() {
                        record.push_str(&format!("dir {target}\n"));
                    }
                    continue;
                }
                Err(_) => {}
                Ok(_) => {
                    if taken.contains(&target) {
                        // Two packages with the same file: the first wins.
                        continue;
                    }
                    let Some(previous) = replaceable(&target) else { continue };
                    let _ = fs::remove_file(&target);
                    record.push_str(&format!("replaced {target} {previous}\n"));
                }
            }
            if fs::symlink(&source, &target).is_ok() {
                record.push_str(&format!("link {target}\n"));
                taken.insert(target);
            }
        }
    }
    fs::write(LINKS, record.as_bytes()).map_err(|error| format!("{LINKS}: {error}"))
}

/// Startup: sets up /data (a fresh live system gets the default
/// configuration and a first generation) and activates the current
/// generation, or the one picked in eliboot's menu.
pub fn boot() -> Result<(), String> {
    // On a live stick, /data on the stick itself rather than in memory.
    super::live::mount_data();
    for directory in [DATA, &format!("{DATA}/etc"), &format!("{DATA}/home"), GENERATIONS] {
        let _ = fs::create_dir(directory);
    }
    if !fs::exists(DATA_CONFIG) {
        // The image's own (in /etc/eli, since /etc/system.conf becomes a
        // link to this one).
        let default = fs::read_to_string("/etc/eli/system.conf").unwrap_or_else(|_| "hostname = elios\n".into());
        fs::write(DATA_CONFIG, default.as_bytes()).map_err(|error| format!("{DATA_CONFIG}: {error}"))?;
    }
    // The password file lives with the data, so password changes last.
    if !fs::exists(&format!("{DATA}/etc/shadow")) {
        let shadow = fs::read("/etc/shadow").unwrap_or_default();
        fs::write(&format!("{DATA}/etc/shadow"), &shadow).map_err(|error| error.to_string())?;
        let _ = fs::set_permissions(&format!("{DATA}/etc/shadow"), 0o600);
    }
    // A machine id (32 hex digits) that lasts: D-Bus and GLib want one.
    if !fs::exists(&format!("{DATA}/etc/machine-id")) {
        let uuid = fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default();
        let id: String = uuid.chars().filter(char::is_ascii_hexdigit).collect();
        let _ = fs::write(&format!("{DATA}/etc/machine-id"), format!("{id}\n").as_bytes());
    }
    for (link, target) in [
        ("/etc/system.conf", DATA_CONFIG),
        ("/etc/shadow", "/data/etc/shadow"),
        ("/etc/machine-id", "/data/etc/machine-id"),
    ] {
        if fs::read_link(link).ok().as_deref() != Some(target) {
            let _ = fs::remove_file(link);
            let _ = fs::symlink(target, link);
        }
    }
    // /home is the data partition's.
    if !fs::mounts().iter().any(|mount| mount.target == "/home") {
        let _ = fs::mount(&format!("{DATA}/home"), "/home", "", fs::MOUNT_BIND);
    }

    let number = match chosen_at_boot().filter(|&number| load(number).is_ok()).or_else(current) {
        Some(number) => number,
        None => {
            // A first boot: the configuration as it is, no packages yet.
            let text = fs::read_to_string(DATA_CONFIG).unwrap_or_default();
            let number = create(&text, &[], "first boot")?;
            make_current(number)?;
            number
        }
    };
    let generation = load(number)?;
    activate(&generation)?;
    if chosen_at_boot().is_some_and(|chosen| Some(chosen) != current()) {
        eprintln!("eliOS: started generation {number} from the boot menu (`eli rollback {number}` keeps it)");
    }
    Ok(())
}
