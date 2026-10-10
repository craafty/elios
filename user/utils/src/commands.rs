//! The core commands.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::{self, FileType};
use eli::io::{self, Lines, STDIN, STDOUT};
use eli::process;
use eli::time::{self, DateTime};
use eli::{env, eprintln, print, println, system};

use crate::applets::APPLETS;

type Command = fn(&[String]) -> i32;

pub fn find(name: &str) -> Option<Command> {
    Some(match name {
        "adduser" => crate::accounts::adduser,
        "basename" => basename,
        "cat" => cat,
        "chgrp" => crate::accounts::chgrp,
        "chmod" => crate::accounts::chmod,
        "chown" => crate::accounts::chown,
        "clear" => clear,
        "cp" => cp,
        "date" => date,
        "deluser" => crate::accounts::deluser,
        "dmesg" => dmesg,
        "echo" => echo,
        "env" => env_command,
        "false" => |_| 1,
        "fetch" => fetch,
        "free" => free,
        "grep" => grep,
        "groups" => crate::accounts::groups,
        "head" => |args| head_or_tail(args, true),
        "hello" => hello,
        "help" => help,
        "host" => crate::network::host,
        "id" => crate::accounts::id,
        "ifconfig" => crate::network::ifconfig,
        "kill" => kill,
        "ln" => ln,
        "login" => crate::accounts::login,
        "ls" => ls,
        "mkdir" => mkdir,
        "mv" => mv,
        "nc" => crate::network::nc,
        "ping" => crate::network::ping,
        "poweroff" => |_| {
            eprintln!("poweroff: {}", process::power_off());
            1
        },
        "ps" => ps,
        "pwd" => pwd,
        "reboot" => |_| {
            eprintln!("reboot: {}", process::reboot());
            1
        },
        "rm" => rm,
        "rmdir" => rmdir,
        "seq" => seq,
        "sleep" => sleep,
        "stat" => stat,
        "strace" => strace,
        "tail" => |args| head_or_tail(args, false),
        "tee" => tee,
        "touch" => touch,
        "true" => |_| 0,
        "uname" => uname,
        "nproc" => nproc,
        "uptime" => uptime,
        "wc" => wc,
        "wget" => crate::network::wget,
        "whoami" => crate::accounts::whoami,
        "yes" => yes,
        _ => return None,
    })
}

/// Splits arguments into flags (`-x`, combinable as `-xy`) and the rest.
fn split_flags(arguments: &[String]) -> (String, Vec<&str>) {
    let mut flags = String::new();
    let mut rest = Vec::new();
    for argument in &arguments[1..] {
        match argument.strip_prefix('-') {
            Some(letters)
                if !letters.is_empty() && rest.is_empty() && !letters.starts_with(|c: char| c.is_ascii_digit()) =>
            {
                flags.push_str(letters)
            }
            _ => rest.push(argument.as_str()),
        }
    }
    (flags, rest)
}

fn report(command: &str, subject: &str, error: eli::Error) -> i32 {
    eprintln!("{command}: {subject}: {error}");
    1
}

/// Human-friendly sizes: 512, 4.0K, 1.5M...
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 { format!("{value:.1}{}", UNITS[unit]) } else { format!("{:.0}{}", value, UNITS[unit]) }
}

fn echo(arguments: &[String]) -> i32 {
    let (newline, words) = match arguments.get(1).map(String::as_str) {
        Some("-n") => (false, &arguments[2..]),
        _ => (true, &arguments[1..]),
    };
    let mut text = words.join(" ");
    if newline {
        text.push('\n');
    }
    print!("{text}");
    0
}

/// Calls `f` with the name and contents of each file (standard input for
/// none, or `-`).
fn for_each_input(command: &str, files: &[&str], mut f: impl FnMut(&str, &[u8])) -> i32 {
    let mut status = 0;
    let inputs: &[&str] = if files.is_empty() { &["-"] } else { files };
    for &file in inputs {
        let data = if file == "-" { io::read_to_end(STDIN) } else { fs::read(file) };
        match data {
            Ok(data) => f(file, &data),
            Err(error) => status = report(command, file, error),
        }
    }
    status
}

fn cat(arguments: &[String]) -> i32 {
    let (_, files) = split_flags(arguments);
    for_each_input("cat", &files, |_, data| {
        let _ = io::write_all(STDOUT, data);
    })
}

fn clear(_: &[String]) -> i32 {
    print!("\x1b[2J\x1b[H");
    0
}

fn pwd(_: &[String]) -> i32 {
    match fs::current_dir() {
        Ok(cwd) => {
            println!("{cwd}");
            0
        }
        Err(error) => report("pwd", ".", error),
    }
}

fn type_color(kind: FileType, path: &str) -> &'static str {
    match kind {
        FileType::Directory => "\x1b[1;34m",
        FileType::Symlink => "\x1b[1;36m",
        FileType::CharDevice => "\x1b[1;33m",
        FileType::Pipe => "\x1b[33m",
        _ if path.starts_with("/bin/") => "\x1b[1;32m",
        _ => "",
    }
}

fn ls(arguments: &[String]) -> i32 {
    let (flags, paths) = split_flags(arguments);
    let long = flags.contains('l');
    let all = flags.contains('a');
    let paths = if paths.is_empty() { alloc::vec!["."] } else { paths };
    let owners = long.then(crate::accounts::Names::load);
    let owners = owners.as_ref();
    let mut status = 0;
    let show_headers = paths.len() > 1;
    for (index, &path) in paths.iter().enumerate() {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                status = report("ls", path, error);
                continue;
            }
        };
        if metadata.kind != FileType::Directory {
            print_entry(path, path, owners);
            continue;
        }
        if show_headers {
            println!("{}{path}:", if index > 0 { "\n" } else { "" });
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                status = report("ls", path, error);
                continue;
            }
        };
        let names: Vec<(String, String)> = entries
            .iter()
            .filter(|entry| all || !entry.name.starts_with('.'))
            .map(|entry| {
                let full = if path == "/" { format!("/{}", entry.name) } else { format!("{path}/{}", entry.name) };
                (entry.name.clone(), full)
            })
            .collect();
        if long {
            for (name, full) in &names {
                print_entry(name, full, owners);
            }
        } else {
            print_columns(&names);
        }
    }
    status
}

/// One `ls` line; with `names` (for `-l`), the long form with permissions,
/// owner, group, size and date.
fn print_entry(name: &str, full: &str, names: Option<&crate::accounts::Names>) {
    let Ok(metadata) = fs::symlink_metadata(full) else { return };
    let color = type_color(metadata.kind, full);
    let Some(names) = names else {
        println!("{color}{name}\x1b[0m");
        return;
    };
    let kind = match metadata.kind {
        FileType::Directory => 'd',
        FileType::Symlink => 'l',
        FileType::CharDevice => 'c',
        FileType::BlockDevice => 'b',
        FileType::Pipe => 'p',
        FileType::Socket => 's',
        _ => '-',
    };
    let date = DateTime::from_unix(metadata.modified.max(0) as u64);
    let target = if metadata.kind == FileType::Symlink {
        fs::read_link(full).map(|target| format!(" -> {target}")).unwrap_or_default()
    } else {
        String::new()
    };
    println!(
        "{kind}{} {:<8} {:<8} {:>6} {:04}-{:02}-{:02} {:02}:{:02} {color}{name}\x1b[0m{target}",
        permissions(metadata.mode),
        names.user(metadata.uid),
        names.group(metadata.gid),
        human_size(metadata.size),
        date.year,
        date.month,
        date.day,
        date.hour,
        date.minute,
    );
}

/// `rwxr-xr-x`, with `s`/`S` for setuid and setgid and `t`/`T` for sticky
/// in the execute places (lowercase when execute is set too).
fn permissions(mode: u32) -> String {
    (0..9)
        .map(|bit| {
            let set = mode & (1 << (8 - bit)) != 0;
            let special = match bit {
                2 => mode & 0o4000 != 0,
                5 => mode & 0o2000 != 0,
                8 => mode & 0o1000 != 0,
                _ => false,
            };
            match (special, set) {
                (true, _) => {
                    let letter = if bit == 8 { 't' } else { 's' };
                    if set { letter } else { letter.to_ascii_uppercase() }
                }
                (false, false) => '-',
                (false, true) => ['r', 'w', 'x'][bit % 3],
            }
        })
        .collect()
}

/// Names in columns across the terminal, like ls does.
fn print_columns(names: &[(String, String)]) {
    if names.is_empty() {
        return;
    }
    let (columns, _) = eli::term::size(STDOUT);
    let width = names.iter().map(|(name, _)| name.chars().count()).max().unwrap_or(0) + 2;
    let per_row = (usize::from(columns) / width).max(1);
    let rows = names.len().div_ceil(per_row);
    for row in 0..rows {
        let mut line = String::new();
        for column in 0..per_row {
            let Some((name, full)) = names.get(column * rows + row) else { break };
            let kind = fs::symlink_metadata(full).map(|m| m.kind).unwrap_or(FileType::Other);
            let padding = width - name.chars().count();
            line.push_str(&format!("{}{name}\x1b[0m{:padding$}", type_color(kind, full), ""));
        }
        println!("{}", line.trim_end());
    }
}

fn mkdir(arguments: &[String]) -> i32 {
    let (flags, paths) = split_flags(arguments);
    let parents = flags.contains('p');
    let mut status = 0;
    for path in paths {
        if parents {
            let mut built = String::new();
            for component in path.split('/') {
                built.push_str(component);
                if !component.is_empty()
                    && !fs::exists(&built)
                    && let Err(error) = fs::create_dir(&built)
                {
                    status = report("mkdir", &built, error);
                    break;
                }
                built.push('/');
            }
        } else if let Err(error) = fs::create_dir(path) {
            status = report("mkdir", path, error);
        }
    }
    status
}

fn rmdir(arguments: &[String]) -> i32 {
    let mut status = 0;
    for path in &arguments[1..] {
        if let Err(error) = fs::remove_dir(path) {
            status = report("rmdir", path, error);
        }
    }
    status
}

pub fn remove_tree(path: &str) -> eli::Result<()> {
    if fs::symlink_metadata(path)?.kind == FileType::Directory {
        for entry in fs::read_dir(path)? {
            remove_tree(&format!("{path}/{}", entry.name))?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

fn rm(arguments: &[String]) -> i32 {
    let (flags, paths) = split_flags(arguments);
    let recursive = flags.contains('r') || flags.contains('R');
    let force = flags.contains('f');
    let mut status = 0;
    for path in paths {
        let result = if recursive { remove_tree(path) } else { fs::remove_file(path) };
        if let Err(error) = result
            && !(force && error == eli::Error::NOT_FOUND)
        {
            status = report("rm", path, error);
        }
    }
    status
}

fn touch(arguments: &[String]) -> i32 {
    let mut status = 0;
    for path in &arguments[1..] {
        let result = if fs::exists(path) { fs::set_modified(path, None) } else { fs::File::append(path).map(|_| ()) };
        if let Err(error) = result {
            status = report("touch", path, error);
        }
    }
    status
}

/// For cp and mv: copying into a directory keeps the file's name.
fn destination(source: &str, target: &str) -> String {
    if fs::metadata(target).is_ok_and(|m| m.kind == FileType::Directory) {
        let name = source.trim_end_matches('/').rsplit('/').next().unwrap_or(source);
        format!("{}/{name}", target.trim_end_matches('/'))
    } else {
        target.to_string()
    }
}

fn cp(arguments: &[String]) -> i32 {
    let (_, paths) = split_flags(arguments);
    let Some((&target, sources)) = paths.split_last().filter(|(_, sources)| !sources.is_empty()) else {
        eprintln!("usage: cp SOURCE... DESTINATION");
        return 2;
    };
    let mut status = 0;
    for &source in sources {
        if let Err(error) = fs::copy(source, &destination(source, target)) {
            status = report("cp", source, error);
        }
    }
    status
}

fn mv(arguments: &[String]) -> i32 {
    let (_, paths) = split_flags(arguments);
    let Some((&target, sources)) = paths.split_last().filter(|(_, sources)| !sources.is_empty()) else {
        eprintln!("usage: mv SOURCE... DESTINATION");
        return 2;
    };
    let mut status = 0;
    for &source in sources {
        let destination = destination(source, target);
        let result = match fs::rename(source, &destination) {
            // Different file systems (say, the disk and memory): copy, then
            // delete the original.
            Err(eli::Error::CROSS_DEVICE) => {
                fs::copy(source, &destination).and_then(|_| fs::remove_file(source)).map(|_| ())
            }
            result => result,
        };
        if let Err(error) = result {
            status = report("mv", source, error);
        }
    }
    status
}

fn ln(arguments: &[String]) -> i32 {
    let (flags, paths) = split_flags(arguments);
    if !flags.contains('s') || paths.len() != 2 {
        eprintln!("usage: ln -s TARGET LINK (only symbolic links are supported)");
        return 2;
    }
    match fs::symlink(paths[0], paths[1]) {
        Ok(()) => 0,
        Err(error) => report("ln", paths[1], error),
    }
}

fn stat(arguments: &[String]) -> i32 {
    let mut status = 0;
    for path in &arguments[1..] {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                let date = DateTime::from_unix(metadata.modified.max(0) as u64);
                println!("  File: {path}");
                println!("  Type: {:?}", metadata.kind);
                println!("  Size: {} bytes", metadata.size);
                println!(" Inode: {}", metadata.inode);
                println!("  Mode: {:o} ({})", metadata.permissions(), permissions(metadata.mode));
                println!(
                    " Owner: {} ({})   Group: {} ({})   Links: {}",
                    eli::users::user_name(metadata.uid),
                    metadata.uid,
                    eli::users::group_name(metadata.gid),
                    metadata.gid,
                    metadata.links
                );
                println!(
                    "Modify: {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                    date.year, date.month, date.day, date.hour, date.minute, date.second
                );
            }
            Err(error) => status = report("stat", path, error),
        }
    }
    status
}

fn strace(arguments: &[String]) -> i32 {
    let Some(name) = arguments.get(1) else {
        eprintln!("usage: strace PROGRAM [ARGUMENTS...]");
        return 2;
    };
    let Some(path) = process::find_program(name) else {
        eprintln!("strace: {name}: command not found");
        return 127;
    };
    match process::fork() {
        Ok(0) => {
            let _ = process::trace(true);
            let error = process::exec(&path, &arguments[1..], &env::vars());
            eprintln!("strace: {name}: {error}");
            process::exit(126)
        }
        Ok(pid) => match process::wait(pid as i32, false) {
            Ok(Some((_, status))) => {
                eprintln!("strace: {name} ended: {status}");
                status.shell_code()
            }
            _ => 1,
        },
        Err(error) => report("strace", name, error),
    }
}

fn basename(arguments: &[String]) -> i32 {
    let Some(path) = arguments.get(1) else {
        eprintln!("usage: basename PATH [SUFFIX]");
        return 2;
    };
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    let name = arguments.get(2).and_then(|suffix| name.strip_suffix(suffix.as_str())).unwrap_or(name);
    println!("{name}");
    0
}

fn head_or_tail(arguments: &[String], head: bool) -> i32 {
    let command = if head { "head" } else { "tail" };
    let mut count = 10;
    let mut files = Vec::new();
    let mut words = arguments[1..].iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-n" => count = words.next().and_then(|n| n.parse().ok()).unwrap_or(10),
            flag if flag.starts_with('-') && flag.len() > 1 => count = flag[1..].parse().unwrap_or(10),
            file => files.push(file),
        }
    }
    if head {
        return head_streaming(&files, count);
    }
    for_each_input(command, &files, |_, data| {
        let text = String::from_utf8_lossy(data);
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(count)..] {
            println!("{line}");
        }
    })
}

/// `head` reads only as far as it needs to, so it works on endless input
/// (`yes | head`).
fn head_streaming(files: &[&str], count: usize) -> i32 {
    let inputs: &[&str] = if files.is_empty() { &["-"] } else { files };
    let mut status = 0;
    for &name in inputs {
        let file = if name == "-" { None } else { Some(fs::File::open(name)) };
        let fd = match &file {
            None => STDIN,
            Some(Ok(file)) => file.fd(),
            Some(Err(error)) => {
                status = report("head", name, *error);
                continue;
            }
        };
        let mut lines = Lines::new(fd);
        for _ in 0..count {
            match lines.next_line() {
                Ok(Some(line)) => println!("{line}"),
                _ => break,
            }
        }
    }
    status
}

fn wc(arguments: &[String]) -> i32 {
    let (flags, files) = split_flags(arguments);
    // With no flags, show all three counts.
    let show = |flag: char| flags.is_empty() || flags.contains(flag);
    let format_counts = |counts: (usize, usize, usize), label: &str| {
        let mut line = String::new();
        for (flag, count) in [('l', counts.0), ('w', counts.1), ('c', counts.2)] {
            if show(flag) {
                line.push_str(&format!("{count:>7} "));
            }
        }
        line.push_str(label);
        line
    };
    let mut totals = (0, 0, 0);
    let names: Vec<&str> = if files.is_empty() { alloc::vec!["-"] } else { files.clone() };
    let mut status = 0;
    for name in &names {
        let data = if *name == "-" { io::read_to_end(STDIN) } else { fs::read(name) };
        match data {
            Ok(data) => {
                let text = String::from_utf8_lossy(&data);
                let counts = (text.lines().count(), text.split_whitespace().count(), data.len());
                totals = (totals.0 + counts.0, totals.1 + counts.1, totals.2 + counts.2);
                let label = if *name == "-" { "" } else { name };
                println!("{}", format_counts(counts, label).trim_end());
            }
            Err(error) => status = report("wc", name, error),
        }
    }
    if names.len() > 1 {
        println!("{}", format_counts(totals, "total"));
    }
    status
}

fn grep(arguments: &[String]) -> i32 {
    let (flags, rest) = split_flags(arguments);
    let Some((&pattern, files)) = rest.split_first() else {
        eprintln!("usage: grep [-i] [-v] [-n] PATTERN [FILE...]");
        return 2;
    };
    let ignore_case = flags.contains('i');
    let invert = flags.contains('v');
    let numbers = flags.contains('n');
    let pattern = if ignore_case { pattern.to_lowercase() } else { pattern.to_string() };
    let multiple = files.len() > 1;
    let mut found = false;
    let status = for_each_input("grep", files, |name, data| {
        let text = String::from_utf8_lossy(data);
        for (number, line) in text.lines().enumerate() {
            let haystack = if ignore_case { line.to_lowercase() } else { line.to_string() };
            if haystack.contains(&pattern) != invert {
                found = true;
                let file = if multiple { format!("\x1b[35m{name}\x1b[0m:") } else { String::new() };
                let number = if numbers { format!("\x1b[32m{}\x1b[0m:", number + 1) } else { String::new() };
                println!("{file}{number}{line}");
            }
        }
    });
    if status != 0 {
        2
    } else if found {
        0
    } else {
        1
    }
}

fn tee(arguments: &[String]) -> i32 {
    let (flags, paths) = split_flags(arguments);
    let mut files = Vec::new();
    for path in paths {
        let file = if flags.contains('a') { fs::File::append(path) } else { fs::File::create(path) };
        match file {
            Ok(file) => files.push(file),
            Err(error) => return report("tee", path, error),
        }
    }
    let mut buffer = [0; 4096];
    loop {
        match io::read(STDIN, &mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(count) => {
                let _ = io::write_all(STDOUT, &buffer[..count]);
                for file in &files {
                    let _ = file.write_all(&buffer[..count]);
                }
            }
        }
    }
}

fn seq(arguments: &[String]) -> i32 {
    let numbers: Vec<i64> = arguments[1..].iter().filter_map(|a| a.parse().ok()).collect();
    let (first, last) = match numbers.as_slice() {
        [last] => (1, *last),
        [first, last, ..] => (*first, *last),
        _ => {
            eprintln!("usage: seq [FIRST] LAST");
            return 2;
        }
    };
    for n in first..=last {
        println!("{n}");
    }
    0
}

fn yes(arguments: &[String]) -> i32 {
    let text = if arguments.len() > 1 { arguments[1..].join(" ") } else { "y".into() };
    let line = format!("{text}\n").repeat(64);
    while io::write_all(STDOUT, line.as_bytes()).is_ok() {}
    0
}

fn sleep(arguments: &[String]) -> i32 {
    let Some(seconds) = arguments.get(1).and_then(|s| s.parse::<f64>().ok()) else {
        eprintln!("usage: sleep SECONDS");
        return 2;
    };
    time::sleep_ms((seconds * 1000.0) as u64);
    0
}

fn date(_: &[String]) -> i32 {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let now = DateTime::from_unix(time::unix_time());
    println!(
        "{} {} {:2} {:02}:{:02}:{:02} UTC {}",
        DAYS[now.weekday as usize],
        MONTHS[now.month as usize - 1],
        now.day,
        now.hour,
        now.minute,
        now.second,
        now.year
    );
    0
}

fn dmesg(_: &[String]) -> i32 {
    match process::kernel_log() {
        Ok(log) => {
            print!("{log}");
            0
        }
        Err(error) => report("dmesg", "kernel log", error),
    }
}

fn env_command(_: &[String]) -> i32 {
    for variable in env::vars() {
        println!("{variable}");
    }
    0
}

fn kill(arguments: &[String]) -> i32 {
    let mut signal = process::SIGTERM;
    let mut targets = &arguments[1..];
    if let Some(first) = targets.first()
        && let Some(name) = first.strip_prefix('-')
    {
        match process::signal_number(name) {
            Some(number) => signal = number,
            None => {
                eprintln!("kill: unknown signal {name}");
                return 2;
            }
        }
        targets = &targets[1..];
    }
    if targets.is_empty() {
        eprintln!("usage: kill [-SIGNAL] PID...");
        return 2;
    }
    let mut status = 0;
    for target in targets {
        match target.parse::<i32>() {
            Ok(pid) => {
                if let Err(error) = process::kill(pid, signal) {
                    status = report("kill", target, error);
                }
            }
            Err(_) => {
                eprintln!("kill: {target}: not a process id");
                status = 1;
            }
        }
    }
    status
}

fn ps(_: &[String]) -> i32 {
    match process::list() {
        Ok(processes) => {
            let names = crate::accounts::Names::load();
            println!("\x1b[1mUSER       PID  PPID S     TIME    MEM COMMAND\x1b[0m");
            for p in processes {
                let seconds = p.cpu_ms / 1000;
                println!(
                    "{:<8} {:>5} {:>5} {} {:>4}:{:02}.{} {:>6} {}",
                    names.user(p.uid),
                    p.pid,
                    p.parent,
                    p.state,
                    seconds / 60,
                    seconds % 60,
                    p.cpu_ms % 1000 / 100,
                    human_size(p.memory),
                    p.name
                );
            }
            0
        }
        Err(error) => report("ps", "process list", error),
    }
}

fn free(_: &[String]) -> i32 {
    let info = time::system_info();
    let used = info.total_memory - info.free_memory;
    println!("\x1b[1m          total      used      free\x1b[0m");
    println!(
        "Memory: {:>9} {:>9} {:>9}",
        human_size(info.total_memory),
        human_size(used),
        human_size(info.free_memory)
    );
    0
}

fn format_duration(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min, {} s", seconds % 60),
        (0, _) => format!("{hours}:{minutes:02}"),
        _ => format!("{days} days, {hours}:{minutes:02}"),
    }
}

fn uptime(_: &[String]) -> i32 {
    let info = time::system_info();
    println!("up {}, {} processes", format_duration(info.uptime_seconds), info.processes);
    0
}

fn nproc(_arguments: &[String]) -> i32 {
    println!("{}", system::cpu_count());
    0
}

fn uname(arguments: &[String]) -> i32 {
    let (flags, _) = split_flags(arguments);
    let info = system::uname();
    let fields =
        [('s', &info.system), ('n', &info.hostname), ('r', &info.release), ('v', &info.version), ('m', &info.machine)];
    let selected: Vec<&str> = fields
        .iter()
        .filter(|(flag, _)| flags.contains('a') || flags.contains(*flag))
        .map(|(_, value)| value.as_str())
        .collect();
    println!("{}", if selected.is_empty() { info.system.clone() } else { selected.join(" ") });
    0
}

fn hello(_: &[String]) -> i32 {
    println!("Hello from eliOS userspace! I'm process {}, child of {}.", process::id(), process::parent_id());
    0
}

fn help(_: &[String]) -> i32 {
    println!("\x1b[1meliOS commands\x1b[0m (each is a program in /bin):\n");
    let auth = crate::applets::AUTH_APPLETS.iter();
    for (name, description) in APPLETS.iter().chain(auth).filter(|(name, _)| *name != "init") {
        println!("  \x1b[1;32m{name:<10}\x1b[0m {description}");
    }
    println!("\nShell built-ins: cd, exit, logout, export, unset, history, source, jobs, wait");
    println!("The shell supports pipes (|), redirection (< > >> 2>), && || ; and &,");
    println!("$VARIABLES, 'quotes', and * wildcards. Tab completes; arrows walk history.");
    0
}

/// The processor's name, straight from CPUID (user programs may run it).
fn cpu_name() -> String {
    let mut bytes = Vec::new();
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let (eax, ecx, edx): (u32, u32, u32);
        let rbx: u64;
        // SAFETY: cpuid exists on every x86_64 CPU; rbx (reserved by LLVM)
        // is saved and restored.
        unsafe {
            core::arch::asm!(
                "mov {rbx}, rbx",
                "cpuid",
                "xchg {rbx}, rbx",
                rbx = out(reg) rbx,
                inout("eax") leaf => eax,
                inout("ecx") 0 => ecx,
                out("edx") edx,
            );
        }
        for register in [eax, rbx as u32, ecx, edx] {
            bytes.extend_from_slice(&register.to_le_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).trim_matches(char::from(0)).trim().to_string()
}

fn fetch(_: &[String]) -> i32 {
    let info = time::system_info();
    let name = system::uname();
    let (columns, rows) = eli::term::size(STDOUT);
    let logo = [
        "\x1b[1;37m      _ _ \x1b[1;36m___  ____  ",
        "\x1b[1;37m  ___| (_)\x1b[1;36m _ \\/ ___| ",
        "\x1b[1;37m / _ \\ | |\x1b[1;36m | | \\___ \\ ",
        "\x1b[1;37m|  __/ | |\x1b[1;36m |_| |___) |",
        "\x1b[1;37m \\___|_|_|\x1b[1;36m\\___/|____/ ",
        "",
        "",
        "",
    ];
    let details = [
        format!("\x1b[1;36mroot\x1b[0m@\x1b[1;36m{}\x1b[0m", name.hostname),
        String::from("--------------"),
        format!("\x1b[1;36mOS\x1b[0m: {} ({})", name.version, name.machine),
        format!("\x1b[1;36mKernel ABI\x1b[0m: Linux-compatible ({})", name.release),
        format!("\x1b[1;36mUptime\x1b[0m: {}", format_duration(info.uptime_seconds)),
        format!("\x1b[1;36mProcesses\x1b[0m: {}", info.processes),
        format!(
            "\x1b[1;36mMemory\x1b[0m: {} / {}",
            human_size(info.total_memory - info.free_memory),
            human_size(info.total_memory)
        ),
        format!("\x1b[1;36mCPU\x1b[0m: {} (x{})", cpu_name(), system::cpu_count()),
        format!("\x1b[1;36mTerminal\x1b[0m: {columns}x{rows}"),
    ];
    // Pad the art to a fixed width, counting only visible characters.
    let visible = |text: &str| {
        let mut width = 0;
        let mut escape = false;
        for c in text.chars() {
            match (escape, c) {
                (false, '\x1b') => escape = true,
                (true, 'm') => escape = false,
                (true, _) => {}
                (false, _) => width += 1,
            }
        }
        width
    };
    for (index, detail) in details.iter().enumerate() {
        let art = logo.get(index).copied().unwrap_or("");
        let padding = " ".repeat(25usize.saturating_sub(visible(art)));
        println!("{art}\x1b[0m{padding}{detail}");
    }
    println!();
    0
}
