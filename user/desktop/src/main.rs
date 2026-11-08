//! desktop: eliOS's graphical desktop, a small Wayland compositor.
//!
//! It takes over the screen (`/dev/fb0`) and the keyboards and mice
//! (`/dev/input/event*`, grabbed so the text console stops seeing keys),
//! listens on `$XDG_RUNTIME_DIR/wayland-0`, and runs programs that speak
//! Wayland: a terminal, Firefox. Everything happens in one event loop
//! around `epoll`; a timer ticks 60 times a second to redraw and tell
//! clients to draw their next frame.
//!
//! `desktop [PROGRAM ARGS...]` starts PROGRAM once the desktop is up.
//! Ctrl+Alt+Backspace quits back to the text console.

#![no_std]
#![no_main]

extern crate alloc;

mod apps;
mod keymap;
mod pointer;
mod render;
mod server;
mod shell;
mod surface;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eli::{eprintln, println};
use ui::{sys, wire};

use crate::render::{Font, Screen};
use crate::server::Client;
use crate::shell::{Desktop, FONT_SIZE};

eli::main!(main);

const TOKEN_LISTENER: u64 = 1;
const TOKEN_TIMER: u64 = 2;
const TOKEN_INPUT: u64 = 1 << 32;
const TOKEN_CLIENT: u64 = 2 << 32;

// evdev
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_SYN: u16 = 0x00;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const BTN_MOUSE: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
/// `EVIOCGRAB`: only this reader gets the device's events.
const EVIOCGRAB: usize = 0x4004_4590;
/// `EVIOCGID`: the device's bus, vendor, product and version.
const EVIOCGID: usize = 0x8008_4502;

struct InputDevice {
    fd: i32,
    path: String,
    /// Motion gathered until the next SYN_REPORT.
    dx: i32,
    dy: i32,
    acceleration: pointer::Acceleration,
}

fn main() -> i32 {
    let arguments = eli::env::args();
    // Before taking over the screen, so the message can be read.
    let font = match Font::load(FONT_SIZE) {
        Ok(font) => font,
        Err(error) => {
            eprintln!("desktop: {error}");
            return 1;
        }
    };
    let screen = match Screen::open() {
        Ok(screen) => screen,
        Err(error) => {
            eprintln!("desktop: {error}");
            return 1;
        }
    };
    let runtime = runtime_directory();
    let socket_path = format!("{runtime}/wayland-0");
    sys::unlink(&socket_path);
    let listener = match sys::listen_unix(&socket_path) {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("desktop: can't listen on {socket_path}: {error}");
            return 1;
        }
    };
    // What the programs started from here need to find the desktop.
    eli::env::set_var("XDG_RUNTIME_DIR", &runtime);
    eli::env::set_var("WAYLAND_DISPLAY", "wayland-0");
    eli::env::set_var("XDG_SESSION_TYPE", "wayland");
    eli::env::set_var("GDK_BACKEND", "wayland");
    eli::env::set_var("MOZ_ENABLE_WAYLAND", "1");

    let Ok(epoll) = sys::Epoll::new() else {
        eprintln!("desktop: no epoll");
        return 1;
    };
    let _ = epoll.add(listener, TOKEN_LISTENER);
    let Ok(timer) = sys::interval_timer(16) else {
        eprintln!("desktop: no timer");
        return 1;
    };
    let _ = epoll.add(timer, TOKEN_TIMER);

    let mut desktop = Desktop::new(screen, font);
    desktop.start_sound_server();
    let mut devices: Vec<InputDevice> = Vec::new();
    let mut ticks = 0u64;
    scan_devices(&mut devices, &epoll);
    if arguments.len() > 1 {
        match eli::process::find_program(&arguments[1]) {
            Some(path) => desktop.run(&path, &arguments[2..]),
            None => eprintln!("desktop: {} not found", arguments[1]),
        }
    }

    let mut next_client = 1u32;
    while desktop.running {
        for (_, token) in epoll.wait(-1) {
            match token {
                TOKEN_LISTENER => {
                    while let Ok(fd) = sys::accept(listener) {
                        let id = next_client;
                        next_client += 1;
                        let _ = epoll.add(fd, TOKEN_CLIENT + u64::from(id));
                        desktop.clients.insert(id, Client::new(fd));
                    }
                }
                TOKEN_TIMER => {
                    let mut expirations = [0u8; 8];
                    let _ = eli::io::read(timer, &mut expirations);
                    desktop.tick();
                    ticks += 1;
                    // New keyboards and mice, every two seconds.
                    if ticks.is_multiple_of(120) {
                        scan_devices(&mut devices, &epoll);
                    }
                }
                token if token >= TOKEN_CLIENT => {
                    let id = (token - TOKEN_CLIENT) as u32;
                    if let Err(error) = serve(&mut desktop, id) {
                        if let wire::Broken::Protocol(reason) = error {
                            eprintln!("desktop: client {id}: {reason}");
                        }
                        disconnect(&mut desktop, &epoll, id);
                    }
                }
                token => {
                    let index = (token - TOKEN_INPUT) as usize;
                    if !read_input(&mut desktop, &mut devices, index) {
                        let device = devices.remove(index);
                        epoll.remove(device.fd);
                        let _ = eli::io::close(device.fd);
                        // Tokens are indexes: re-register the rest.
                        for (index, device) in devices.iter().enumerate() {
                            epoll.remove(device.fd);
                            let _ = epoll.add(device.fd, TOKEN_INPUT + index as u64);
                        }
                    }
                }
            }
        }
        // Send what was queued; a client that can't take it is dropped.
        let broken: Vec<u32> = desktop
            .clients
            .iter_mut()
            .filter_map(|(&id, client)| client.connection.flush().err().map(|_| id))
            .collect();
        for id in broken {
            disconnect(&mut desktop, &epoll, id);
        }
    }
    desktop.end_programs();
    for device in &devices {
        // SAFETY: EVIOCGRAB takes its argument by value.
        let _ = unsafe { sys::ioctl(device.fd, EVIOCGRAB, 0) };
    }
    sys::unlink(&socket_path);
    println!("desktop: goodbye");
    0
}

/// `$XDG_RUNTIME_DIR`, or a private directory in /tmp.
fn runtime_directory() -> String {
    if let Some(directory) = eli::env::var("XDG_RUNTIME_DIR")
        && eli::fs::exists(&directory)
    {
        return directory;
    }
    let directory = format!("/tmp/runtime-{}", eli::users::uid());
    let _ = sys::make_directory(&directory, 0o700);
    directory
}

/// Reads and handles a client's requests.
fn serve(desktop: &mut Desktop, id: u32) -> Result<(), wire::Broken> {
    let mut actions = Vec::new();
    {
        let client = desktop.clients.get_mut(&id).ok_or(wire::Broken::Closed)?;
        client.connection.read()?;
        while let Some(message) = client.connection.next_message()? {
            let action = client.dispatch(message, &mut desktop.serial)?;
            actions.push(action);
        }
    }
    for action in actions {
        desktop.act(id, action);
    }
    Ok(())
}

fn disconnect(desktop: &mut Desktop, epoll: &sys::Epoll, id: u32) {
    if let Some(mut client) = desktop.clients.remove(&id) {
        epoll.remove(client.connection.fd);
        client.close_pools();
    }
    desktop.client_gone(id);
}

/// Opens input devices not open yet, grabbing them all.
fn scan_devices(devices: &mut Vec<InputDevice>, epoll: &sys::Epoll) {
    let Ok(entries) = eli::fs::read_dir("/dev/input") else { return };
    let mut paths: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry.name.starts_with("event"))
        .map(|entry| format!("/dev/input/{}", entry.name))
        .collect();
    paths.sort();
    for path in paths {
        if devices.iter().any(|device| device.path == path) {
            continue;
        }
        let Ok(fd) = sys::open(&path, sys::O_RDONLY | sys::O_NONBLOCK) else { continue };
        // SAFETY: EVIOCGRAB takes its argument by value.
        let _ = unsafe { sys::ioctl(fd, EVIOCGRAB, 1) };
        let index = devices.len();
        let _ = epoll.add(fd, TOKEN_INPUT + index as u64);
        let mut id = [0u16; 4];
        // SAFETY: EVIOCGID writes a `struct input_id` (four u16s).
        let _ = unsafe { sys::ioctl(fd, EVIOCGID, id.as_mut_ptr() as usize) };
        let acceleration = pointer::Acceleration::new(id[0]);
        devices.push(InputDevice { fd, path, dx: 0, dy: 0, acceleration });
    }
}

/// Handles what an input device reported; false if it's gone.
fn read_input(desktop: &mut Desktop, devices: &mut [InputDevice], index: usize) -> bool {
    let Some(device) = devices.get_mut(index) else { return true };
    let mut buffer = [0u8; 24 * 64];
    loop {
        let count = match eli::io::read(device.fd, &mut buffer) {
            Ok(0) => return false,
            Ok(count) => count,
            Err(eli::Error::AGAIN) => return true,
            Err(_) => return false,
        };
        for event in buffer[..count].as_chunks::<24>().0 {
            let kind = u16::from_le_bytes([event[16], event[17]]);
            let code = u16::from_le_bytes([event[18], event[19]]);
            let value = i32::from_le_bytes(event[20..24].try_into().unwrap());
            match (kind, code) {
                (EV_REL, REL_X) => device.dx += value,
                (EV_REL, REL_Y) => device.dy += value,
                (EV_REL, REL_WHEEL) => desktop.pointer_axis(true, -value),
                (EV_REL, REL_HWHEEL) => desktop.pointer_axis(false, value),
                (EV_KEY, BTN_MOUSE..=BTN_TASK) => desktop.pointer_button(u32::from(code), value != 0),
                // Key presses (1) and releases (0); repeats (2) are the
                // clients' business.
                (EV_KEY, _) if value != 2 => desktop.key(u32::from(code), value == 1),
                (EV_SYN, _) if device.dx != 0 || device.dy != 0 => {
                    let (dx, dy) = (device.dx, device.dy);
                    device.dx = 0;
                    device.dy = 0;
                    let seconds = u64::from_le_bytes(event[0..8].try_into().unwrap());
                    let micros = u64::from_le_bytes(event[8..16].try_into().unwrap());
                    let nanos = seconds * 1_000_000_000 + micros * 1000;
                    let (dx, dy) = device.acceleration.apply(f64::from(dx), f64::from(dy), nanos);
                    desktop.pointer_motion(dx, dy);
                }
                _ => {}
            }
        }
    }
}
