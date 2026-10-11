//! init: the first user process. It runs /etc/rc if there is one, then
//! keeps a login session going on the console: `login` asks who you are
//! and starts your shell, and when you log out, it asks again. As process 1
//! it also collects orphaned processes when they exit.
//!
//! On the live system, /etc/autologin names an account (root) to log in
//! automatically the first time, so the system starts at a prompt.

use alloc::string::String;
use alloc::vec::Vec;

use eli::process::{self, SIGINT, SIGQUIT, SIGTERM};
use eli::{env, eprintln, fs, time};

const SHELL: &str = "/bin/sh";
const LOGIN: &str = "/bin/login";
const ELI: &str = "/bin/eli";

pub fn main() -> i32 {
    if process::id() != 1 {
        eprintln!("init: only the kernel starts init");
        return 1;
    }
    // Ctrl+C and friends are for the programs the user runs, not for init.
    for signal in [SIGINT, SIGQUIT, SIGTERM] {
        process::ignore_signal(signal);
    }

    // The system as /etc/system.conf describes it: accounts, hostname,
    // packages (see the `eli` command).
    if let Ok(pid) = spawn(ELI, &["eli".into(), "boot".into()]) {
        wait_for(pid);
    }
    if fs::exists("/etc/rc")
        && let Ok(pid) = spawn(SHELL, &[SHELL.into(), "/etc/rc".into()])
    {
        wait_for(pid);
    }

    let mut autologin = crate::accounts::autologin_user();
    loop {
        let started = time::monotonic_nanos();
        let arguments: Vec<String> = match autologin.take() {
            Some(user) => alloc::vec!["login".into(), "-f".into(), user],
            None => alloc::vec!["login".into()],
        };
        match spawn(LOGIN, &arguments) {
            Ok(session) => wait_for(session),
            Err(error) => eprintln!("init: can't start {LOGIN}: {error}"),
        }
        // A session that ends immediately would otherwise respawn in a
        // busy loop.
        if time::monotonic_nanos() - started < 1_000_000_000 {
            time::sleep_ms(1000);
        }
    }
}

/// Starts a program in a child process.
fn spawn(program: &str, arguments: &[String]) -> eli::Result<u32> {
    let pid = process::fork()?;
    if pid == 0 {
        for signal in [SIGINT, SIGQUIT, SIGTERM] {
            process::default_signal(signal);
        }
        let error = process::exec(program, arguments, &env::vars());
        eprintln!("init: can't run {program}: {error}");
        process::exit(127);
    }
    Ok(pid)
}

/// Waits for one process, collecting any orphans that exit meanwhile.
fn wait_for(pid: u32) {
    loop {
        match process::wait(-1, false) {
            Ok(Some((exited, _))) if exited == pid => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}
