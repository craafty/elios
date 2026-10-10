//! eliauth: `su`, `sudo` and `passwd`, the commands that act with root's
//! privileges on behalf of ordinary users.
//!
//! This program is installed setuid root (mode 4755): whoever runs it, it
//! starts with an effective user id of 0, and its real user id says who
//! asked. It's a separate program from eliutils so that only this small
//! amount of code ever runs with privileges someone else gave it. Each
//! command checks a password (or that the caller is already root) before
//! doing anything, and the caller's environment is never trusted: commands
//! are found through a fixed search path.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eli::users::{self, User};
use eli::{env, eprintln, fs, println, process, term, time};

eli::main!(main);

/// Where sudo looks for commands, whatever the caller's PATH says.
const SECURE_PATH: &str = "/bin:/usr/bin:/sbin:/usr/sbin";
/// How long sudo remembers a password was given.
const SUDO_GRACE_SECONDS: u64 = 5 * 60;
const SUDO_TIMESTAMPS: &str = "/run/sudo";

fn main() -> i32 {
    let arguments = env::args();
    match env::program_name().as_str() {
        "su" => su(&arguments),
        "sudo" => sudo(&arguments),
        "passwd" => passwd(&arguments),
        _ => {
            eprintln!("eliauth provides su, sudo and passwd; run it under one of those names");
            2
        }
    }
}

/// The account of whoever ran this program (the real user id).
fn caller() -> Option<User> {
    users::user_with_id(users::uid())
}

/// Asks for `user`'s password, up to three times. Accounts without one
/// pass straight away.
fn ask_password(user: &User, prompt: &str) -> bool {
    if users::has_empty_password(&user.name) {
        return true;
    }
    for attempt in 0..3 {
        let Some(password) = term::read_password(prompt) else { return false };
        if users::check_password(&user.name, &password) {
            return true;
        }
        time::sleep_ms(2000);
        if attempt < 2 {
            eprintln!("Sorry, try again.");
        }
    }
    false
}

/// What a shell is told its name is (argv[0]): `-sh` for a login shell,
/// `sh` otherwise.
fn shell_name(user: &User, login: bool) -> String {
    let base = user.login_shell().rsplit('/').next().unwrap_or("sh");
    if login { format!("-{base}") } else { base.into() }
}

/// Becomes `target` and runs `command` (or their shell). A login session
/// starts in their home with a fresh environment; otherwise the
/// environment is kept, with HOME, SHELL, USER and LOGNAME changed.
fn run_as(target: &User, login: bool, command: Option<Vec<String>>, extra: &[String]) -> i32 {
    let mut environment = if login {
        users::login_environment(target)
    } else {
        let mut kept: Vec<String> = env::vars()
            .into_iter()
            .filter(|v| !["HOME=", "SHELL=", "USER=", "LOGNAME="].iter().any(|name| v.starts_with(name)))
            .collect();
        // The caller's PATH and TERM stay.
        let changed = users::login_environment(target).into_iter();
        kept.extend(changed.filter(|v| !v.starts_with("PATH=") && !v.starts_with("TERM=")));
        kept
    };
    environment.extend_from_slice(extra);
    if let Err(error) = users::become_user(target) {
        eprintln!("can't become {}: {error}", target.name);
        return 1;
    }
    if login && fs::set_current_dir(&target.home).is_err() {
        let _ = fs::set_current_dir("/");
    }
    let (program, arguments) = match command {
        Some(arguments) => {
            let Some(program) = find_program(&arguments[0]) else {
                eprintln!("{}: command not found", arguments[0]);
                return 127;
            };
            (program, arguments)
        }
        None => (target.login_shell().into(), alloc::vec![shell_name(target, login)]),
    };
    let error = process::exec(&program, &arguments, &environment);
    eprintln!("can't run {program}: {error}");
    126
}

/// Finds a program on the secure path (or as given, with a slash).
fn find_program(name: &str) -> Option<String> {
    if name.contains('/') {
        return fs::exists(name).then(|| name.into());
    }
    SECURE_PATH.split(':').map(|directory| format!("{directory}/{name}")).find(|candidate| {
        fs::metadata(candidate).is_ok_and(|m| m.kind == fs::FileType::File && m.permissions() & 0o111 != 0)
    })
}

/// `su [-] [-c COMMAND] [USER]`: runs a shell (or a command) as another
/// user, root by default, after asking for that user's password. Root
/// needn't give one.
fn su(arguments: &[String]) -> i32 {
    let mut login = false;
    let mut command = None;
    let mut name = None;
    let mut rest = arguments[1..].iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-" | "-l" | "--login" => login = true,
            "-c" => command = rest.next().cloned(),
            _ if name.is_none() && !argument.starts_with('-') => name = Some(argument.clone()),
            _ => {
                eprintln!("usage: su [-] [-c COMMAND] [USER]");
                return 2;
            }
        }
    }
    let name = name.unwrap_or_else(|| "root".into());
    let Some(target) = users::user_named(&name) else {
        eprintln!("su: user {name} does not exist");
        return 1;
    };
    if users::uid() != 0 && !ask_password(&target, "Password: ") {
        eprintln!("su: Authentication failure");
        return 1;
    }
    let command = command.map(|command| alloc::vec![target.login_shell().into(), "-c".into(), command]);
    run_as(&target, login, command, &[])
}

/// Whether `user` gave sudo their password in the last few minutes.
fn recently_authenticated(user: &User) -> bool {
    let path = format!("{SUDO_TIMESTAMPS}/{}", user.uid);
    fs::metadata(&path).is_ok_and(|metadata| {
        let age = time::unix_time().saturating_sub(metadata.modified.max(0) as u64);
        metadata.uid == 0 && age < SUDO_GRACE_SECONDS
    })
}

fn remember_authentication(user: &User) {
    let _ = fs::create_dir_with("/run", 0o755);
    let _ = fs::create_dir_with(SUDO_TIMESTAMPS, 0o700);
    let path = format!("{SUDO_TIMESTAMPS}/{}", user.uid);
    let _ = fs::write(&path, b"");
    let _ = fs::set_modified(&path, None);
}

/// `sudo [-i | -s] [-u USER] [COMMAND...]`: runs a command as root (or
/// USER) for members of the `wheel` group, after they confirm it's them
/// with their own password. `-i` starts a login shell, `-s` a shell.
fn sudo(arguments: &[String]) -> i32 {
    let mut login = false;
    let mut shell = false;
    let mut target_name = String::from("root");
    let mut index = 1;
    while let Some(argument) = arguments.get(index) {
        match argument.as_str() {
            "-i" => login = true,
            "-s" => shell = true,
            "-u" => {
                index += 1;
                match arguments.get(index) {
                    Some(name) => target_name = name.clone(),
                    None => {
                        eprintln!("sudo: -u needs a user");
                        return 2;
                    }
                }
            }
            "-k" => {
                let _ = fs::remove_file(&format!("{SUDO_TIMESTAMPS}/{}", users::uid()));
                return 0;
            }
            _ if argument.starts_with('-') => {
                eprintln!("usage: sudo [-i | -s] [-u USER] COMMAND...");
                return 2;
            }
            _ => break,
        }
        index += 1;
    }
    let command: Vec<String> = arguments[index..].to_vec();
    if command.is_empty() && !login && !shell {
        eprintln!("usage: sudo [-i | -s] [-u USER] COMMAND...");
        return 2;
    }
    let Some(me) = caller() else {
        eprintln!("sudo: you (user {}) have no account", users::uid());
        return 1;
    };
    let Some(target) = users::user_named(&target_name) else {
        eprintln!("sudo: unknown user {target_name}");
        return 1;
    };
    if me.uid != 0 {
        if !users::is_admin(&me) {
            eprintln!("sudo: {} is not allowed to use sudo (only members of {})", me.name, users::ADMIN_GROUP);
            return 1;
        }
        if !recently_authenticated(&me) {
            if !ask_password(&me, &format!("[sudo] password for {}: ", me.name)) {
                eprintln!("sudo: incorrect password");
                return 1;
            }
            remember_authentication(&me);
        }
    }
    let extra = [
        format!("SUDO_USER={}", me.name),
        format!("SUDO_UID={}", me.uid),
        format!("SUDO_GID={}", me.gid),
        format!("SUDO_COMMAND={}", command.join(" ")),
    ];
    let command = if command.is_empty() {
        None
    } else if shell {
        Some(alloc::vec![target.login_shell().into(), "-c".into(), command.join(" ")])
    } else {
        Some(command)
    };
    run_as(&target, login, command, &extra)
}

/// `passwd [-d | -l | -u] [USER]`: changes a password. Users change their
/// own (after giving the current one); root can change anyone's, remove
/// one (`-d`), or lock and unlock accounts (`-l`, `-u`).
fn passwd(arguments: &[String]) -> i32 {
    let option = arguments[1..].iter().find(|a| a.starts_with('-')).map(String::as_str);
    let name = arguments[1..].iter().find(|a| !a.starts_with('-')).cloned();
    let Some(me) = caller() else {
        eprintln!("passwd: you (user {}) have no account", users::uid());
        return 1;
    };
    let name = name.unwrap_or_else(|| me.name.clone());
    let Some(target) = users::user_named(&name) else {
        eprintln!("passwd: user {name} does not exist");
        return 1;
    };
    let root = me.uid == 0;
    if !root && (target.uid != me.uid || option.is_some()) {
        eprintln!("passwd: only root can change other accounts");
        return 1;
    }
    let current = users::password_hash(&target.name).ok().flatten().unwrap_or_else(|| "!".into());
    let new_hash = match option {
        Some("-d") => String::new(),
        Some("-l") if current.starts_with('!') => current,
        Some("-l") => format!("!{current}"),
        Some("-u") => current.trim_start_matches('!').into(),
        Some(other) => {
            eprintln!("passwd: unknown option {other}");
            return 2;
        }
        None => {
            if !root && !users::has_empty_password(&me.name) {
                let given = term::read_password("Current password: ").unwrap_or_default();
                if !users::check_password(&me.name, &given) {
                    time::sleep_ms(2000);
                    eprintln!("passwd: Authentication failure");
                    return 1;
                }
            }
            let first = term::read_password("New password: ");
            let second = term::read_password("Retype new password: ");
            match (first, second) {
                (Some(first), Some(second)) if first == second => {
                    if first.is_empty() && !root {
                        eprintln!("passwd: an empty password isn't allowed (root can remove one with -d)");
                        return 1;
                    }
                    if first.is_empty() { String::new() } else { eli::crypt::hash_password(&first) }
                }
                (Some(_), Some(_)) => {
                    eprintln!("passwd: the passwords don't match");
                    return 1;
                }
                _ => return 1,
            }
        }
    };
    match users::set_password_hash(&target.name, new_hash) {
        Ok(()) => {
            println!("passwd: password for {} updated", target.name);
            0
        }
        Err(error) => {
            eprintln!("passwd: can't update {}: {error}", users::SHADOW);
            1
        }
    }
}
