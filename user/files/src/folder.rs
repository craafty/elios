//! What the explorer shows and does to files, apart from the window:
//! reading a folder, sorting it, paths, and copying, moving and naming.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::Error;
use eli::fs::{self, FileType};
use ui::widgets::Icon;

/// One thing in a folder.
pub struct Entry {
    pub name: String,
    /// What it is, after following a symbolic link (a dangling link is
    /// [`FileType::Symlink`]).
    pub kind: FileType,
    pub link: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
    pub executable: bool,
}

impl Entry {
    pub fn is_folder(&self) -> bool {
        self.kind == FileType::Directory
    }

    pub fn hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    pub fn icon(&self) -> Icon {
        match self.kind {
            FileType::Directory => Icon::Folder,
            FileType::File if self.executable => Icon::Program,
            FileType::BlockDevice => Icon::Disk,
            _ => Icon::File,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Size,
    Modified,
}

/// How a folder is listed.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    pub column: Column,
    pub descending: bool,
    pub show_hidden: bool,
}

/// The entries of the folder at `path` (in `order`), and how many hidden
/// ones were left out.
pub fn read(path: &str, order: Order) -> Result<(Vec<Entry>, usize), Error> {
    let mut entries = Vec::new();
    let mut hidden = 0;
    for item in fs::read_dir(path)? {
        if item.name.starts_with('.') && !order.show_hidden {
            hidden += 1;
            continue;
        }
        let full = join(path, &item.name);
        let link = item.kind == FileType::Symlink;
        // Follow links, so a link to a folder opens like one; a dangling
        // link (or a file we may not look at) is still listed.
        let metadata = fs::metadata(&full).or_else(|_| fs::symlink_metadata(&full)).ok();
        entries.push(Entry {
            kind: metadata.map_or(item.kind, |m| m.kind),
            link,
            size: metadata.map_or(0, |m| m.size),
            modified: metadata.map_or(0, |m| m.modified),
            executable: metadata.is_some_and(|m| m.mode & 0o111 != 0),
            name: item.name,
        });
    }
    sort(&mut entries, order);
    Ok((entries, hidden))
}

/// Folders first, then by the chosen column (names break ties).
pub fn sort(entries: &mut [Entry], order: Order) {
    entries.sort_by(|a, b| {
        let by_name = || {
            let (x, y) = (a.name.to_lowercase(), b.name.to_lowercase());
            x.cmp(&y).then_with(|| a.name.cmp(&b.name))
        };
        let by_column = match order.column {
            Column::Name => by_name(),
            Column::Size => a.size.cmp(&b.size).then_with(by_name),
            Column::Modified => a.modified.cmp(&b.modified).then_with(by_name),
        };
        let by_column = if order.descending { by_column.reverse() } else { by_column };
        b.is_folder().cmp(&a.is_folder()).then(by_column)
    });
}

pub fn join(folder: &str, name: &str) -> String {
    if folder.ends_with('/') { format!("{folder}{name}") } else { format!("{folder}/{name}") }
}

/// The folder holding `path` (`/` for `/` itself).
pub fn parent(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(slash) => &path[..slash],
    }
}

/// The last part of `path`.
pub fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed.rsplit('/').next().unwrap_or(trimmed) }
}

/// Whether `path` is `folder` or somewhere inside it.
pub fn is_within(path: &str, folder: &str) -> bool {
    path == folder || path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/') || folder == "/")
}

/// A path typed by the user, made absolute: `~` is the home folder and a
/// relative path starts at `current`. `.` and `..` are worked out, so the
/// path bar never shows them.
pub fn resolve(typed: &str, current: &str, home: &str) -> String {
    let typed = typed.trim();
    let full = if typed == "~" {
        String::from(home)
    } else if let Some(rest) = typed.strip_prefix("~/") {
        join(home, rest)
    } else if typed.starts_with('/') {
        String::from(typed)
    } else {
        join(current, typed)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// A name for a copy of `name` in `folder` that isn't taken: the name
/// itself, then "name (copy)", "name (copy 2)" and so on, keeping the
/// extension at the end.
pub fn free_name(folder: &str, name: &str) -> String {
    let taken = |candidate: &str| fs::symlink_metadata(&join(folder, candidate)).is_ok();
    if !taken(name) {
        return String::from(name);
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (1..)
        .map(|n| if n == 1 { format!("{stem} (copy){extension}") } else { format!("{stem} (copy {n}){extension}") })
        .find(|candidate| !taken(candidate))
        .unwrap_or_default()
}

/// Checks a name typed for a new or renamed file.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        Err("a name can't be empty")
    } else if name.contains('/') {
        Err("a name can't contain /")
    } else if name == "." || name == ".." {
        Err("that name is reserved")
    } else {
        Ok(())
    }
}

/// Copies (or moves) `source` into `folder`, under a free name; returns
/// the name it got.
pub fn paste(source: &str, folder: &str, cut: bool) -> Result<String, String> {
    if fs::symlink_metadata(source).is_err() {
        return Err(format!("{} isn't there any more", base_name(source)));
    }
    if is_within(folder, source) {
        return Err(String::from("a folder can't go inside itself"));
    }
    if cut && parent(source) == folder {
        return Ok(base_name(source).to_string());
    }
    let name = free_name(folder, base_name(source));
    let destination = join(folder, &name);
    let result = if cut {
        match fs::rename(source, &destination) {
            // Another file system: copy, then remove the original.
            Err(Error::CROSS_DEVICE) => fs::copy_tree(source, &destination).and_then(|()| fs::remove_tree(source)),
            other => other,
        }
    } else {
        fs::copy_tree(source, &destination)
    };
    result
        .map(|()| name)
        .map_err(|error| format!("can't {} {}: {error}", if cut { "move" } else { "copy" }, base_name(source)))
}

/// Whether a file looks like text (no NUL bytes near the start), so an
/// editor can open it.
pub fn is_text(path: &str) -> bool {
    let Ok(file) = fs::File::open(path) else { return false };
    let mut start = [0u8; 4096];
    file.read_at(0, &mut start).is_ok_and(|len| !start[..len].contains(&0))
}

/// A size as people read it: "512 B", "3.4 KB", "120 MB".
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < 3 {
        value /= 1024.0;
        unit += 1;
    }
    let unit = ["KB", "MB", "GB", "TB"][unit];
    if value < 10.0 { format!("{value:.1} {unit}") } else { format!("{value:.0} {unit}") }
}

/// A modification time as "2026-10-17 01:09" (UTC, like the desktop's
/// clock).
pub fn date(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    let time = eli::time::DateTime::from_unix(seconds as u64);
    format!("{}-{:02}-{:02} {:02}:{:02}", time.year, time.month, time.day, time.hour, time.minute)
}

/// A place in the sidebar.
pub struct Place {
    pub label: String,
    pub path: String,
    pub icon: Icon,
}

/// The sidebar: home, the root, and every mounted disk (the boot disk,
/// the data partition, USB sticks).
pub fn places(home: &str) -> Vec<Place> {
    let mut places = alloc::vec![
        Place { label: String::from("Home"), path: String::from(home), icon: Icon::Home },
        Place { label: String::from("Computer"), path: String::from("/"), icon: Icon::Folder },
    ];
    for mount in fs::mounts() {
        if mount.source.starts_with("/dev/") && mount.target != "/" && !places.iter().any(|p| p.path == mount.target) {
            let label = match mount.target.as_str() {
                "/disk" => String::from("Boot disk"),
                "/data" => String::from("Data"),
                target => base_name(target).to_string(),
            };
            places.push(Place { label, path: mount.target, icon: Icon::Disk });
        }
    }
    places
}
