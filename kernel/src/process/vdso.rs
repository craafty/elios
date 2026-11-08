//! The vDSO ("virtual dynamic shared object"): a tiny shared library the
//! kernel maps into every program so it can read the clock without a
//! system call.
//!
//! Browsers and media programs ask for the time constantly (Firefox calls
//! `clock_gettime` thousands of times a second while playing video), and a
//! system call costs far more than reading the time does: the time is just
//! the CPU's timestamp counter (TSC) scaled by a factor measured at boot
//! (see `time.rs`). So, like Linux, eliOS gives programs that formula
//! itself. glibc and musl find the library through the `AT_SYSINFO_EHDR`
//! auxiliary vector entry and look up `__vdso_clock_gettime` and friends in
//! it; with no vDSO they make the system call instead, so it's purely an
//! optimization.
//!
//! The library is two pages, mapped at [`BASE`]:
//!
//! - **Page 0**, readable and executable: an ELF header, program headers,
//!   a dynamic section, symbol and string tables and a hash table (what the
//!   dynamic linkers read), then the code at [`CODE_OFFSET`].
//! - **Page 1**, read-only: the clock's parameters ([`time::clock_parameters`]),
//!   which the code reads at a fixed distance from itself.
//!
//! The code is written in assembly, here in the kernel, because it runs in
//! user mode at whatever address it's mapped: it must be position
//! independent and use nothing of the kernel's. The ELF wrapping is built
//! at first use rather than linked separately, which keeps the build to
//! one step. The parameters never change after boot, so unlike Linux's
//! there's no sequence counter to retry on.
//!
//! The mapping is private: a debugger (or a program) writing to it gets its
//! own copy, never changing anyone else's.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::address_space::{AddressSpace, Backing, PROT_EXEC, PROT_READ, STACK_SIZE, STACK_TOP};
use crate::errno::Result;
use crate::memory::PAGE_SIZE;
use crate::memory::object::MemoryObject;
use crate::sync::Once;
use crate::time;

/// Where the vDSO is mapped: just below the stack, out of `mmap`'s way.
pub const BASE: u64 = STACK_TOP - STACK_SIZE - 0x10_0000;
/// Where in page 0 the code starts; the ELF tables come before it.
const CODE_OFFSET: usize = 0x400;
/// From the start of the code to the clock parameters in page 1.
const DATA_DISTANCE: usize = PAGE_SIZE as usize - CODE_OFFSET;

// The functions, with the System V calling convention. Each falls back to
// the real system call for what it doesn't handle (the CPU-time clocks,
// whose meaning the kernel may refine, and unknown clocks, for the
// error). The parameters page holds, as u64s: the TSC at boot, the
// nanoseconds-per-count multiplier (32.32 fixed point) and the wall-clock
// time at boot in nanoseconds.
//
// `eli_vdso_offsets` (not copied) gives each function's offset from the
// start of the code, then the code's end.
core::arch::global_asm!(
    ".pushsection .rodata.eli_vdso, \"a\"",
    ".balign 16",
    "eli_vdso_code:",
    // Nanoseconds since boot in rax; changes rcx and rdx.
    "eli_vdso_uptime:",
    "    lfence",
    "    rdtsc",
    "    shl rdx, 32",
    "    or rax, rdx",
    "    sub rax, qword ptr [rip + eli_vdso_code + {data}]",
    "    mul qword ptr [rip + eli_vdso_code + {data} + 8]",
    "    shrd rax, rdx, 32",
    "    ret",
    // int clock_gettime(clockid_t clock (edi), struct timespec *ts (rsi))
    "eli_vdso_clock_gettime:",
    "    cmp edi, 7",
    "    ja 2f",
    "    cmp edi, 2",
    "    je 2f",
    "    cmp edi, 3",
    "    je 2f",
    "    call eli_vdso_uptime",
    "    test edi, edi",
    "    je 1f",
    "    cmp edi, 5",
    "    jne 3f",
    "1:  add rax, qword ptr [rip + eli_vdso_code + {data} + 16]",
    "3:  xor edx, edx",
    "    mov ecx, 1000000000",
    "    div rcx",
    "    mov qword ptr [rsi], rax",
    "    mov qword ptr [rsi + 8], rdx",
    "    xor eax, eax",
    "    ret",
    "2:  mov eax, 228",
    "    syscall",
    "    ret",
    // int gettimeofday(struct timeval *tv (rdi), struct timezone *tz (rsi))
    "eli_vdso_gettimeofday:",
    "    test rdi, rdi",
    "    jz 1f",
    "    call eli_vdso_uptime",
    "    add rax, qword ptr [rip + eli_vdso_code + {data} + 16]",
    "    xor edx, edx",
    "    mov ecx, 1000",
    "    div rcx",
    "    xor edx, edx",
    "    mov ecx, 1000000",
    "    div rcx",
    "    mov qword ptr [rdi], rax",
    "    mov qword ptr [rdi + 8], rdx",
    "1:  test rsi, rsi",
    "    jz 2f",
    "    mov qword ptr [rsi], 0",
    "2:  xor eax, eax",
    "    ret",
    // time_t time(time_t *t (rdi))
    "eli_vdso_time:",
    "    call eli_vdso_uptime",
    "    add rax, qword ptr [rip + eli_vdso_code + {data} + 16]",
    "    xor edx, edx",
    "    mov ecx, 1000000000",
    "    div rcx",
    "    test rdi, rdi",
    "    jz 1f",
    "    mov qword ptr [rdi], rax",
    "1:  ret",
    // Rarely called; the system calls do these.
    "eli_vdso_clock_getres:",
    "    mov eax, 229",
    "    syscall",
    "    ret",
    "eli_vdso_getcpu:",
    "    mov eax, 309",
    "    syscall",
    "    ret",
    "eli_vdso_end:",
    ".balign 8",
    ".globl eli_vdso_offsets",
    "eli_vdso_offsets:",
    "    .quad eli_vdso_clock_gettime - eli_vdso_code",
    "    .quad eli_vdso_gettimeofday - eli_vdso_code",
    "    .quad eli_vdso_time - eli_vdso_code",
    "    .quad eli_vdso_clock_getres - eli_vdso_code",
    "    .quad eli_vdso_getcpu - eli_vdso_code",
    "    .quad eli_vdso_end - eli_vdso_code",
    ".popsection",
    data = const DATA_DISTANCE,
);

unsafe extern "C" {
    /// Offsets of the functions from the code's start, then the code's
    /// length (the code itself sits just before this table).
    static eli_vdso_offsets: [u64; 6];
}

/// The exported functions, in `eli_vdso_offsets` order. Each is exported
/// under Linux's `__vdso_` name, which the C libraries look up, and as a
/// weak plain alias, as Linux's vDSO does.
const FUNCTIONS: [&str; 5] = ["clock_gettime", "gettimeofday", "time", "clock_getres", "getcpu"];

static VDSO: Once<Option<Arc<MemoryObject>>> = Once::new();

/// Maps the vDSO into a new program's address space and returns its
/// address (for `AT_SYSINFO_EHDR`), or 0 if there's none to map.
pub fn map(space: &mut AddressSpace) -> Result<u64> {
    let Some(object) = VDSO.get_or_init(make) else {
        return Ok(0);
    };
    let page = |offset| Backing::Object { object: object.clone(), offset, shared: false };
    space.add_named_area(BASE, BASE + PAGE_SIZE, PROT_READ | PROT_EXEC, page(0), Some("[vdso]".into()))?;
    let data = BASE + PAGE_SIZE;
    space.add_named_area(data, data + PAGE_SIZE, PROT_READ, page(PAGE_SIZE), Some("[vvar]".into()))?;
    Ok(BASE)
}

/// Builds the two pages.
fn make() -> Option<Arc<MemoryObject>> {
    // SAFETY: the table is assembled above and never written.
    let offsets = unsafe { &*core::ptr::addr_of!(eli_vdso_offsets) };
    let code_len = offsets[5] as usize;
    // SAFETY: the code ends where the table starts (both in the section
    // above, the table 8-byte aligned after it), so these bytes are the
    // code, plus at most 7 bytes of padding.
    let code = unsafe {
        let table = core::ptr::addr_of!(eli_vdso_offsets).cast::<u8>();
        core::slice::from_raw_parts(table.sub(code_len.next_multiple_of(8)), code_len)
    };
    let mut image = elf_tables(offsets);
    assert!(image.len() <= CODE_OFFSET && CODE_OFFSET + code.len() <= PAGE_SIZE as usize, "vDSO too big");
    image.resize(CODE_OFFSET, 0);
    image.extend_from_slice(code);
    image.resize(PAGE_SIZE as usize, 0);
    for value in time::clock_parameters() {
        image.extend_from_slice(&value.to_le_bytes());
    }
    image.resize(2 * PAGE_SIZE as usize, 0);

    let object = MemoryObject::new();
    match object.write(0, &image) {
        Ok(_) => Some(object),
        Err(error) => {
            warn!("vdso: couldn't make it ({error:?}); programs will make clock system calls");
            None
        }
    }
}

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const SYMBOL_SIZE: usize = 24;
const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_SONAME: u64 = 14;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_FUNC: u8 = 2;

/// The ELF header and the dynamic linking tables for a shared object whose
/// one loadable segment is page 0 (at address 0, so every address in the
/// tables is an offset from wherever it's mapped).
fn elf_tables(offsets: &[u64; 6]) -> Vec<u8> {
    let mut strings = alloc::vec![0u8];
    let mut add_string = |text: &str| {
        let at = strings.len() as u32;
        strings.extend_from_slice(text.as_bytes());
        strings.push(0);
        at
    };
    let soname = add_string("linux-vdso.so.1");
    // Symbol 0 is the null symbol; then each function twice.
    let mut symbols: Vec<(u32, u8, u64, u64)> = Vec::new();
    for (index, name) in FUNCTIONS.iter().enumerate() {
        let start = CODE_OFFSET as u64 + offsets[index];
        let size = offsets[index + 1] - offsets[index];
        symbols.push((add_string(&alloc::format!("__vdso_{name}")), STB_GLOBAL, start, size));
        symbols.push((add_string(name), STB_WEAK, start, size));
    }
    let symbol_count = symbols.len() + 1;

    let dynamic_count = 7;
    let dynamic_at = ELF_HEADER_SIZE + 2 * PROGRAM_HEADER_SIZE;
    let symbols_at = dynamic_at + dynamic_count * 16;
    let hash_at = symbols_at + symbol_count * SYMBOL_SIZE;
    // The SysV hash table: one bucket chaining through every symbol (the
    // linkers compare names as they go; a search through ten is nothing).
    let hash_len = (2 + 1 + symbol_count) * 4;
    let strings_at = hash_at + hash_len;

    let mut out = Vec::with_capacity(CODE_OFFSET);
    let u16s = |out: &mut Vec<u8>, values: &[u16]| values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
    let u32s = |out: &mut Vec<u8>, values: &[u32]| values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
    let u64s = |out: &mut Vec<u8>, values: &[u64]| values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));

    // ELF header: 64-bit, little-endian, version 1, a shared object for
    // x86-64 with two program headers and no sections.
    out.extend_from_slice(b"\x7FELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    u16s(&mut out, &[3, 0x3E]);
    u32s(&mut out, &[1]);
    u64s(&mut out, &[0, ELF_HEADER_SIZE as u64, 0]);
    u32s(&mut out, &[0]);
    u16s(&mut out, &[ELF_HEADER_SIZE as u16, PROGRAM_HEADER_SIZE as u16, 2, 64, 0, 0]);

    // PT_LOAD: page 0, readable and executable.
    u32s(&mut out, &[1, 5]);
    u64s(&mut out, &[0, 0, 0, PAGE_SIZE, PAGE_SIZE, PAGE_SIZE]);
    // PT_DYNAMIC.
    let dynamic_len = (dynamic_count * 16) as u64;
    u32s(&mut out, &[2, 4]);
    u64s(&mut out, &[dynamic_at as u64, dynamic_at as u64, dynamic_at as u64, dynamic_len, dynamic_len, 8]);

    for (tag, value) in [
        (DT_HASH, hash_at as u64),
        (DT_STRTAB, strings_at as u64),
        (DT_SYMTAB, symbols_at as u64),
        (DT_STRSZ, strings.len() as u64),
        (DT_SYMENT, SYMBOL_SIZE as u64),
        (DT_SONAME, u64::from(soname)),
        (DT_NULL, 0),
    ] {
        u64s(&mut out, &[tag, value]);
    }

    out.resize(out.len() + SYMBOL_SIZE, 0);
    for (name, binding, value, size) in symbols {
        u32s(&mut out, &[name]);
        // Section index 1: any defined section (there's no section table);
        // 0 would mean undefined, and the absolute index wouldn't be moved
        // to where the library is mapped.
        out.extend_from_slice(&[binding << 4 | STT_FUNC, 0]);
        u16s(&mut out, &[1]);
        u64s(&mut out, &[value, size]);
    }

    let chain: Vec<u32> =
        (0..symbol_count as u32).map(|i| if i == 0 { 0 } else { (i + 1) % symbol_count as u32 }).collect();
    u32s(&mut out, &[1, symbol_count as u32, 1]);
    u32s(&mut out, &chain);
    out.extend_from_slice(&strings);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn the_vdso_tables_fit_before_the_code() {
            // SAFETY: as in `make`.
            let offsets = unsafe { &*core::ptr::addr_of!(eli_vdso_offsets) };
            let tables = elf_tables(offsets);
            assert!(tables.len() <= CODE_OFFSET, "{} bytes of tables", tables.len());
            assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
}
