//! Alpine's package index (APKINDEX), versions, and dependency resolution.
//!
//! The index is a text file of records separated by blank lines, one per
//! package, each line a one-letter field: `P:` name, `V:` version, `D:`
//! dependencies, `p:` what else it provides, `C:` the checksum of its
//! control section, and so on.
//!
//! Dependencies name packages (`git`), versions of them (`musl>=1.2`),
//! shared libraries (`so:libz.so.1`) or commands (`cmd:vim`); a package
//! provides its own name plus whatever its `p:` line lists.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::hash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub description: String,
    pub url: String,
    pub license: String,
    /// Bytes to download, and on disk once installed.
    pub size: u64,
    pub installed_size: u64,
    pub depends: Vec<String>,
    pub provides: Vec<String>,
    /// `i:`: installed automatically once all of these are (a library's
    /// plugins come with it this way: GTK's image loaders with libglycin).
    pub install_if: Vec<String>,
    /// The SHA-1 of the package's control section (from `C:Q1<base64>`).
    pub checksum: [u8; 20],
    /// Preference among packages providing the same thing (`k:`).
    pub priority: u32,
    /// Which repository it came from (an index into the caller's list).
    pub repository: usize,
}

impl Package {
    /// The file name in the repository.
    pub fn file_name(&self) -> String {
        alloc::format!("{}-{}.apk", self.name, self.version)
    }

    /// `name-version`, the usual way to refer to an exact package.
    pub fn id(&self) -> String {
        alloc::format!("{}-{}", self.name, self.version)
    }
}

/// Parses an index's text into packages (from repository `repository`).
pub fn parse(text: &str, repository: usize) -> Vec<Package> {
    text.split("\n\n")
        .filter_map(|record| {
            let mut package = Package {
                name: String::new(),
                version: String::new(),
                description: String::new(),
                url: String::new(),
                license: String::new(),
                size: 0,
                installed_size: 0,
                depends: Vec::new(),
                provides: Vec::new(),
                install_if: Vec::new(),
                checksum: [0; 20],
                priority: 0,
                repository,
            };
            for line in record.lines() {
                let Some((key, value)) = line.split_once(':') else { continue };
                match key {
                    "P" => package.name = value.into(),
                    "V" => package.version = value.into(),
                    "T" => package.description = value.into(),
                    "U" => package.url = value.into(),
                    "L" => package.license = value.into(),
                    "S" => package.size = value.parse().unwrap_or(0),
                    "I" => package.installed_size = value.parse().unwrap_or(0),
                    "D" => package.depends = value.split_whitespace().map(String::from).collect(),
                    "p" => package.provides = value.split_whitespace().map(String::from).collect(),
                    "i" => package.install_if = value.split_whitespace().map(String::from).collect(),
                    "k" => package.priority = value.parse().unwrap_or(0),
                    "C" => {
                        let digest = value.strip_prefix("Q1").and_then(hash::base64_decode).unwrap_or_default();
                        package.checksum = digest.try_into().ok()?;
                    }
                    _ => {}
                }
            }
            (!package.name.is_empty() && !package.version.is_empty()).then_some(package)
        })
        .collect()
}

/// One piece of a version: a number, a letter, or a suffix like `_rc2`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    /// Pre-release suffixes sort before a plain version, the rest after.
    Suffix(i8, u64),
    Number(u64),
    Letter(u8),
}

fn suffix_rank(name: &str) -> i8 {
    match name {
        "alpha" => -4,
        "beta" => -3,
        "pre" => -2,
        "rc" => -1,
        "cvs" => 1,
        "svn" => 2,
        "git" => 3,
        "hg" => 4,
        "p" => 5,
        _ => 0,
    }
}

/// Splits a version like `1.2.3a_rc2-r1` into comparable parts and its
/// release number.
fn parse_version(version: &str) -> (Vec<Part>, u64) {
    let (main, release) = match version.rsplit_once("-r") {
        Some((main, release)) if release.bytes().all(|b| b.is_ascii_digit()) => (main, release.parse().unwrap_or(0)),
        _ => (version, 0),
    };
    let mut parts = Vec::new();
    let bytes = main.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            parts.push(Part::Number(main[start..index].parse().unwrap_or(u64::MAX)));
        } else if byte == b'_' {
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            let name = &main[start..index];
            let number_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            parts.push(Part::Suffix(suffix_rank(name), main[number_start..index].parse().unwrap_or(0)));
        } else if byte.is_ascii_alphabetic() {
            parts.push(Part::Letter(byte));
            index += 1;
        } else {
            index += 1;
        }
    }
    (parts, release)
}

/// Compares versions the way Alpine does: `1.10 > 1.9`, `1.2_rc1 < 1.2`,
/// `1.2-r2 > 1.2-r1`.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a_parts, a_release) = parse_version(a);
    let (b_parts, b_release) = parse_version(b);
    for index in 0..a_parts.len().max(b_parts.len()) {
        let ordering = match (a_parts.get(index), b_parts.get(index)) {
            (Some(x), Some(y)) => x.cmp(y),
            // What's left decides: more numbers mean newer, a pre-release
            // suffix means older.
            (Some(Part::Suffix(rank, _)), None) => (*rank).cmp(&0),
            (None, Some(Part::Suffix(rank, _))) => 0.cmp(rank),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a_release.cmp(&b_release)
}

/// A dependency: what's needed, and optionally which versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub constraint: Option<(String, String)>,
    /// `!name`: must not be installed (ignored when resolving).
    pub conflict: bool,
}

impl Dependency {
    pub fn parse(text: &str) -> Self {
        let (conflict, text) = match text.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        for operator in [">=", "<=", "~", "=", ">", "<"] {
            if let Some(at) = text.find(operator) {
                return Self {
                    name: text[..at].into(),
                    constraint: Some((operator.into(), text[at + operator.len()..].into())),
                    conflict,
                };
            }
        }
        Self { name: text.into(), constraint: None, conflict }
    }

    fn accepts(&self, version: &str) -> bool {
        let Some((operator, wanted)) = &self.constraint else { return true };
        let ordering = compare_versions(version, wanted);
        match operator.as_str() {
            ">=" => ordering != Ordering::Less,
            "<=" => ordering != Ordering::Greater,
            ">" => ordering == Ordering::Greater,
            "<" => ordering == Ordering::Less,
            // `~1.2`: any 1.2.x.
            "~" => version.starts_with(wanted.as_str()),
            _ => ordering == Ordering::Equal,
        }
    }
}

/// All the packages of some repositories, searchable.
pub struct Index {
    pub packages: Vec<Package>,
    /// Package indexes by name, and by what they provide.
    by_name: BTreeMap<String, Vec<usize>>,
    providers: BTreeMap<String, Vec<(usize, String)>>,
    /// The newest version of each package that has an `install_if`.
    automatic: Vec<usize>,
}

impl Index {
    pub fn new(packages: Vec<Package>) -> Self {
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut providers: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        for (index, package) in packages.iter().enumerate() {
            by_name.entry(package.name.clone()).or_default().push(index);
            for provided in &package.provides {
                let (name, version) = provided.split_once('=').unwrap_or((provided, package.version.as_str()));
                providers.entry(name.into()).or_default().push((index, version.into()));
            }
        }
        let mut index = Self { packages, by_name, providers, automatic: Vec::new() };
        index.automatic = index
            .by_name
            .values()
            .filter_map(|versions| {
                versions
                    .iter()
                    .copied()
                    .max_by(|&a, &b| compare_versions(&index.packages[a].version, &index.packages[b].version))
            })
            .filter(|&newest| !index.packages[newest].install_if.is_empty())
            .collect();
        index
    }

    /// The newest package called `name`.
    pub fn find(&self, name: &str) -> Option<&Package> {
        self.by_name
            .get(name)?
            .iter()
            .map(|&index| &self.packages[index])
            .max_by(|a, b| compare_versions(&a.version, &b.version))
    }

    /// The package that best satisfies a dependency: the newest one of
    /// that name, or else the most preferred (then newest) provider.
    pub fn best(&self, dependency: &Dependency) -> Option<usize> {
        let named = self
            .by_name
            .get(&dependency.name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&index| dependency.accepts(&self.packages[index].version));
        if let Some(index) =
            named.max_by(|&a, &b| compare_versions(&self.packages[a].version, &self.packages[b].version))
        {
            return Some(index);
        }
        self.providers
            .get(&dependency.name)?
            .iter()
            .filter(|(_, version)| dependency.accepts(version))
            .max_by(|(a, a_version), (b, b_version)| {
                self.packages[*a]
                    .priority
                    .cmp(&self.packages[*b].priority)
                    .then_with(|| compare_versions(a_version, b_version))
                    // Equal otherwise: the shorter (more basic) name.
                    .then_with(|| self.packages[*b].name.len().cmp(&self.packages[*a].name.len()))
            })
            .map(|(index, _)| *index)
    }

    /// Whether package `index` satisfies `dependency`.
    fn satisfies(&self, index: usize, dependency: &Dependency) -> bool {
        let package = &self.packages[index];
        if package.name == dependency.name {
            return dependency.accepts(&package.version);
        }
        package.provides.iter().any(|provided| {
            let (name, version) = provided.split_once('=').unwrap_or((provided, package.version.as_str()));
            name == dependency.name && dependency.accepts(version)
        })
    }

    /// Everything needed to install `wanted` (names, possibly with
    /// versions), dependencies first, plus whatever their `install_if`s
    /// bring along, as apk does. An error names what can't be found.
    pub fn resolve(&self, wanted: &[String]) -> Result<Vec<&Package>, String> {
        let mut chosen: Vec<usize> = Vec::new();
        let mut queue: VecDeque<(Dependency, String)> =
            wanted.iter().map(|text| (Dependency::parse(text), String::from("you"))).collect();
        loop {
            self.choose(&mut chosen, &mut queue)?;
            // Packages that come automatically with what was chosen; their
            // dependencies may in turn bring more.
            for &index in &self.automatic {
                let package = &self.packages[index];
                let wanted = package
                    .install_if
                    .iter()
                    .map(|text| Dependency::parse(text))
                    .all(|dependency| chosen.iter().any(|&other| self.satisfies(other, &dependency)));
                if wanted && !chosen.iter().any(|&other| self.packages[other].name == package.name) {
                    queue.push_back((Dependency::parse(&package.name), package.name.clone()));
                }
            }
            if queue.is_empty() {
                break;
            }
        }
        // Dependencies before what needs them: reverse discovery order is
        // close, then any package is moved after the ones it depends on.
        let mut order: Vec<usize> = Vec::new();
        let mut visiting = Vec::new();
        for &index in &chosen {
            self.visit(index, &chosen, &mut order, &mut visiting);
        }
        Ok(order.into_iter().map(|index| &self.packages[index]).collect())
    }

    /// Chooses packages for the queued dependencies and theirs.
    fn choose(&self, chosen: &mut Vec<usize>, queue: &mut VecDeque<(Dependency, String)>) -> Result<(), String> {
        while let Some((dependency, needed_by)) = queue.pop_front() {
            if dependency.conflict || chosen.iter().any(|&index| self.satisfies(index, &dependency)) {
                continue;
            }
            let index = self.best(&dependency).ok_or_else(|| {
                alloc::format!("{} (needed by {needed_by}) isn't in the repositories", dependency.name)
            })?;
            // Another version of the same package is already chosen: keep
            // the one picked first.
            if chosen.iter().any(|&other| self.packages[other].name == self.packages[index].name) {
                continue;
            }
            chosen.push(index);
            let package = &self.packages[index];
            for text in &package.depends {
                queue.push_back((Dependency::parse(text), package.name.clone()));
            }
        }
        Ok(())
    }

    fn visit(&self, index: usize, chosen: &[usize], order: &mut Vec<usize>, visiting: &mut Vec<usize>) {
        if order.contains(&index) || visiting.contains(&index) {
            return;
        }
        visiting.push(index);
        for text in &self.packages[index].depends {
            let dependency = Dependency::parse(text);
            if let Some(&provider) = chosen.iter().find(|&&other| self.satisfies(other, &dependency)) {
                self.visit(provider, chosen, order, visiting);
            }
        }
        visiting.pop();
        order.push(index);
    }

    /// Packages whose name or description contains `term` (any case).
    pub fn search(&self, term: &str) -> Vec<&Package> {
        let term = term.to_lowercase();
        let mut found: Vec<&Package> = self
            .by_name
            .keys()
            .filter_map(|name| self.find(name))
            .filter(|package| {
                package.name.to_lowercase().contains(&term) || package.description.to_lowercase().contains(&term)
            })
            .collect();
        // The package of that very name, then names starting with it, then
        // names containing it, then descriptions; each alphabetically.
        found.sort_by_key(|package| {
            let name = package.name.to_lowercase();
            let rank = if name == term {
                0
            } else if name.starts_with(&term) {
                1
            } else if name.contains(&term) {
                2
            } else {
                3
            };
            (rank, package.name.clone())
        });
        found
    }

    /// The packages that provide a command (`cmd:NAME`), for "command not
    /// found" hints.
    pub fn providing_command(&self, command: &str) -> Vec<&Package> {
        let wanted = alloc::format!("cmd:{command}");
        let mut found: Vec<&Package> =
            self.providers.get(&wanted).into_iter().flatten().map(|(index, _)| &self.packages[*index]).collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found.dedup_by(|a, b| a.name == b.name);
        found
    }
}

/// Splits a package's `name-version` into its parts (versions start at the
/// first `-` followed by a digit).
pub fn split_id(id: &str) -> Option<(&str, &str)> {
    let bytes = id.as_bytes();
    (1..bytes.len()).find(|&i| bytes[i - 1] == b'-' && bytes[i].is_ascii_digit()).map(|i| (&id[..i - 1], &id[i..]))
}
