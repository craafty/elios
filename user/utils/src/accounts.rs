//! Accounts and ownership: logging in, adding and removing users, and the
//! commands that show or change who owns what.
//!
//! The commands that need root's privileges when an ordinary user runs them
//! (`su`, `sudo`, `passwd`) are in a separate, setuid program, eliauth; see
//! src/auth.rs.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::{self, FileType};
use eli::users::{self, Group, Shadow, User};
use eli::{env, eprintln, io, print, println, process, system, term, time};

/// Where new users' homes start from: everything in it is copied to them.
const SKELETON: &str = "/etc/skel";
/// The group terminals belong to (so `write`-style tools could reach them).
const TTY_GROUP: &str = "tty";

/// `login [-f] [NAME]`: asks for a name and password, then starts that
/// user's shell. `-f` skips the password (for init's automatic login, and
/// only root may use it). Run by init on the console.
pub fn login(arguments: &[String]) -> i32 {
    if users::uid() != 0 {
        eprintln!("login: only root can start a login session");
        return 1;
    }
    let preauthenticated = arguments.iter().any(|a| a == "-f");
    let mut name = arguments[1..].iter().find(|a| !a.starts_with('-')).cloned();
    for _ in 0..3 {
        let entered = match name.take() {
            Some(name) => name,
            None => {
                print!("\n{} login: ", system::uname().hostname);
                match io::Lines::new(io::STDIN).next_line() {
                    Ok(Some(line)) if !line.trim().is_empty() => line.trim().to_string(),
                    Ok(Some(_)) => continue,
                    _ => return 1,
                }
            }
        };
        let account = users::user_named(&entered);
        let authenticated = preauthenticated || authenticate(&entered, account.is_some());
        match account {
            Some(user) if authenticated => return start_session(&user, None),
            _ => {
                // Pause the same way whether the name or the password was
                // wrong, so guessing names gains nothing.
                time::sleep_ms(2000);
                println!("Login incorrect");
            }
        }
    }
    1
}

/// Asks for `name`'s password, unless the account has none. Unknown names
/// are asked for one too, so they can't be told apart from real ones.
fn authenticate(name: &str, exists: bool) -> bool {
    if exists && users::has_empty_password(name) {
        return true;
    }
    match term::read_password("Password: ") {
        Some(password) => exists && users::check_password(name, &password),
        None => false,
    }
}

/// Becomes `user` and, in their home, runs `program` (the desktop, say),
/// or else their shell as a login shell.
pub fn start_session(user: &User, program: Option<&str>) -> i32 {
    // The terminal becomes theirs, so programs they run can reopen it.
    let tty_group = users::group_named(TTY_GROUP).map_or(user.gid, |group| group.gid);
    let _ = fs::set_owner("/dev/console", Some(user.uid), Some(tty_group));
    let _ = fs::set_permissions("/dev/console", 0o620);
    if let Err(error) = users::become_user(user) {
        eprintln!("login: can't become {}: {error}", user.name);
        return 1;
    }
    let mut environment = users::login_environment(user);
    if fs::set_current_dir(&user.home).is_err() {
        println!("No home directory {}; starting in /", user.home);
        let _ = fs::set_current_dir("/");
        environment.retain(|variable| !variable.starts_with("HOME="));
        environment.push("HOME=/".into());
    }
    if let Some(program) = program {
        let name = program.rsplit('/').next().unwrap_or(program).to_string();
        let error = process::exec(program, &[name], &environment);
        eprintln!("login: can't run {program}: {error}");
        return 1;
    }
    if let Ok(motd) = fs::read_to_string("/etc/motd") {
        print!("{motd}");
    }
    let shell = user.login_shell();
    let name = format!("-{}", shell.rsplit('/').next().unwrap_or("sh"));
    let error = process::exec(shell, &[name], &environment);
    eprintln!("login: can't run {shell}: {error}");
    1
}

fn needs_root(command: &str) -> bool {
    if users::euid() != 0 {
        eprintln!("{command}: only root can do that (try sudo {command})");
        return true;
    }
    false
}

/// Asks for a new password twice. `None` if they didn't match or input
/// ended.
pub fn ask_new_password(command: &str) -> Option<String> {
    let first = term::read_password("New password: ")?;
    let second = term::read_password("Retype new password: ")?;
    if first != second {
        eprintln!("{command}: the passwords don't match");
        return None;
    }
    Some(first)
}

/// `adduser [--admin] [--disabled-password] [--full-name NAME] USER`: a new
/// account with its own group and a home directory copied from /etc/skel.
/// `--admin` lets them use sudo.
pub fn adduser(arguments: &[String]) -> i32 {
    if needs_root("adduser") {
        return 1;
    }
    let mut admin = false;
    let mut disabled = false;
    let mut full_name = String::new();
    let mut name = None;
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--admin" => admin = true,
            "--disabled-password" => disabled = true,
            "--full-name" => full_name = rest.next().cloned().unwrap_or_default(),
            _ if name.is_none() && !argument.starts_with('-') => name = Some(argument.clone()),
            _ => {
                eprintln!("usage: adduser [--admin] [--disabled-password] [--full-name NAME] USER");
                return 2;
            }
        }
    }
    let Some(name) = name else {
        eprintln!("usage: adduser [--admin] [--disabled-password] [--full-name NAME] USER");
        return 2;
    };
    if !users::valid_name(&name) {
        eprintln!("adduser: {name}: use lowercase letters, digits, - and _, starting with a letter");
        return 1;
    }
    let mut accounts = users::users();
    let mut groups = users::all_groups();
    if accounts.iter().any(|user| user.name == name) || groups.iter().any(|group| group.name == name) {
        eprintln!("adduser: {name} already exists");
        return 1;
    }
    let password = if disabled {
        None
    } else {
        match ask_new_password("adduser") {
            Some(password) => Some(password),
            None => return 1,
        }
    };

    // A system described by /etc/system.conf gets the account there, so it
    // lasts (/etc/passwd is rewritten from it at every boot).
    if crate::manager::managed() {
        let user = crate::manager::ConfiguredUser { name: name.clone(), full_name, admin, shell: "/bin/sh".into() };
        if let Err(error) = crate::manager::change_user(Some(user), None) {
            eprintln!("adduser: {error}");
            return 1;
        }
        let hash = match password {
            None => "!".into(),
            Some(password) if password.is_empty() => String::new(),
            Some(password) => eli::crypt::hash_password(&password),
        };
        if let Err(error) = users::set_password_hash(&name, hash) {
            eprintln!("adduser: can't set the password: {error}");
            return 1;
        }
        println!("Added {name}{} (in /etc/system.conf).", if admin { ", who can use sudo" } else { "" });
        return 0;
    }

    // The first free id from 1000 up, the same for the user and their group.
    let taken = |id: u32| accounts.iter().any(|u| u.uid == id) || groups.iter().any(|g| g.gid == id);
    let id = (users::FIRST_USER_ID..60_000).find(|&id| !taken(id)).unwrap_or(60_000);
    let user = User {
        name: name.clone(),
        uid: id,
        gid: id,
        full_name,
        home: format!("/home/{name}"),
        shell: "/bin/sh".into(),
    };
    groups.push(Group { name: name.clone(), gid: id, members: Vec::new() });
    for group in groups.iter_mut().filter(|group| users::SEAT_GROUPS.contains(&group.name.as_str())) {
        group.members.push(name.clone());
    }
    if admin {
        match groups.iter_mut().find(|group| group.name == users::ADMIN_GROUP) {
            Some(wheel) => wheel.members.push(name.clone()),
            None => groups.push(Group { name: users::ADMIN_GROUP.into(), gid: 10, members: alloc::vec![name.clone()] }),
        }
    }
    accounts.push(user.clone());
    let hash = match password {
        None => "!".into(),
        Some(password) if password.is_empty() => String::new(),
        Some(password) => eli::crypt::hash_password(&password),
    };
    let saved = users::write_groups(&groups)
        .and_then(|_| users::write_users(&accounts))
        .and_then(|_| users::set_password_hash(&name, hash.clone()));
    if let Err(error) = saved {
        eprintln!("adduser: can't update the account files: {error}");
        return 1;
    }
    if let Err(error) = make_home(&user) {
        eprintln!("adduser: can't make {}: {error}", user.home);
        return 1;
    }
    println!("Added {name} (user id {id}){}.", if admin { ", who can use sudo" } else { "" });
    if hash.is_empty() {
        println!("{name} has no password: anyone can log in as them. Set one with: passwd {name}");
    }
    0
}

/// Creates a user's home from the skeleton, owned by them.
fn make_home(user: &User) -> eli::Result<()> {
    let _ = fs::create_dir_with("/home", 0o755);
    if fs::exists(SKELETON) {
        fs::copy_tree(SKELETON, &user.home)?;
    } else {
        fs::create_dir(&user.home)?;
    }
    give_tree(&user.home, user.uid, user.gid)?;
    fs::set_permissions(&user.home, 0o750)
}

/// Gives every file under `path` to a user and group.
fn give_tree(path: &str, uid: u32, gid: u32) -> eli::Result<()> {
    fs::set_owner(path, Some(uid), Some(gid))?;
    if fs::symlink_metadata(path)?.kind == FileType::Directory {
        for entry in fs::read_dir(path)? {
            give_tree(&format!("{path}/{}", entry.name), uid, gid)?;
        }
    }
    Ok(())
}

/// `deluser [--remove-home] USER`: removes an account (and its group, if no
/// one else uses it).
pub fn deluser(arguments: &[String]) -> i32 {
    if needs_root("deluser") {
        return 1;
    }
    let remove_home = arguments.iter().any(|a| a == "--remove-home");
    let Some(name) = arguments[1..].iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: deluser [--remove-home] USER");
        return 2;
    };
    let mut accounts = users::users();
    let Some(user) = accounts.iter().find(|user| user.name == *name).cloned() else {
        eprintln!("deluser: {name}: no such user");
        return 1;
    };
    if user.uid == 0 {
        eprintln!("deluser: refusing to remove root");
        return 1;
    }
    if crate::manager::managed() {
        if let Err(error) = crate::manager::change_user(None, Some(name)) {
            eprintln!("deluser: {error}");
            return 1;
        }
        let shadow: Vec<Shadow> =
            users::shadow_entries().unwrap_or_default().into_iter().filter(|entry| entry.name != *name).collect();
        let _ = users::write_shadow(&shadow);
        if remove_home {
            let _ = crate::commands::remove_tree(&user.home);
        }
        println!("Removed {name}.");
        return 0;
    }
    accounts.retain(|account| account.name != *name);
    let mut groups = users::all_groups();
    for group in &mut groups {
        group.members.retain(|member| member != name);
    }
    // Their own group goes too, unless it's someone else's primary group.
    groups.retain(|group| group.name != *name || accounts.iter().any(|account| account.gid == group.gid));
    let shadow: Vec<Shadow> =
        users::shadow_entries().unwrap_or_default().into_iter().filter(|entry| entry.name != *name).collect();
    let saved = users::write_users(&accounts)
        .and_then(|_| users::write_groups(&groups))
        .and_then(|_| users::write_shadow(&shadow));
    if let Err(error) = saved {
        eprintln!("deluser: can't update the account files: {error}");
        return 1;
    }
    if remove_home && let Err(error) = crate::commands::remove_tree(&user.home) {
        eprintln!("deluser: can't remove {}: {error}", user.home);
        return 1;
    }
    println!("Removed {name}.");
    0
}

/// `user(group)` for an id.
fn describe_group(gid: u32) -> String {
    format!("{gid}({})", users::group_name(gid))
}

/// `id [USER]`: user and group ids.
pub fn id(arguments: &[String]) -> i32 {
    let (uid, euid, gid, egid, groups) = match arguments.get(1) {
        Some(name) => match users::user_named(name) {
            Some(user) => (user.uid, user.uid, user.gid, user.gid, users::groups_of(&user)),
            None => {
                eprintln!("id: {name}: no such user");
                return 1;
            }
        },
        None => (users::uid(), users::euid(), users::gid(), users::egid(), users::groups()),
    };
    print!("uid={uid}({}) gid={}", users::user_name(uid), describe_group(gid));
    if euid != uid {
        print!(" euid={euid}({})", users::user_name(euid));
    }
    if egid != gid {
        print!(" egid={}", describe_group(egid));
    }
    let mut all = alloc::vec![gid];
    all.extend(groups.into_iter().filter(|&group| group != gid));
    println!(" groups={}", all.iter().map(|&group| describe_group(group)).collect::<Vec<_>>().join(","));
    0
}

pub fn whoami(_: &[String]) -> i32 {
    println!("{}", users::user_name(users::euid()));
    0
}

/// `groups [USER]`: the names of the groups someone is in.
pub fn groups(arguments: &[String]) -> i32 {
    let ids = match arguments.get(1) {
        Some(name) => match users::user_named(name) {
            Some(user) => users::groups_of(&user),
            None => {
                eprintln!("groups: {name}: no such user");
                return 1;
            }
        },
        None => {
            let mut ids = alloc::vec![users::egid()];
            ids.extend(users::groups().into_iter().filter(|&group| group != users::egid()));
            ids
        }
    };
    println!("{}", ids.iter().map(|&gid| users::group_name(gid)).collect::<Vec<_>>().join(" "));
    0
}

/// Applies a mode like `755`, `u+x`, `go-w` or `a=rX,u+s` to `current`.
fn apply_mode(spec: &str, current: u32, directory: bool) -> Option<u32> {
    if !spec.is_empty() && spec.chars().all(|c| c.is_digit(8)) {
        return u32::from_str_radix(spec, 8).ok().filter(|&mode| mode <= 0o7777);
    }
    let mut mode = current;
    for clause in spec.split(',') {
        let operator_at = clause.find(['+', '-', '='])?;
        let (who, rest) = clause.split_at(operator_at);
        let mut mask = 0;
        for c in who.chars() {
            mask |= match c {
                'u' => 0o4700,
                'g' => 0o2070,
                'o' => 0o1007,
                'a' => 0o7777,
                _ => return None,
            };
        }
        // No "who" means everyone, less the umask (as POSIX says).
        let umask = if mask == 0 {
            mask = 0o7777;
            let umask = fs::set_umask(0);
            fs::set_umask(umask);
            umask
        } else {
            0
        };
        let operator = rest.chars().next()?;
        let mut bits = 0;
        for c in rest[1..].chars() {
            bits |= match c {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                // Execute only for directories, or files executable by
                // someone already.
                'X' if directory || current & 0o111 != 0 => 0o111,
                'X' => 0,
                's' => 0o6000,
                't' => 0o1000,
                _ => return None,
            };
        }
        let bits = bits & mask & !umask;
        mode = match operator {
            '+' => mode | bits,
            '-' => mode & !bits,
            _ => (mode & !(mask & 0o7777)) | bits,
        };
    }
    Some(mode)
}

/// Runs `change` on each path, and with `-R` on everything inside.
fn for_each_path(command: &str, paths: &[&str], recursive: bool, change: &dyn Fn(&str) -> eli::Result<()>) -> i32 {
    let mut status = 0;
    for &path in paths {
        if let Err(error) = change(path) {
            eprintln!("{command}: {path}: {error}");
            status = 1;
            continue;
        }
        if recursive && fs::symlink_metadata(path).is_ok_and(|m| m.kind == FileType::Directory) {
            let entries = fs::read_dir(path).unwrap_or_default();
            let children: Vec<String> = entries.iter().map(|entry| format!("{path}/{}", entry.name)).collect();
            let children: Vec<&str> = children.iter().map(String::as_str).collect();
            status |= for_each_path(command, &children, true, change);
        }
    }
    status
}

/// Splits `[-R] FIRST PATH...`.
fn recursive_arguments(arguments: &[String]) -> (bool, Option<&str>, Vec<&str>) {
    let recursive = arguments[1..].iter().any(|a| a == "-R" || a == "-r");
    let mut rest = arguments[1..].iter().map(String::as_str).filter(|a| *a != "-R" && *a != "-r");
    (recursive, rest.next(), rest.collect())
}

/// `chmod [-R] MODE FILE...`.
pub fn chmod(arguments: &[String]) -> i32 {
    let (recursive, Some(spec), paths) = recursive_arguments(arguments) else {
        eprintln!("usage: chmod [-R] MODE FILE...");
        return 2;
    };
    if paths.is_empty() {
        eprintln!("usage: chmod [-R] MODE FILE...");
        return 2;
    }
    let change = |path: &str| {
        let metadata = fs::metadata(path)?;
        let mode = apply_mode(spec, metadata.permissions(), metadata.kind == FileType::Directory)
            .ok_or(eli::Error::INVALID)?;
        fs::set_permissions(path, mode)
    };
    for_each_path("chmod", &paths, recursive, &change)
}

/// Looks up a user or group given by name or number.
fn user_id(name: &str) -> Option<u32> {
    name.parse().ok().or_else(|| users::user_named(name).map(|user| user.uid))
}

fn group_id(name: &str) -> Option<u32> {
    name.parse().ok().or_else(|| users::group_named(name).map(|group| group.gid))
}

/// `chown [-R] USER[:GROUP] FILE...` (`USER:` means the user's own group).
pub fn chown(arguments: &[String]) -> i32 {
    let (recursive, Some(spec), paths) = recursive_arguments(arguments) else {
        eprintln!("usage: chown [-R] USER[:GROUP] FILE...");
        return 2;
    };
    let (user_part, group_part) = match spec.split_once([':', '.']) {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };
    let uid = if user_part.is_empty() {
        None
    } else {
        match user_id(user_part) {
            Some(uid) => Some(uid),
            None => {
                eprintln!("chown: {user_part}: no such user");
                return 1;
            }
        }
    };
    let gid = match group_part {
        None => None,
        Some("") => uid.and_then(users::user_with_id).map(|user| user.gid),
        Some(group) => match group_id(group) {
            Some(gid) => Some(gid),
            None => {
                eprintln!("chown: {group}: no such group");
                return 1;
            }
        },
    };
    for_each_path("chown", &paths, recursive, &|path| fs::set_owner(path, uid, gid))
}

/// `chgrp [-R] GROUP FILE...`.
pub fn chgrp(arguments: &[String]) -> i32 {
    let (recursive, Some(group), paths) = recursive_arguments(arguments) else {
        eprintln!("usage: chgrp [-R] GROUP FILE...");
        return 2;
    };
    let Some(gid) = group_id(group) else {
        eprintln!("chgrp: {group}: no such group");
        return 1;
    };
    for_each_path("chgrp", &paths, recursive, &|path| fs::set_owner(path, None, Some(gid)))
}

/// Names for user and group ids, read once (for `ls -l` and `ps`).
pub struct Names {
    users: Vec<User>,
    groups: Vec<Group>,
}

impl Names {
    pub fn load() -> Self {
        Self { users: users::users(), groups: users::all_groups() }
    }

    pub fn user(&self, uid: u32) -> String {
        self.users.iter().find(|user| user.uid == uid).map_or_else(|| uid.to_string(), |user| user.name.clone())
    }

    pub fn group(&self, gid: u32) -> String {
        self.groups.iter().find(|group| group.gid == gid).map_or_else(|| gid.to_string(), |group| group.name.clone())
    }
}

/// The account init logs in automatically the first time: system.conf's
/// `autologin`, or root on the live system.
pub fn autologin_user() -> Option<String> {
    let name = crate::manager::autologin()?;
    users::user_named(&name).is_some().then_some(name)
}

/// Whether this shell is a login shell (started as `-sh`).
pub fn is_login_shell() -> bool {
    env::args().first().is_some_and(|name| name.starts_with('-'))
}
