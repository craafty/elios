//! UEFI runtime services: the part of the firmware that stays available
//! after boot. eliOS uses it for firmware variables (boot entries, which the
//! installer writes).
//!
//! The firmware's code expects to run where it was loaded, at its physical
//! addresses (eliboot never calls `SetVirtualAddressMap`, which would move
//! it for good, and which some firmware gets wrong). So calls are made on a
//! separate set of page tables: the kernel's half, plus the firmware's
//! runtime memory and device registers mapped at their physical addresses.
//! For the duration of a call, this CPU switches to those tables and to a
//! private stack (firmware may use more stack than a kernel thread has),
//! with interrupts off, and with the user program's FPU registers saved
//! around it (firmware is free to use SSE).
//!
//! Firmware calls use the Microsoft x64 convention: arguments in rcx, rdx,
//! r8, r9, then the stack, above 32 bytes of "shadow space".

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::naked_asm;

use bootinfo::{BootInfo, MemoryKind};

use crate::arch::{self, fpu};
use crate::memory;
use crate::memory::paging::{CACHE_DISABLE, PageTable, WRITABLE, WRITE_THROUGH};
use crate::sync::{Once, SpinLock};

/// Offsets of the runtime services in their table (after the 24-byte
/// header).
const GET_VARIABLE: u64 = 72;
const GET_NEXT_VARIABLE_NAME: u64 = 80;
const SET_VARIABLE: u64 = 88;

/// The stack firmware calls run on: 64 KiB.
const STACK_PAGES: u64 = 16;

const STATUS_ERROR: u64 = 1 << 63;
const BUFFER_TOO_SMALL: u64 = STATUS_ERROR | 5;
const NOT_FOUND: u64 = STATUS_ERROR | 14;

struct Runtime {
    /// Physical address of the runtime services table.
    table: u64,
    page_table: u64,
    stack_top: u64,
}

static RUNTIME: Once<Runtime> = Once::new();
/// Firmware may only be entered by one CPU at a time. (A spinlock: calls
/// run with interrupts off.)
static CALL: SpinLock<()> = SpinLock::new(());

/// Sets up the page tables firmware calls need. Without runtime services
/// (not booted by UEFI, or no table), variables are unavailable.
pub fn init(boot_info: &BootInfo) {
    if boot_info.runtime_services == 0 {
        return;
    }
    let Ok(mut table) = PageTable::new() else { return };
    table.share_kernel_half(memory::kernel_page_table_root());
    for region in boot_info.memory_regions() {
        let flags = match region.kind {
            MemoryKind::FirmwareRuntime => WRITABLE,
            MemoryKind::FirmwareRuntimeMmio => WRITABLE | CACHE_DISABLE | WRITE_THROUGH,
            _ => continue,
        };
        if table.map_range(region.start, region.start, region.len, flags).is_err() {
            warn!("efi: can't map runtime memory at {:#x}", region.start);
            return;
        }
    }
    // The tables and the stack live as long as the kernel.
    let stack_top = memory::allocate_kernel_stack(STACK_PAGES).top;
    let _ = RUNTIME.set(Runtime { table: boot_info.runtime_services, page_table: table.root(), stack_top });
    debug!("efi: runtime services at {:#x}", boot_info.runtime_services);
}

pub fn available() -> bool {
    RUNTIME.get().is_some()
}

/// Calls `function` with the Microsoft x64 convention and five arguments,
/// on the stack whose top is `stack`.
///
/// # Safety
/// `function` must be a firmware function taking up to five arguments, and
/// the active page tables must map it, its data, and everything the
/// arguments point to.
#[unsafe(naked)]
unsafe extern "sysv64" fn call_on_stack(function: u64, arguments: *const u64, stack: u64) -> u64 {
    naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        // The firmware's stack, keeping 16-byte alignment: 32 bytes of
        // shadow space and the fifth argument, plus padding.
        "mov rsp, rdx",
        "sub rsp, 48",
        "mov rax, rdi",
        "mov r10, rsi",
        "mov rcx, [r10]",
        "mov rdx, [r10 + 8]",
        "mov r8, [r10 + 16]",
        "mov r9, [r10 + 24]",
        "mov r11, [r10 + 32]",
        "mov [rsp + 32], r11",
        "call rax",
        "mov rsp, rbp",
        "pop rbp",
        "ret",
    )
}

/// Calls runtime service `offset` with `arguments`; returns its status.
/// Every pointer in the arguments must point into kernel memory: only the
/// kernel's half is mapped while the firmware runs.
fn call(offset: u64, arguments: [u64; 5]) -> Option<u64> {
    let runtime = RUNTIME.get()?;
    // SAFETY: the table is runtime services data, which the direct map
    // covers; the entry holds the function's physical address.
    let function = unsafe { memory::phys_to_virt(runtime.table + offset).cast::<u64>().read() };
    let _only_one = CALL.lock();
    let mut registers = fpu::FpuState::initial();
    fpu::save(&mut registers);
    let previous = arch::page_table_root();
    // SAFETY: the firmware's tables share the kernel half (this code, the
    // stack and the arguments, which all point into kernel memory) and add
    // the firmware's own memory; interrupts are off while they're active.
    let status = unsafe {
        arch::set_page_table_root(runtime.page_table);
        let status = call_on_stack(function, arguments.as_ptr(), runtime.stack_top);
        arch::set_page_table_root(previous);
        status
    };
    fpu::restore(&registers);
    Some(status)
}

/// A variable's name as UEFI wants it: UCS-2, NUL-terminated.
fn ucs2(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Reads a variable: its attributes and value. `None` if it doesn't exist
/// (or there are no runtime services).
pub fn get_variable(name: &str, vendor: &[u8; 16]) -> Option<(u32, Vec<u8>)> {
    let name = ucs2(name);
    let vendor = *vendor;
    let mut attributes = 0u32;
    let mut size = 0usize;
    let mut data = Vec::new();
    loop {
        let status = call(
            GET_VARIABLE,
            [
                name.as_ptr() as u64,
                vendor.as_ptr() as u64,
                &raw mut attributes as u64,
                &raw mut size as u64,
                data.as_mut_ptr() as u64,
            ],
        )?;
        match status {
            0 => {
                data.truncate(size);
                return Some((attributes, data));
            }
            BUFFER_TOO_SMALL if size > data.len() && size <= 1 << 20 => data = vec![0; size],
            _ => return None,
        }
    }
}

/// Creates, changes or (with empty `data`) deletes a variable.
pub fn set_variable(name: &str, vendor: &[u8; 16], attributes: u32, data: &[u8]) -> Result<(), u64> {
    let name = ucs2(name);
    // The firmware's page tables have only the kernel's half: `data` may be
    // a user program's buffer, so it's copied into kernel memory.
    let data = data.to_vec();
    let vendor = *vendor;
    let status = call(
        SET_VARIABLE,
        [name.as_ptr() as u64, vendor.as_ptr() as u64, u64::from(attributes), data.len() as u64, data.as_ptr() as u64],
    )
    .ok_or(NOT_FOUND)?;
    if status == 0 { Ok(()) } else { Err(status) }
}

/// Every variable: (name, vendor GUID).
pub fn variable_names() -> Vec<(String, [u8; 16])> {
    let mut names = Vec::new();
    let mut name: Vec<u16> = vec![0; 256];
    let mut vendor = [0u8; 16];
    // A guard against firmware that never says it's done.
    for _ in 0..4096 {
        let mut size = name.len() * 2;
        let status = call(
            GET_NEXT_VARIABLE_NAME,
            [&raw mut size as u64, name.as_mut_ptr() as u64, vendor.as_mut_ptr() as u64, 0, 0],
        );
        match status {
            Some(0) => {
                let len = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
                names.push((String::from_utf16_lossy(&name[..len]), vendor));
            }
            Some(BUFFER_TOO_SMALL) if size / 2 > name.len() && size <= 1 << 16 => {
                // Keep the current name (the call continues from it).
                name.resize(size / 2, 0);
            }
            _ => break,
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn firmware_variables_can_be_read_and_written() {
            if !available() {
                return;
            }
            // Every UEFI system has a BootOrder... or at least some variable.
            assert!(!variable_names().is_empty());
            const TEST_VENDOR: [u8; 16] = [0xE1; 16];
            // Non-volatile, boot and runtime access.
            set_variable("eliOSTest", &TEST_VENDOR, 7, b"hello").unwrap();
            assert_eq!(get_variable("eliOSTest", &TEST_VENDOR), Some((7, b"hello".to_vec())));
            assert!(variable_names().iter().any(|(name, vendor)| name == "eliOSTest" && *vendor == TEST_VENDOR));
            set_variable("eliOSTest", &TEST_VENDOR, 7, &[]).unwrap();
            assert_eq!(get_variable("eliOSTest", &TEST_VENDOR), None);
        }
    }
}
