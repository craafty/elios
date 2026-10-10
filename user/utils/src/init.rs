//! init: the first user process. It greets the user, runs /etc/rc if there
//! is one, and keeps a shell running on the console. As process 1 it also
//! collects orphaned processes when they exit.

use alloc::string::String;

use eli::process::{self, SIGINT, SIGQUIT, SIGTERM};
use eli::{env, eprintln, fs, println, time};

const SHELL: &str = "/bin/sh";

pub fn main() -> i32 {
    if process::id() != 1 {
        eprintln!("init: only the kernel starts init");
        return 1;
    }
    // Ctrl+C and friends are for the programs the user runs, not for init.
    for signal in [SIGINT, SIGQUIT, SIGTERM] {
        process::ignore_signal(signal);
    }

    if let Ok(motd) = fs::read_to_string("/etc/motd") {
        eli::print!("{motd}");
    }
    if fs::exists("/etc/rc")
        && let Ok(pid) = spawn(&[SHELL.into(), "/etc/rc".into()])
    {
        wait_for(pid);
    }

    loop {
        let started = time::monotonic_nanos();
        match spawn(&["-sh".into()]) {
            Ok(shell) => wait_for(shell),
            Err(error) => eprintln!("init: can't start {SHELL}: {error}"),
        }
        // A shell that exits immediately would otherwise respawn in a busy
        // loop.
        if time::monotonic_nanos() - started < 1_000_000_000 {
            time::sleep_ms(1000);
        }
        println!("\n[init] starting a new shell");
    }
}

/// Starts the shell (or a script) in a child process.
fn spawn(arguments: &[String]) -> eli::Result<u32> {
    let pid = process::fork()?;
    if pid == 0 {
        for signal in [SIGINT, SIGQUIT, SIGTERM] {
            process::default_signal(signal);
        }
        if let Some(home) = env::var("HOME") {
            let _ = fs::set_current_dir(&home);
        }
        let error = process::exec(SHELL, arguments, &env::vars());
        eprintln!("init: can't run {SHELL}: {error}");
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
