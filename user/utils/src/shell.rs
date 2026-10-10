//! sh: the eliOS shell.
//!
//! Runs commands typed at the prompt, from a script (`sh script`), or given
//! with `sh -c`. It understands pipelines (`a | b`), sequences (`;`, `&&`,
//! `||`), background jobs (`&`), redirections (`<`, `>`, `>>`, `2>`),
//! quoting, `$VAR`/`$?`/`$$` expansion, `~`, and `*`/`?` wildcards.
//!
//! Each pipeline runs in its own process group, which becomes the
//! terminal's foreground group while it runs: Ctrl+C then interrupts the
//! program, not the shell.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eli::fs::{self, FileType};
use eli::io;
use eli::process::{self, ExitStatus, SIGINT, SIGQUIT};
use eli::{env, eprintln, println, system, term};

use crate::editor::{Editor, Line};

const BUILTINS: &[&str] = &["cd", "exit", "export", "unset", "history", "source", ".", "jobs", "wait"];

pub fn main(arguments: &[String]) -> i32 {
    let mut shell = Shell::new();
    match arguments.get(1).map(String::as_str) {
        Some("-c") => match arguments.get(2) {
            Some(command) => {
                shell.positional = arguments[2..].to_vec();
                shell.run_line(command);
                shell.exit_code.unwrap_or(shell.status)
            }
            None => {
                eprintln!("sh: -c needs a command");
                2
            }
        },
        Some(script) => shell.run_script(script, &arguments[1..]),
        None => shell.interactive(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Read,
    Write,
    Append,
    /// `N>&M`: make descriptor N a copy of M (the target).
    Duplicate,
}

#[derive(Debug, Clone)]
struct Redirect {
    fd: i32,
    kind: RedirectKind,
    target: String,
}

#[derive(Debug)]
enum Token {
    Word(String),
    Pipe,
    And,
    Or,
    Separator,
    Background,
    Redirect { fd: i32, kind: RedirectKind },
}

#[derive(Debug, Default)]
struct Command {
    words: Vec<String>,
    redirects: Vec<Redirect>,
}

#[derive(Debug, Default)]
struct Pipeline {
    commands: Vec<Command>,
    background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connector {
    Always,
    /// Run only if the previous pipeline succeeded.
    And,
    /// Run only if it failed.
    Or,
}

struct Shell {
    /// The last pipeline's exit code, for `$?`.
    status: i32,
    history: Vec<String>,
    interactive: bool,
    /// Background processes not yet reported as finished.
    jobs: Vec<u32>,
    /// `$0`, `$1`, ...
    positional: Vec<String>,
    /// Set by `exit`.
    exit_code: Option<i32>,
}

impl Shell {
    fn new() -> Self {
        Self {
            status: 0,
            history: Vec::new(),
            interactive: false,
            jobs: Vec::new(),
            positional: alloc::vec!["sh".into()],
            exit_code: None,
        }
    }

    fn interactive(&mut self) -> i32 {
        self.interactive = term::is_terminal(io::STDIN);
        if self.interactive {
            process::ignore_signal(SIGINT);
            process::ignore_signal(SIGQUIT);
            let _ = process::set_group(0, 0);
            let _ = term::set_foreground_group(io::STDIN, process::id());
        }
        loop {
            self.report_finished_jobs();
            let prompt = self.prompt();
            match Editor::new(&prompt, &self.history).read(complete) {
                Line::Text(line) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && self.history.last().is_none_or(|last| last != trimmed) {
                        self.history.push(trimmed.to_string());
                    }
                    self.run_line(&line);
                }
                Line::Cancelled => self.status = 130,
                Line::EndOfInput => {
                    println!("exit");
                    return self.status;
                }
            }
            if let Some(code) = self.exit_code {
                return code;
            }
        }
    }

    fn prompt(&self) -> String {
        let cwd = fs::current_dir().unwrap_or_else(|_| "?".into());
        let home = env::var("HOME").unwrap_or_default();
        let shown = match cwd.strip_prefix(home.as_str()) {
            Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
            _ => cwd,
        };
        let marker = if self.status == 0 { "\x1b[1;32m#" } else { "\x1b[1;31m#" };
        format!("\x1b[1;36mroot@{}\x1b[0m:\x1b[1;34m{shown}\x1b[0m{marker}\x1b[0m ", system::uname().hostname)
    }

    fn run_script(&mut self, path: &str, arguments: &[String]) -> i32 {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("sh: {path}: {error}");
                return 127;
            }
        };
        self.positional = arguments.to_vec();
        for line in text.lines() {
            self.run_line(line);
            if let Some(code) = self.exit_code {
                return code;
            }
        }
        self.status
    }

    fn run_line(&mut self, line: &str) {
        match split(line).and_then(build) {
            Ok(list) => {
                for (connector, pipeline) in list {
                    let skip = match connector {
                        Connector::Always => false,
                        Connector::And => self.status != 0,
                        Connector::Or => self.status == 0,
                    };
                    if !skip {
                        self.run_pipeline(pipeline);
                    }
                    if self.exit_code.is_some() {
                        return;
                    }
                }
            }
            Err(message) => {
                eprintln!("sh: {message}");
                self.status = 2;
            }
        }
    }

    fn run_pipeline(&mut self, pipeline: Pipeline) {
        // Expansion happens now, not when the line was read: `$?` and
        // wildcards see the effects of earlier commands on the same line.
        let mut commands = Vec::new();
        for command in &pipeline.commands {
            match self.expand_command(command) {
                Ok(command) => commands.push(command),
                Err(message) => {
                    eprintln!("sh: {message}");
                    self.status = 2;
                    return;
                }
            }
        }
        let pipeline = Pipeline { commands, background: pipeline.background };
        if let [command] = pipeline.commands.as_slice()
            && !pipeline.background
        {
            if command.words.is_empty() {
                return;
            }
            if let Some((name, value)) = assignment(command) {
                env::set_var(name, value);
                self.status = 0;
                return;
            }
            if BUILTINS.contains(&command.words[0].as_str()) {
                self.status = self.builtin(&command.words);
                return;
            }
        }

        let mut pids = Vec::new();
        let mut group = 0;
        let mut input = None;
        let count = pipeline.commands.len();
        for (index, command) in pipeline.commands.iter().enumerate() {
            let (next_input, output) = if index + 1 < count {
                match io::pipe() {
                    Ok((read, write)) => (Some(read), Some(write)),
                    Err(error) => {
                        eprintln!("sh: can't make a pipe: {error}");
                        break;
                    }
                }
            } else {
                (None, None)
            };
            match process::fork() {
                Ok(0) => self.run_child(command, input, output, group),
                Ok(pid) => {
                    if group == 0 {
                        group = pid;
                    }
                    // Both sides set the group, so it's in place whichever
                    // runs first.
                    let _ = process::set_group(pid, group);
                    pids.push(pid);
                }
                Err(error) => {
                    eprintln!("sh: can't start a process: {error}");
                    break;
                }
            }
            for fd in [input, output].into_iter().flatten() {
                let _ = io::close(fd);
            }
            input = next_input;
        }
        if let Some(fd) = input {
            let _ = io::close(fd);
        }

        if pipeline.background {
            if let Some(&last) = pids.last() {
                println!("[{last}]");
            }
            self.jobs.extend(&pids);
            self.status = 0;
            return;
        }
        if self.interactive && group != 0 {
            let _ = term::set_foreground_group(io::STDIN, group);
        }
        let mut last_status = ExitStatus(0);
        for pid in &pids {
            if let Ok(Some((_, status))) = process::wait(*pid as i32, false) {
                last_status = status;
            }
        }
        if self.interactive {
            let _ = term::set_foreground_group(io::STDIN, process::id());
        }
        if let Some(signal) = last_status.signal()
            && signal != SIGINT
            && signal != 13
        {
            eprintln!("{}", process::signal_description(signal));
        }
        self.status = last_status.shell_code();
    }

    /// In a freshly forked child: wires up input and output, then runs the
    /// command.
    fn run_child(&mut self, command: &Command, input: Option<i32>, output: Option<i32>, group: u32) -> ! {
        let _ = process::set_group(0, group);
        process::default_signal(SIGINT);
        process::default_signal(SIGQUIT);
        if let Some(fd) = input {
            let _ = io::dup2(fd, io::STDIN);
        }
        if let Some(fd) = output {
            let _ = io::dup2(fd, io::STDOUT);
        }
        for redirect in &command.redirects {
            let file = match redirect.kind {
                RedirectKind::Read => fs::File::open(&redirect.target),
                RedirectKind::Write => fs::File::create(&redirect.target),
                RedirectKind::Append => fs::File::append(&redirect.target),
                RedirectKind::Duplicate => {
                    let Ok(source) = redirect.target.parse::<i32>() else {
                        eprintln!("sh: {}: not a file descriptor", redirect.target);
                        process::exit(1);
                    };
                    if let Err(error) = io::dup2(source, redirect.fd) {
                        eprintln!("sh: {source}: {error}");
                        process::exit(1);
                    }
                    continue;
                }
            };
            match file {
                Ok(file) => {
                    let _ = io::dup2(file.fd(), redirect.fd);
                }
                Err(error) => {
                    eprintln!("sh: {}: {error}", redirect.target);
                    process::exit(1);
                }
            }
        }

        let Some(name) = command.words.first() else { process::exit(0) };
        if BUILTINS.contains(&name.as_str()) {
            process::exit(self.builtin(&command.words));
        }
        let Some(path) = process::find_program(name) else {
            eprintln!("sh: {name}: command not found");
            process::exit(127);
        };
        let error = process::exec(&path, &command.words, &env::vars());
        eprintln!("sh: {name}: {error}");
        process::exit(126)
    }

    fn builtin(&mut self, words: &[String]) -> i32 {
        let argument = words.get(1).map(String::as_str);
        match words[0].as_str() {
            "cd" => {
                let home = env::var("HOME").unwrap_or_else(|| "/".into());
                let target = match argument {
                    None => home,
                    Some("-") => env::var("OLDPWD").unwrap_or(home),
                    Some(path) => path.into(),
                };
                let previous = fs::current_dir().unwrap_or_default();
                match fs::set_current_dir(&target) {
                    Ok(()) => {
                        env::set_var("OLDPWD", &previous);
                        env::set_var("PWD", &fs::current_dir().unwrap_or_default());
                        0
                    }
                    Err(error) => {
                        eprintln!("cd: {target}: {error}");
                        1
                    }
                }
            }
            "exit" => {
                let code = argument.and_then(|a| a.parse().ok()).unwrap_or(self.status);
                self.exit_code = Some(code);
                code
            }
            "export" => {
                if words.len() == 1 {
                    for variable in env::vars() {
                        println!("export {variable}");
                    }
                }
                for word in &words[1..] {
                    if let Some((name, value)) = word.split_once('=') {
                        env::set_var(name, value);
                    }
                }
                0
            }
            "unset" => {
                words[1..].iter().for_each(|name| env::remove_var(name));
                0
            }
            "history" => {
                for (index, line) in self.history.iter().enumerate() {
                    println!("{:>4}  {line}", index + 1);
                }
                0
            }
            "source" | "." => match argument {
                Some(path) => {
                    let saved = core::mem::take(&mut self.positional);
                    let status = self.run_script(path, &words[1..]);
                    self.positional = saved;
                    status
                }
                None => {
                    eprintln!("source: needs a file");
                    2
                }
            },
            "jobs" => {
                for pid in &self.jobs {
                    println!("[{pid}] running");
                }
                0
            }
            "wait" => {
                for pid in core::mem::take(&mut self.jobs) {
                    let _ = process::wait(pid as i32, false);
                }
                0
            }
            _ => 127,
        }
    }

    fn report_finished_jobs(&mut self) {
        self.jobs.retain(|&pid| match process::wait(pid as i32, true) {
            Ok(Some((_, status))) => {
                println!("[{pid}] done ({status})");
                false
            }
            Ok(None) => true,
            Err(_) => false,
        });
    }

    /// Expands one raw word, as typed, into the words it stands for:
    /// removes quotes and backslashes, substitutes `$` variables and `~`,
    /// and expands unquoted wildcards (which can produce several words).
    fn expand(&self, raw: &str) -> Result<Vec<String>, String> {
        let mut word = String::new();
        let mut wildcard = false;
        let mut chars = raw.chars().peekable();
        let mut first = true;
        while let Some(c) = chars.next() {
            match c {
                '\'' => loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("missing closing '".into()),
                    }
                },
                '"' => loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("missing closing \"".into()),
                        },
                        Some('$') => word.push_str(&self.expand_variable(&mut chars)),
                        Some(c) => word.push(c),
                        None => return Err("missing closing \"".into()),
                    }
                },
                '\\' => {
                    if let Some(c) = chars.next() {
                        word.push(c);
                    }
                }
                '$' => word.push_str(&self.expand_variable(&mut chars)),
                '~' if first && matches!(chars.peek(), None | Some('/')) => {
                    word.push_str(&env::var("HOME").unwrap_or_else(|| "/".into()));
                }
                '*' | '?' => {
                    wildcard = true;
                    word.push(c);
                }
                c => word.push(c),
            }
            first = false;
        }
        if wildcard {
            let matches = expand_wildcard(&word);
            if !matches.is_empty() {
                return Ok(matches);
            }
        }
        Ok(alloc::vec![word])
    }

    /// Expands every word and redirection target of a command.
    fn expand_command(&self, command: &Command) -> Result<Command, String> {
        let mut words = Vec::new();
        for raw in &command.words {
            words.extend(self.expand(raw)?);
        }
        let mut redirects = Vec::new();
        for redirect in &command.redirects {
            let target = self.expand(&redirect.target)?.join(" ");
            redirects.push(Redirect { target, ..redirect.clone() });
        }
        Ok(Command { words, redirects })
    }

    /// The value for a `$` expansion; the `$` has been consumed.
    fn expand_variable(&self, chars: &mut core::iter::Peekable<core::str::Chars>) -> String {
        match chars.peek().copied() {
            Some('?') => {
                chars.next();
                self.status.to_string()
            }
            Some('$') => {
                chars.next();
                process::id().to_string()
            }
            Some(digit @ '0'..='9') => {
                chars.next();
                let index = digit as usize - '0' as usize;
                self.positional.get(index).cloned().unwrap_or_default()
            }
            Some('{') => {
                chars.next();
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                env::var(&name).unwrap_or_default()
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if !(c.is_ascii_alphanumeric() || c == '_') {
                        break;
                    }
                    name.push(c);
                    chars.next();
                }
                env::var(&name).unwrap_or_default()
            }
            _ => "$".into(),
        }
    }
}

/// Splits a line into raw words and operators. Quotes are kept (they matter
/// for expansion later) but protect spaces and operators inside them.
fn split(line: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut chars = line.chars().peekable();

    fn finish(tokens: &mut Vec<Token>, word: &mut String) {
        if !word.is_empty() {
            tokens.push(Token::Word(core::mem::take(word)));
        }
    }

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => finish(&mut tokens, &mut word),
            '#' if word.is_empty() => break,
            '|' | '&' | ';' => {
                finish(&mut tokens, &mut word);
                let doubled = chars.peek() == Some(&c) && c != ';';
                if doubled {
                    chars.next();
                }
                tokens.push(match (c, doubled) {
                    ('|', true) => Token::Or,
                    ('|', false) => Token::Pipe,
                    ('&', true) => Token::And,
                    ('&', false) => Token::Background,
                    _ => Token::Separator,
                });
            }
            '>' | '<' => {
                // A number right before the operator (as in 2>) picks the
                // descriptor.
                let fd = if !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
                    let fd = word.parse().unwrap_or(1);
                    word.clear();
                    Some(fd)
                } else {
                    finish(&mut tokens, &mut word);
                    None
                };
                let kind = if c == '<' {
                    RedirectKind::Read
                } else if chars.peek() == Some(&'>') {
                    chars.next();
                    RedirectKind::Append
                } else if chars.peek() == Some(&'&') {
                    chars.next();
                    RedirectKind::Duplicate
                } else {
                    RedirectKind::Write
                };
                let default_fd = if c == '<' { 0 } else { 1 };
                tokens.push(Token::Redirect { fd: fd.unwrap_or(default_fd), kind });
            }
            quote @ ('\'' | '"') => {
                word.push(quote);
                loop {
                    match chars.next() {
                        Some(c) if c == quote => break,
                        Some('\\') if quote == '"' => {
                            word.push('\\');
                            word.extend(chars.next());
                        }
                        Some(c) => word.push(c),
                        None => return Err(format!("missing closing {quote}")),
                    }
                }
                word.push(quote);
            }
            '\\' => {
                word.push('\\');
                word.extend(chars.next());
            }
            c => word.push(c),
        }
    }
    finish(&mut tokens, &mut word);
    Ok(tokens)
}

/// `NAME=value` on its own sets a variable.
fn assignment(command: &Command) -> Option<(&str, &str)> {
    let [word] = command.words.as_slice() else { return None };
    let (name, value) = word.split_once('=')?;
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some((name, value))
}

/// Groups tokens into pipelines and the connectors between them.
fn build(tokens: Vec<Token>) -> Result<Vec<(Connector, Pipeline)>, String> {
    let mut list = Vec::new();
    let mut connector = Connector::Always;
    let mut pipeline = Pipeline::default();
    let mut command = Command::default();
    let mut tokens = tokens.into_iter().peekable();

    while let Some(token) = tokens.next() {
        match token {
            Token::Word(word) => command.words.push(word),
            Token::Redirect { fd, kind } => match tokens.next() {
                Some(Token::Word(target)) => command.redirects.push(Redirect { fd, kind, target }),
                _ => return Err("a redirection needs a file name".into()),
            },
            Token::Pipe => {
                if command.words.is_empty() {
                    return Err("nothing to pipe from".into());
                }
                pipeline.commands.push(core::mem::take(&mut command));
            }
            Token::Background | Token::Separator | Token::And | Token::Or => {
                if command.words.is_empty() && pipeline.commands.is_empty() {
                    if matches!(token, Token::Separator) {
                        continue;
                    }
                    return Err("syntax error: nothing before an operator".into());
                }
                if command.words.is_empty() {
                    return Err("syntax error: a pipe needs a command after it".into());
                }
                pipeline.commands.push(core::mem::take(&mut command));
                pipeline.background = matches!(token, Token::Background);
                list.push((connector, core::mem::take(&mut pipeline)));
                connector = match token {
                    Token::And => Connector::And,
                    Token::Or => Connector::Or,
                    _ => Connector::Always,
                };
            }
        }
    }
    if !command.words.is_empty() || !command.redirects.is_empty() {
        pipeline.commands.push(command);
    } else if !pipeline.commands.is_empty() {
        return Err("syntax error: a pipe needs a command after it".into());
    }
    if !pipeline.commands.is_empty() {
        list.push((connector, pipeline));
    } else if connector != Connector::Always {
        return Err("syntax error: a command must follow && or ||".into());
    }
    Ok(list)
}

/// Whether `name` matches a pattern with `*` (any run of characters) and
/// `?` (any one character).
fn matches(pattern: &[char], name: &[char]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some('*'), _) => matches(&pattern[1..], name) || (!name.is_empty() && matches(pattern, &name[1..])),
        (Some('?'), Some(_)) => matches(&pattern[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => matches(&pattern[1..], &name[1..]),
        _ => false,
    }
}

/// Expands a wildcard in the last path component, sorted. Files starting
/// with a dot only match patterns that do too.
fn expand_wildcard(word: &str) -> Vec<String> {
    let (directory, pattern) = match word.rfind('/') {
        Some(slash) => (&word[..=slash], &word[slash + 1..]),
        None => ("", word),
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let listing = fs::read_dir(if directory.is_empty() { "." } else { directory }).unwrap_or_default();
    listing
        .into_iter()
        .filter(|entry| !entry.name.starts_with('.') || pattern.first() == Some(&'.'))
        .filter(|entry| matches(&pattern, &entry.name.chars().collect::<Vec<_>>()))
        .map(|entry| format!("{directory}{}", entry.name))
        .collect()
}

/// Tab completion: command names for the first word, paths after that.
fn complete(before_cursor: &str) -> Vec<String> {
    let word_start = before_cursor.rfind([' ', '|', ';', '&']).map_or(0, |index| index + 1);
    let word = &before_cursor[word_start..];
    let earlier = before_cursor[..word_start].trim_end();
    let first_word = earlier.is_empty() || earlier.ends_with(['|', ';', '&']);

    let mut candidates: Vec<String> = Vec::new();
    if first_word && !word.contains('/') {
        candidates.extend(BUILTINS.iter().filter(|b| b.starts_with(word)).map(|b| b.to_string()));
        for entry in fs::read_dir("/bin").unwrap_or_default() {
            if entry.name.starts_with(word) {
                candidates.push(entry.name);
            }
        }
    } else {
        let (directory, prefix) = match word.rfind('/') {
            Some(slash) => (&word[..=slash], &word[slash + 1..]),
            None => ("", word),
        };
        for entry in fs::read_dir(if directory.is_empty() { "." } else { directory }).unwrap_or_default() {
            if entry.name.starts_with(prefix) && (!entry.name.starts_with('.') || prefix.starts_with('.')) {
                let is_directory =
                    fs::metadata(&format!("{directory}{}", entry.name)).is_ok_and(|m| m.kind == FileType::Directory);
                let suffix = if is_directory { "/" } else { "" };
                candidates.push(format!("{directory}{}{suffix}", entry.name));
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}
