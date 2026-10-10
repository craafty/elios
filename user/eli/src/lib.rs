//! libeli: the runtime for eliOS programs written in Rust.
//!
//! It provides program startup, memory allocation, and a small standard
//! library: I/O, files, processes, time and the environment. Everything goes
//! through the Linux system call ABI, which eliOS implements, so programs
//! built with it run on Linux too.
//!
//! A program defines its entry point with [`main!`]:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! eli::main!(main);
//! fn main() -> i32 {
//!     eli::println!("hello");
//!     0
//! }
//! ```

#![no_std]

extern crate alloc;

pub mod crypt;
pub mod env;
pub mod fs;
mod heap;
pub mod io;
pub mod net;
pub mod process;
pub mod sys;
pub mod system;
pub mod term;
pub mod time;
pub mod users;

use core::arch::naked_asm;
use core::panic::PanicInfo;

pub use crate::sys::{Error, Result};

/// Declares the program's entry point: a `fn() -> i32` whose result is the
/// exit code.
#[macro_export]
macro_rules! main {
    ($main:path) => {
        #[unsafe(no_mangle)]
        fn __eli_main() -> i32 {
            $main()
        }
    };
}

unsafe extern "Rust" {
    fn __eli_main() -> i32;
}

/// Where the kernel starts the program. The stack holds argc, argv and envp
/// (see the System V ABI); the stack pointer is passed on to Rust code.
#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    naked_asm!("xor ebp, ebp", "mov rdi, rsp", "and rsp, -16", "call {start}", "ud2", start = sym start)
}

extern "C" fn start(stack: *const usize) -> ! {
    // SAFETY: the kernel lays the stack out as the ABI specifies.
    unsafe { env::init(stack) };
    // SAFETY: provided by the program through `main!`.
    let code = unsafe { __eli_main() };
    process::exit(code)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    eprintln!("{}: panicked: {}", env::program_name(), info);
    process::exit(101)
}
