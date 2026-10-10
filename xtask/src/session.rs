//! Scripted sessions: boot eliOS, type commands at the shell, and print a
//! clean transcript. For checking that things work end to end (and for
//! automated checks) without a person at the keyboard.
//!
//! Each step is one argument:
//!
//! - `text`: typed as a line at the shell; waits for the next prompt
//! - `@raw TEXT`: typed as is, with `\n`, `\r`, `\t`, `\e` and `\xNN` escapes
//! - `@wait TEXT`: waits until TEXT appears (say, a password prompt) in
//!   what was printed since the previous step started
//! - `@sleep SECONDS`
//! - `@key NAME`: a key on the PS/2 keyboard (QEMU names: `ret`, `f12`, `ctrl-c`...)
//! - `@screenshot PATH`: saves the screen as a PNG
//! - `@monitor COMMAND`: any QEMU monitor command (`device_add ...`)

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The end of the shell's prompt: `#` (root) or `$`, then the color reset,
/// then a space.
const PROMPT_ENDS: [&[u8]; 2] = [b"#\x1b[0m ", b"$\x1b[0m "];
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run(mut qemu: Command, steps: &[String], monitor: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(monitor);
    qemu.arg("-monitor").arg(format!("unix:{},server,nowait", monitor.display()));
    qemu.args(["-display", "none"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = qemu.spawn().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    let mut input = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let output = output.clone();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = stdout.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        })
    };
    let length = || output.lock().unwrap().len();
    // The line editor redraws the prompt as each key is typed, so only a
    // prompt after the newline that Enter produces counts.
    let prompt_since = |start: usize| {
        let data = output.lock().unwrap();
        let typed = &data[start.min(data.len())..];
        let Some(enter) = typed.iter().position(|&byte| byte == b'\n') else { return false };
        PROMPT_ENDS.iter().any(|end| typed[enter..].windows(end.len()).any(|window| window == *end))
    };
    let wait_for_prompt = |start: usize, timeout: Duration| {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && !prompt_since(start) {
            thread::sleep(Duration::from_millis(50));
        }
    };

    wait_for_prompt(0, BOOT_TIMEOUT);
    if !prompt_since(0) {
        eprintln!("xtask: no shell prompt within {}s", BOOT_TIMEOUT.as_secs());
    }
    let mut previous_start = length();
    for step in steps {
        let start = length();
        let result = if let Some(text) = step.strip_prefix("@wait ") {
            let deadline = Instant::now() + COMMAND_TIMEOUT;
            let appeared = || {
                let data = output.lock().unwrap();
                let since = &data[previous_start.min(data.len())..];
                since.windows(text.len()).any(|window| window == text.as_bytes())
            };
            while Instant::now() < deadline && !appeared() {
                thread::sleep(Duration::from_millis(50));
            }
            if !appeared() {
                eprintln!("xtask: `{text}` didn't appear within {}s", COMMAND_TIMEOUT.as_secs());
            }
            Ok(())
        } else if let Some(seconds) = step.strip_prefix("@sleep ") {
            let seconds: f64 = seconds.trim().parse().map_err(|_| format!("bad step `{step}`"))?;
            thread::sleep(Duration::from_secs_f64(seconds));
            Ok(())
        } else if let Some(key) = step.strip_prefix("@key ") {
            monitor_command(monitor, &format!("sendkey {key}"))
        } else if let Some(command) = step.strip_prefix("@monitor ") {
            monitor_command(monitor, command)
        } else if let Some(path) = step.strip_prefix("@screenshot ") {
            monitor_command(monitor, &format!("screendump {path} -f png"))
        } else if let Some(text) = step.strip_prefix("@raw ") {
            input.write_all(&unescape(text)).and_then(|_| input.flush()).map_err(|e| e.to_string()).map(|_| {
                thread::sleep(Duration::from_millis(500));
            })
        } else {
            input.write_all(format!("{step}\r").as_bytes()).and_then(|_| input.flush()).map_err(|e| e.to_string())?;
            wait_for_prompt(start, COMMAND_TIMEOUT);
            Ok(())
        };
        result?;
        if !step.starts_with("@wait ") {
            previous_start = start;
        }
    }
    thread::sleep(Duration::from_millis(300));
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    let _ = std::fs::remove_file(monitor);

    let data = output.lock().unwrap();
    print!("{}", transcript(&data));
    Ok(())
}

fn monitor_command(monitor: &Path, command: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(monitor).map_err(|e| format!("connecting to the QEMU monitor: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_millis(300))).ok();
    let mut greeting = [0; 4096];
    let _ = stream.read(&mut greeting);
    stream.write_all(format!("{command}\n").as_bytes()).map_err(|e| e.to_string())?;
    // Give QEMU time to act before the connection closes.
    thread::sleep(Duration::from_millis(800));
    Ok(())
}

fn unescape(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('t') => bytes.push(b'\t'),
            Some('e') => bytes.push(0x1B),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                bytes.push(u8::from_str_radix(&hex, 16).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other as u8),
            None => bytes.push(b'\\'),
        }
    }
    bytes
}

/// The output as a terminal would leave it: escape sequences removed, and
/// each line showing only what was written after its last carriage return
/// (the shell redraws the line as you type). Starts at the kernel's first
/// message; debug messages are left out.
fn transcript(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let start = text.find("eliOS 0.1.0 starting").map_or(0, |at| text[..at].rfind('\n').map_or(0, |n| n + 1));
    let mut plain = String::new();
    let mut chars = text[start..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            continue;
        }
        plain.push(c);
    }
    let mut result = String::new();
    for line in plain.split('\n') {
        let line = line.trim_end_matches('\r');
        let shown = line.rsplit('\r').next().unwrap_or(line);
        if !shown.contains("[debug]") {
            result.push_str(shown);
            result.push('\n');
        }
    }
    result
}
