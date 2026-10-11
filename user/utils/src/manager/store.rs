//! The package store: repository indexes, and packages unpacked into
//! `/data/pkg/NAME-VERSION/`, one directory each, never changed once
//! there. Generations pick which of them are in use.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use apk::tar::{self, Kind};
use apk::{Index, Key, Package, PublicKey};
use eli::fs::{self, FileType};
use eli::{eprint, eprintln, http, time};

use super::{CACHE, KEYS, STORE};

/// Repositories used when system.conf names none.
const REPOSITORIES: &str = "/etc/eli/repositories";
/// Marks a store entry as complete (written last).
pub const COMPLETE: &str = ".eli-package";

/// The repositories to use: system.conf's, or the defaults.
pub fn repositories(configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    fs::read_to_string(REPOSITORIES)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

/// The keys repositories may be signed with (Alpine's, from
/// /etc/eli/keys).
fn keys() -> Vec<Key> {
    fs::read_dir(KEYS)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let text = fs::read_to_string(&format!("{KEYS}/{}", entry.name)).ok()?;
            Some(Key { name: entry.name, key: PublicKey::from_pem(&text)? })
        })
        .collect()
}

/// Shows download progress on one line.
fn progress_bar(label: &str) -> impl FnMut(usize, Option<usize>) + '_ {
    let mut last = 0;
    move |done, total| {
        let now = time::monotonic_nanos() / 100_000_000;
        if now != last {
            last = now;
            match total {
                Some(total) if total > 0 => eprint!("\r\x1b[K  {label}: {}%", done * 100 / total),
                _ => eprint!("\r\x1b[K  {label}: {} KiB", done / 1024),
            }
        }
    }
}

fn index_path(number: usize) -> String {
    format!("{CACHE}/index-{number}")
}

/// Downloads every repository's index and checks its signature; keeps
/// them in the cache.
pub fn update(repositories: &[String]) -> Result<(), String> {
    let keys = keys();
    if keys.is_empty() {
        return Err(format!("no signing keys in {KEYS}"));
    }
    let _ = fs::create_dir(CACHE);
    for (number, repository) in repositories.iter().enumerate() {
        let url = format!("{repository}/x86_64/APKINDEX.tar.gz");
        let file = http::get(&url, &mut progress_bar(repository))?;
        eprint!("\r\x1b[K");
        let text = apk::read_index(&file, &keys).map_err(|error| format!("{url}: {error}"))?;
        fs::write(&index_path(number), text.as_bytes()).map_err(|error| format!("saving the index: {error}"))?;
        eprintln!("  {repository}: {} packages", text.matches("\nP:").count() + 1);
    }
    let sources = repositories.join("\n");
    fs::write(&format!("{CACHE}/sources"), sources.as_bytes()).map_err(|error| error.to_string())?;
    let _ = fs::remove_file(&index_path(repositories.len()));
    Ok(())
}

/// The cached indexes, if they're for these repositories. `None` means
/// `update` is needed.
pub fn load(repositories: &[String]) -> Option<Index> {
    let sources = fs::read_to_string(&format!("{CACHE}/sources")).ok()?;
    if sources.lines().collect::<Vec<_>>() != repositories.iter().map(String::as_str).collect::<Vec<_>>() {
        return None;
    }
    let mut packages = Vec::new();
    for number in 0..repositories.len() {
        let text = fs::read_to_string(&index_path(number)).ok()?;
        packages.extend(apk::index::parse(&text, number));
    }
    Some(Index::new(packages))
}

/// The cached indexes, fetching them first if there are none.
pub fn load_or_update(repositories: &[String]) -> Result<Index, String> {
    if let Some(index) = load(repositories) {
        return Ok(index);
    }
    eprintln!("Fetching the package lists:");
    update(repositories)?;
    load(repositories).ok_or_else(|| "the package lists couldn't be read".into())
}

/// Where a package lives in the store.
pub fn path(id: &str) -> String {
    format!("{STORE}/{id}")
}

pub fn installed(id: &str) -> bool {
    fs::exists(&format!("{}/{COMPLETE}", path(id)))
}

/// Makes sure a package is in the store: downloads it, checks it against
/// the (signed) index, and unpacks it.
pub fn fetch(package: &Package, repositories: &[String], label: &str) -> Result<(), String> {
    let id = package.id();
    if installed(&id) {
        return Ok(());
    }
    let repository = repositories.get(package.repository).ok_or("unknown repository")?;
    let url = format!("{repository}/x86_64/{}", package.file_name());
    let file = http::get(&url, &mut progress_bar(label))?;
    eprint!("\r\x1b[K  {label}: checking and unpacking");
    let contents = apk::read_package(&file, &package.checksum).map_err(|error| format!("{id}: {error}"))?;
    let entries = tar::entries(&contents.data).map_err(|error| format!("{id}: {error}"))?;

    let _ = fs::create_dir(STORE);
    let unpacking = format!("{STORE}/.unpacking-{id}");
    if fs::exists(&unpacking) {
        let _ = crate::commands::remove_tree(&unpacking);
    }
    fs::create_dir(&unpacking).map_err(|error| format!("{unpacking}: {error}"))?;
    for entry in &entries {
        if entry.path.is_empty() || entry.path.split('/').any(|part| part == "..") {
            continue;
        }
        let target = format!("{unpacking}/{}", entry.path);
        let result = match entry.kind {
            Kind::Directory => match fs::create_dir(&target) {
                Err(eli::Error::EXISTS) => Ok(()),
                result => result,
            },
            Kind::File => fs::write(&target, entry.data),
            Kind::Symlink => fs::symlink(&entry.link, &target),
            Kind::HardLink => fs::hard_link(&format!("{unpacking}/{}", entry.link), &target),
            Kind::Other => continue,
        };
        result.map_err(|error| format!("{id}: {}: {error}", entry.path))?;
        if matches!(entry.kind, Kind::File | Kind::Directory) {
            let _ = fs::set_permissions(&target, entry.mode);
            let _ = fs::set_modified(&target, Some(entry.modified));
        }
    }
    let scripts = contents.scripts();
    let record = format!(
        "name = {}\nversion = {}\ndescription = {}\nurl = {}\nlicense = {}\nskipped scripts = {}\n",
        package.name,
        package.version,
        package.description,
        package.url,
        package.license,
        scripts.join(" ")
    );
    fs::write(&format!("{unpacking}/{COMPLETE}"), record.as_bytes()).map_err(|error| error.to_string())?;
    fs::rename(&unpacking, &path(&id)).map_err(|error| format!("{id}: {error}"))?;
    Ok(())
}

/// The files of a store entry, relative to it, as (path, is a directory).
pub fn files(id: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let mut pending = alloc::vec![String::new()];
    while let Some(directory) = pending.pop() {
        let full = if directory.is_empty() { path(id) } else { format!("{}/{directory}", path(id)) };
        for entry in fs::read_dir(&full).unwrap_or_default() {
            if directory.is_empty() && entry.name == COMPLETE {
                continue;
            }
            let relative =
                if directory.is_empty() { entry.name.clone() } else { format!("{directory}/{}", entry.name) };
            let is_directory = entry.kind == FileType::Directory;
            if is_directory {
                pending.push(relative.clone());
            }
            found.push((relative, is_directory));
        }
    }
    found.sort();
    found
}
