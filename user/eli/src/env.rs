//! Program arguments and environment variables.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

/// Process-wide state. eliOS processes have a single thread, and RefCell
/// turns any accidental overlapping use into a clean panic.
pub(crate) struct Global<T>(RefCell<T>);

// SAFETY: there's only one thread.
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(RefCell::new(value))
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

static ARGUMENTS: Global<Vec<String>> = Global::new(Vec::new());
/// `NAME=value` strings, as passed to `exec`.
static VARIABLES: Global<Vec<String>> = Global::new(Vec::new());

/// Reads argc, argv and envp from the initial stack.
///
/// # Safety
/// `stack` must be the stack pointer the kernel started the program with.
pub(crate) unsafe fn init(stack: *const usize) {
    unsafe {
        let count = *stack;
        let arguments = stack.add(1) as *const *const u8;
        let strings = |mut pointer: *const *const u8| {
            let mut list = Vec::new();
            while !(*pointer).is_null() {
                list.push(c_string(*pointer));
                pointer = pointer.add(1);
            }
            list
        };
        let argument_list: Vec<String> = (0..count).map(|i| c_string(*arguments.add(i))).collect();
        let environment = strings(arguments.add(count + 1));
        ARGUMENTS.with(|a| *a = argument_list);
        VARIABLES.with(|v| *v = environment);
    }
}

unsafe fn c_string(pointer: *const u8) -> String {
    let mut len = 0;
    while unsafe { *pointer.add(len) } != 0 {
        len += 1;
    }
    String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(pointer, len) }).into_owned()
}

/// The program's arguments, starting with its name.
pub fn args() -> Vec<String> {
    ARGUMENTS.with(|a| a.clone())
}

/// The name the program was run as, without any directories.
pub fn program_name() -> String {
    ARGUMENTS.with(|a| a.first().map(|name| name.rsplit('/').next().unwrap_or(name).to_string()).unwrap_or_default())
}

pub fn var(name: &str) -> Option<String> {
    VARIABLES.with(|vars| {
        vars.iter().find_map(|entry| entry.split_once('=').filter(|(key, _)| *key == name).map(|(_, v)| v.to_string()))
    })
}

pub fn set_var(name: &str, value: &str) {
    remove_var(name);
    VARIABLES.with(|vars| vars.push(alloc::format!("{name}={value}")));
}

pub fn remove_var(name: &str) {
    VARIABLES.with(|vars| vars.retain(|entry| entry.split_once('=').is_none_or(|(key, _)| key != name)));
}

/// All variables as `NAME=value` strings (the form `exec` takes).
pub fn vars() -> Vec<String> {
    VARIABLES.with(|vars| vars.clone())
}
