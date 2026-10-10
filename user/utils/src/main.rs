//! eliutils: the eliOS shell, init and core commands in one binary.
//!
//! /bin holds a link per command, all pointing here; the name the program
//! was started under picks the command. `eliutils <command> ...` works too.

#![no_std]
#![no_main]

extern crate alloc;

mod applets;
mod commands;
mod edit;
mod editor;
mod init;
mod shell;

use alloc::string::String;
use alloc::vec::Vec;

use eli::{env, eprintln, println};

eli::main!(main);

fn main() -> i32 {
    let arguments = env::args();
    let name = env::program_name();
    if name == "eliutils" {
        if arguments.len() < 2 {
            println!("usage: eliutils <command> [arguments]");
            println!("commands: {}", applets::APPLETS.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(" "));
            return 0;
        }
        return run(&arguments[1], &arguments[1..]);
    }
    run(&name, &arguments)
}

fn run(name: &str, arguments: &[String]) -> i32 {
    // Login shells are started as "-sh".
    let name = name.trim_start_matches('-');
    match name {
        "sh" => shell::main(arguments),
        "init" => init::main(),
        "edit" => edit::main(arguments),
        _ => match commands::find(name) {
            Some(command) => command(arguments),
            None => {
                eprintln!("eliutils: no command named {name}");
                127
            }
        },
    }
}
