//! The in-kernel test harness, compiled into test builds only.
//!
//! Tests are declared with [`ktest!`](crate::ktest). Each one places a
//! [`TestCase`] in the `.kernel_tests` linker section, and the linker script
//! brackets that section with symbols, so the harness finds every test in the
//! kernel without a central list. `cargo xtask test` boots the test kernel in
//! QEMU, which runs every test and exits with the result.

use core::fmt;
use core::sync::atomic::{AtomicPtr, Ordering};

use bootinfo::BootInfo;

use crate::arch;

pub struct TestCase {
    pub name: &'static str,
    pub run: fn(),
}

/// Declares kernel tests:
///
/// ```ignore
/// crate::ktest! {
///     fn addition_works() {
///         assert_eq!(1 + 1, 2);
///     }
/// }
/// ```
#[macro_export]
macro_rules! ktest {
    ($(fn $name:ident() $body:block)*) => {
        $(
            fn $name() $body

            const _: () = {
                #[used]
                #[unsafe(link_section = ".kernel_tests")]
                static TEST: $crate::test::TestCase = $crate::test::TestCase {
                    name: concat!(module_path!(), "::", stringify!($name)),
                    run: $name,
                };
            };
        )*
    };
}

// Markers defined by the linker script; only their addresses matter.
unsafe extern "C" {
    static __kernel_tests_start: u8;
    static __kernel_tests_end: u8;
}

fn all_tests() -> &'static [TestCase] {
    // SAFETY: the linker script places these symbols around an array of
    // TestCase statics.
    unsafe {
        let start = (&raw const __kernel_tests_start).cast::<TestCase>();
        let end = (&raw const __kernel_tests_end).cast::<TestCase>();
        core::slice::from_raw_parts(start, end.offset_from(start) as usize)
    }
}

static BOOT_INFO: AtomicPtr<BootInfo> = AtomicPtr::new(core::ptr::null_mut());

/// The boot info the kernel was started with, for tests that check the
/// handoff.
pub fn boot_info() -> &'static BootInfo {
    // SAFETY: set from a `&'static BootInfo` before any test runs.
    unsafe { BOOT_INFO.load(Ordering::Acquire).as_ref().expect("boot info not set") }
}

pub fn run(boot_info: &'static BootInfo) -> ! {
    BOOT_INFO.store(core::ptr::from_ref(boot_info).cast_mut(), Ordering::Release);

    let tests = all_tests();
    println!("running {} tests", tests.len());
    for test in tests {
        print!("test {} ... ", test.name.strip_prefix("elikernel::").unwrap_or(test.name));
        (test.run)();
        println!("\x1B[32mok\x1B[0m");
    }
    println!("\x1B[32mall {} tests passed\x1B[0m", tests.len());
    exit_qemu(ExitCode::Success)
}

/// Called by the panic handler after it has printed the panic message.
pub fn on_panic() -> ! {
    println!("\x1B[91mFAILED\x1B[0m");
    exit_qemu(ExitCode::Failure)
}

/// A fixed-capacity string, for checking formatted output in tests.
pub struct StrBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> StrBuf<N> {
    pub fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        // Only whole `&str`s are ever appended.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}

impl<const N: usize> fmt::Write for StrBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Values for QEMU's `isa-debug-exit` device, which exits QEMU with status
/// `(value << 1) | 1`. Both are chosen to differ from QEMU's own exit codes.
#[repr(u8)]
enum ExitCode {
    Success = 0x10,
    Failure = 0x11,
}

const DEBUG_EXIT_PORT: u16 = 0xF4;

fn exit_qemu(code: ExitCode) -> ! {
    // SAFETY: under `cargo xtask test` this port is QEMU's exit device.
    // Elsewhere nothing listens on it and we halt below.
    unsafe { arch::outb(DEBUG_EXIT_PORT, code as u8) };
    arch::halt_forever()
}
