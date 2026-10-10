//! Loading programs: ELF executables and the initial stack Linux programs
//! expect.

use alloc::string::String;
use alloc::vec::Vec;

use super::address_space::{AddressSpace, PROT_EXEC, PROT_READ, PROT_WRITE, STACK_SIZE, STACK_TOP, USER_END};
use crate::errno::{Errno, Result};
use crate::memory::PAGE_SIZE;
use crate::random;

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 0x3E;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PROGRAM_HEADER_SIZE: usize = 56;

/// Where position-independent executables are loaded.
const PIE_BASE: u64 = 0x5555_5560_0000;

/// What the loader learned about a program.
pub struct Program {
    pub entry: u64,
    /// Where the program headers are in memory (for the AT_PHDR aux entry).
    program_headers: u64,
    program_header_count: u64,
}

struct Segment {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    file_size: u64,
    mem_size: u64,
}

fn read<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    data.get(offset..offset + N).and_then(|bytes| bytes.try_into().ok()).ok_or(Errno::ENOEXEC)
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    read(data, offset).map(u16::from_le_bytes)
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    read(data, offset).map(u32::from_le_bytes)
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    read(data, offset).map(u64::from_le_bytes)
}

/// Loads an executable's segments into `space`. Static executables only:
/// there's no dynamic linker yet.
pub fn load(space: &mut AddressSpace, data: &[u8]) -> Result<Program> {
    if data.get(..4) != Some(b"\x7FELF") || data.get(4) != Some(&2) || data.get(5) != Some(&1) {
        return Err(Errno::ENOEXEC);
    }
    let kind = u16_at(data, 16)?;
    if u16_at(data, 18)? != EM_X86_64 || (kind != ET_EXEC && kind != ET_DYN) {
        return Err(Errno::ENOEXEC);
    }
    let base = if kind == ET_DYN { PIE_BASE } else { 0 };
    let entry = base + u64_at(data, 24)?;
    let header_offset = u64_at(data, 32)? as usize;
    let header_size = usize::from(u16_at(data, 54)?);
    let header_count = usize::from(u16_at(data, 56)?);
    if header_size < PROGRAM_HEADER_SIZE {
        return Err(Errno::ENOEXEC);
    }

    let mut segments = Vec::new();
    for index in 0..header_count {
        let at = header_offset + index * header_size;
        segments.push(Segment {
            kind: u32_at(data, at)?,
            flags: u32_at(data, at + 4)?,
            offset: u64_at(data, at + 8)?,
            vaddr: u64_at(data, at + 16)?,
            file_size: u64_at(data, at + 32)?,
            mem_size: u64_at(data, at + 40)?,
        });
    }
    if segments.iter().any(|s| s.kind == PT_INTERP) {
        // Needs a dynamic linker, which eliOS doesn't have yet.
        return Err(Errno::ENOEXEC);
    }

    let mut end_of_image = 0;
    for segment in segments.iter().filter(|s| s.kind == PT_LOAD && s.mem_size > 0) {
        let start = base.checked_add(segment.vaddr).ok_or(Errno::ENOEXEC)?;
        let end = start.checked_add(segment.mem_size).filter(|&end| end <= USER_END).ok_or(Errno::ENOEXEC)?;
        if segment.file_size > segment.mem_size {
            return Err(Errno::ENOEXEC);
        }
        let file_end = segment.offset.checked_add(segment.file_size).ok_or(Errno::ENOEXEC)?;
        let contents = data.get(segment.offset as usize..file_end as usize).ok_or(Errno::ENOEXEC)?;

        let mut protection = 0;
        if segment.flags & 4 != 0 {
            protection |= PROT_READ;
        }
        if segment.flags & 2 != 0 {
            protection |= PROT_WRITE;
        }
        if segment.flags & 1 != 0 {
            protection |= PROT_EXEC;
        }
        let first_page = start & !(PAGE_SIZE - 1);
        let last_page = end.next_multiple_of(PAGE_SIZE);
        // Segments may share a page at their edges; then the first one's
        // area covers it.
        let area_start = first_page.max(end_of_image);
        if area_start < last_page {
            space.add_area(area_start, last_page, protection)?;
        }
        space.write(start, contents)?;
        // The rest of the segment (.bss) is zero. Fresh pages already are,
        // except where a page is shared with the previous segment.
        let zero_end = end.min(start + segment.file_size + PAGE_SIZE);
        let zeros = alloc::vec![0; (zero_end - start - segment.file_size) as usize];
        space.write(start + segment.file_size, &zeros)?;
        end_of_image = end_of_image.max(last_page);
    }
    if end_of_image == 0 {
        return Err(Errno::ENOEXEC);
    }
    space.brk_start = end_of_image;
    space.brk = end_of_image;

    let program_headers = match segments.iter().find(|s| s.kind == PT_PHDR) {
        Some(phdr) => base + phdr.vaddr,
        None => segments
            .iter()
            .find(|s| s.kind == PT_LOAD && s.offset == 0)
            .map(|s| base + s.vaddr + header_offset as u64)
            .unwrap_or(0),
    };
    Ok(Program { entry, program_headers, program_header_count: header_count as u64 })
}

// Auxiliary vector keys.
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_PLATFORM: u64 = 15;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// The most argument and environment data a program can be given.
const MAX_ARGUMENT_BYTES: usize = 128 * 1024;

/// Creates the stack and fills it the way the System V ABI (and Linux)
/// specify: argc, the argv and envp pointer arrays, the auxiliary vector,
/// and the strings they point to. Returns the initial stack pointer.
pub fn build_stack(
    space: &mut AddressSpace,
    program: &Program,
    arguments: &[String],
    environment: &[String],
    path: &str,
) -> Result<u64> {
    let total: usize = arguments.iter().chain(environment).map(|s| s.len() + 1).sum();
    if total > MAX_ARGUMENT_BYTES {
        return Err(Errno::E2BIG);
    }
    space.add_area(STACK_TOP - STACK_SIZE, STACK_TOP, PROT_READ | PROT_WRITE)?;

    let mut stack = StackWriter { space, top: STACK_TOP };
    let execfn = stack.push_string(path)?;
    let argument_pointers = arguments.iter().map(|a| stack.push_string(a)).collect::<Result<Vec<u64>>>()?;
    let environment_pointers = environment.iter().map(|v| stack.push_string(v)).collect::<Result<Vec<u64>>>()?;
    let platform = stack.push_string("x86_64")?;
    let mut random_bytes = [0; 16];
    random::fill(&mut random_bytes);
    let random_address = stack.push(&random_bytes)?;

    let hardware_capabilities = u64::from(crate::arch::cpuid(1, 0)[3]);
    let auxiliary = [
        (AT_PHDR, program.program_headers),
        (AT_PHENT, PROGRAM_HEADER_SIZE as u64),
        (AT_PHNUM, program.program_header_count),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_BASE, 0),
        (AT_FLAGS, 0),
        (AT_ENTRY, program.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_PLATFORM, platform),
        (AT_HWCAP, hardware_capabilities),
        (AT_CLKTCK, 100),
        (AT_SECURE, 0),
        (AT_RANDOM, random_address),
        (AT_EXECFN, execfn),
        (AT_NULL, 0),
    ];

    let mut words = Vec::new();
    words.push(arguments.len() as u64);
    words.extend(&argument_pointers);
    words.push(0);
    words.extend(&environment_pointers);
    words.push(0);
    for (key, value) in auxiliary {
        words.extend([key, value]);
    }
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    // The stack pointer must be 16-byte aligned at entry, pointing at argc.
    let stack_pointer = (stack.top - bytes.len() as u64) & !15;
    stack.space.write(stack_pointer, &bytes)?;
    Ok(stack_pointer)
}

/// Pushes data down from the top of a new stack.
struct StackWriter<'a> {
    space: &'a mut AddressSpace,
    top: u64,
}

impl StackWriter<'_> {
    fn push(&mut self, bytes: &[u8]) -> Result<u64> {
        self.top -= bytes.len() as u64;
        self.space.write(self.top, bytes)?;
        Ok(self.top)
    }

    fn push_string(&mut self, text: &str) -> Result<u64> {
        self.push(&[0])?;
        self.push(text.as_bytes())
    }
}
