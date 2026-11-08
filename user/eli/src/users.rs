//! Users and groups: the running process's ids, and the account files.
//!
//! Accounts live in the same files, with the same formats, as on Linux:
//!
//! - `/etc/passwd`: `name:x:uid:gid:full name:home:shell`, readable by all.
//! - `/etc/group`: `name:x:gid:member,member,...`.
//! - `/etc/shadow`: `name:password hash:...`, readable only by root. A hash
//!   is `$6$...` (see [`crate::crypt`]), empty for no password, or starts
//!   with `!` or `*` for a locked account.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs;
use crate::sys::{self, Error, Result};

pub const PASSWD: &str = "/etc/passwd";
pub const GROUP: &str = "/etc/group";
pub const SHADOW: &str = "/etc/shadow";

/// Members of this group may run commands as root with `sudo`.
pub const ADMIN_GROUP: &str = "wheel";

/// Groups every user of the computer is in: the devices of the person at
/// it (sound, the screen, keyboards and mice) and its Wi-Fi and Bluetooth
/// (`netdev`), as desktop Linux systems grant them.
pub const SEAT_GROUPS: [&str; 4] = ["audio", "video", "input", "netdev"];

/// Ordinary accounts get user ids from here up.
pub const FIRST_USER_ID: u32 = 1000;

fn id_call(number: usize) -> u32 {
    // SAFETY: these calls take no arguments.
    unsafe { sys::syscall(number, [0; 6]) as u32 }
}

/// The real user id: who started the program.
pub fn uid() -> u32 {
    id_call(sys::GETUID)
}

/// The effective user id: who the program acts as.
pub fn euid() -> u32 {
    id_call(sys::GETEUID)
}

pub fn gid() -> u32 {
    id_call(sys::GETGID)
}

pub fn egid() -> u32 {
    id_call(sys::GETEGID)
}

/// The supplementary groups.
pub fn groups() -> Vec<u32> {
    let mut list = [0u32; 64];
    // SAFETY: room for 64 ids.
    let count = unsafe { sys::call(sys::GETGROUPS, [list.len(), list.as_mut_ptr() as usize, 0, 0, 0, 0]) };
    list[..count.unwrap_or(0)].to_vec()
}

pub fn set_groups(groups: &[u32]) -> Result<()> {
    // SAFETY: the pointer and count describe the list.
    unsafe { sys::call(sys::SETGROUPS, [groups.len(), groups.as_ptr() as usize, 0, 0, 0, 0]) }.map(|_| ())
}

/// `setresuid`: `None` leaves an id unchanged.
pub fn set_user_ids(real: Option<u32>, effective: Option<u32>, saved: Option<u32>) -> Result<()> {
    let id = |value: Option<u32>| value.map_or(usize::MAX, |id| id as usize);
    // SAFETY: takes no pointers.
    unsafe { sys::call(sys::SETRESUID, [id(real), id(effective), id(saved), 0, 0, 0]) }.map(|_| ())
}

pub fn set_group_ids(real: Option<u32>, effective: Option<u32>, saved: Option<u32>) -> Result<()> {
    let id = |value: Option<u32>| value.map_or(usize::MAX, |id| id as usize);
    // SAFETY: takes no pointers.
    unsafe { sys::call(sys::SETRESGID, [id(real), id(effective), id(saved), 0, 0, 0]) }.map(|_| ())
}

/// An account, from /etc/passwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The "GECOS" field: a full name, normally.
    pub full_name: String,
    pub home: String,
    pub shell: String,
}

impl User {
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 || fields[0].is_empty() {
            return None;
        }
        Some(Self {
            name: fields[0].into(),
            uid: fields[2].parse().ok()?,
            gid: fields[3].parse().ok()?,
            full_name: fields[4].into(),
            home: fields[5].into(),
            shell: fields[6].into(),
        })
    }

    fn line(&self) -> String {
        alloc::format!("{}:x:{}:{}:{}:{}:{}", self.name, self.uid, self.gid, self.full_name, self.home, self.shell)
    }

    /// The shell to start for them (`/bin/sh` if none is set).
    pub fn login_shell(&self) -> &str {
        if self.shell.is_empty() { "/bin/sh" } else { &self.shell }
    }
}

/// A group, from /etc/group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

impl Group {
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 4 || fields[0].is_empty() {
            return None;
        }
        let members = fields[3].split(',').filter(|m| !m.is_empty()).map(String::from).collect();
        Some(Self { name: fields[0].into(), gid: fields[2].parse().ok()?, members })
    }

    fn line(&self) -> String {
        alloc::format!("{}:x:{}:{}", self.name, self.gid, self.members.join(","))
    }
}

/// A password entry, from /etc/shadow. Fields after the hash (password
/// ageing, which eliOS doesn't enforce) are kept as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadow {
    pub name: String,
    pub hash: String,
    rest: String,
}

impl Shadow {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.splitn(3, ':');
        let name = fields.next().filter(|name| !name.is_empty())?;
        let hash = fields.next()?;
        Some(Self { name: name.into(), hash: hash.into(), rest: fields.next().unwrap_or("").into() })
    }

    fn line(&self) -> String {
        alloc::format!("{}:{}:{}", self.name, self.hash, self.rest)
    }

    /// A new entry, with the date of the change recorded as Linux does (in
    /// days since 1970) and no ageing limits.
    pub fn new(name: &str, hash: String) -> Self {
        let days = crate::time::unix_time() / 86_400;
        Self { name: name.into(), hash, rest: alloc::format!("{days}:0:99999:7:::") }
    }

    /// Whether the account can't be logged into with a password.
    pub fn is_locked(&self) -> bool {
        self.hash.starts_with('!') || self.hash.starts_with('*')
    }
}

fn read_lines(path: &str) -> Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    Ok(text.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('#')).map(String::from).collect())
}

pub fn users() -> Vec<User> {
    read_lines(PASSWD).unwrap_or_default().iter().filter_map(|line| User::parse(line)).collect()
}

pub fn all_groups() -> Vec<Group> {
    read_lines(GROUP).unwrap_or_default().iter().filter_map(|line| Group::parse(line)).collect()
}

/// The password entries (only root can read them).
pub fn shadow_entries() -> Result<Vec<Shadow>> {
    Ok(read_lines(SHADOW)?.iter().filter_map(|line| Shadow::parse(line)).collect())
}

pub fn user_named(name: &str) -> Option<User> {
    users().into_iter().find(|user| user.name == name)
}

pub fn user_with_id(uid: u32) -> Option<User> {
    users().into_iter().find(|user| user.uid == uid)
}

pub fn group_named(name: &str) -> Option<Group> {
    all_groups().into_iter().find(|group| group.name == name)
}

pub fn group_with_id(gid: u32) -> Option<Group> {
    all_groups().into_iter().find(|group| group.gid == gid)
}

/// A user's name, or their number if they have no account.
pub fn user_name(uid: u32) -> String {
    user_with_id(uid).map_or_else(|| uid.to_string(), |user| user.name)
}

pub fn group_name(gid: u32) -> String {
    group_with_id(gid).map_or_else(|| gid.to_string(), |group| group.name)
}

/// Every group `user` is in: their primary group, then the ones that list
/// them as a member.
pub fn groups_of(user: &User) -> Vec<u32> {
    let mut ids = alloc::vec![user.gid];
    for group in all_groups() {
        if group.members.contains(&user.name) && !ids.contains(&group.gid) {
            ids.push(group.gid);
        }
    }
    ids
}

/// Whether `user` may use `sudo`.
pub fn is_admin(user: &User) -> bool {
    user.uid == 0 || group_named(ADMIN_GROUP).is_some_and(|group| groups_of(user).contains(&group.gid))
}

/// The stored password hash for an account.
pub fn password_hash(name: &str) -> Result<Option<String>> {
    Ok(shadow_entries()?.into_iter().find(|entry| entry.name == name).map(|entry| entry.hash))
}

/// Whether `password` is right for `name`. Accounts without a shadow entry
/// or with a locked one never match.
pub fn check_password(name: &str, password: &str) -> bool {
    match password_hash(name) {
        Ok(Some(hash)) => crate::crypt::verify_password(password, &hash),
        _ => false,
    }
}

/// Whether logging in as `name` needs no password at all.
pub fn has_empty_password(name: &str) -> bool {
    matches!(password_hash(name), Ok(Some(hash)) if hash.is_empty())
}

/// Replaces a file's contents all at once: written to a temporary file next
/// to it, given the same owner and permissions, then renamed over it. Anyone
/// reading it sees the old or the new file, never half of one.
fn replace_file(path: &str, lines: &[String], permissions: u32) -> Result<()> {
    // Through symbolic links: on eliOS, /etc/shadow is a link to the data
    // partition, and that's the file to replace.
    let mut path = String::from(path);
    for _ in 0..8 {
        match fs::read_link(&path) {
            Ok(target) if target.starts_with('/') => path = target,
            Ok(target) => {
                let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory).to_string();
                path = alloc::format!("{directory}/{target}");
            }
            Err(_) => break,
        }
    }
    let path = path.as_str();
    let temporary = alloc::format!("{path}+");
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(&temporary, text.as_bytes())?;
    fs::set_permissions(&temporary, permissions)?;
    fs::rename(&temporary, path)
}

pub fn write_users(users: &[User]) -> Result<()> {
    replace_file(PASSWD, &users.iter().map(User::line).collect::<Vec<_>>(), 0o644)
}

pub fn write_groups(groups: &[Group]) -> Result<()> {
    replace_file(GROUP, &groups.iter().map(Group::line).collect::<Vec<_>>(), 0o644)
}

pub fn write_shadow(entries: &[Shadow]) -> Result<()> {
    replace_file(SHADOW, &entries.iter().map(Shadow::line).collect::<Vec<_>>(), 0o600)
}

/// Sets (or replaces) the stored hash for an account.
pub fn set_password_hash(name: &str, hash: String) -> Result<()> {
    let mut entries = shadow_entries().unwrap_or_default();
    match entries.iter_mut().find(|entry| entry.name == name) {
        Some(entry) => *entry = Shadow { hash, ..entry.clone() },
        None => entries.push(Shadow::new(name, hash)),
    }
    write_shadow(&entries)
}

/// Whether a name is acceptable for a new account or group: lowercase
/// letters, digits, `_` and `-`, starting with a letter or `_`, as most
/// Linux tools require.
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Turns this process into `user`: their groups, then their group id, then
/// their user id (the order matters: once the user id is no longer root,
/// the others can't be changed).
pub fn become_user(user: &User) -> Result<()> {
    set_groups(&groups_of(user))?;
    set_group_ids(Some(user.gid), Some(user.gid), Some(user.gid))?;
    set_user_ids(Some(user.uid), Some(user.uid), Some(user.uid))?;
    // Make sure it took: a program that failed to drop privileges must not
    // carry on.
    if uid() != user.uid || euid() != user.uid {
        return Err(Error::NOT_PERMITTED);
    }
    Ok(())
}

/// The environment a new session for `user` starts with: their home,
/// name and shell, the standard search path, and the terminal type.
pub fn login_environment(user: &User) -> Vec<String> {
    let path = if user.uid == 0 { "/bin:/usr/bin:/sbin:/usr/sbin" } else { "/bin:/usr/bin" };
    let term = crate::env::var("TERM").unwrap_or_else(|| "linux".into());
    alloc::vec![
        alloc::format!("HOME={}", user.home),
        alloc::format!("USER={}", user.name),
        alloc::format!("LOGNAME={}", user.name),
        alloc::format!("SHELL={}", user.login_shell()),
        alloc::format!("PATH={path}"),
        alloc::format!("TERM={term}"),
    ]
}
