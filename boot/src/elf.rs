//! A minimal ELF64 parser: just enough to load a static x86_64 executable.

pub const PT_LOAD: u32 = 1;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 0x3E;
const PROGRAM_HEADER_SIZE: usize = 56;

pub struct Elf<'a> {
    data: &'a [u8],
    pub entry: u64,
    program_headers: &'a [u8],
    program_header_size: usize,
}

pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub file_size: u64,
    pub mem_size: u64,
}

impl<'a> Elf<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        const TRUNCATED: &str = "kernel ELF is truncated";

        if data.get(0..4) != Some(b"\x7FELF") {
            return Err("kernel is not an ELF file");
        }
        if data.get(4) != Some(&2) || data.get(5) != Some(&1) {
            return Err("kernel is not a 64-bit little-endian ELF");
        }
        if read_u16(data, 16).ok_or(TRUNCATED)? != ET_EXEC {
            return Err("kernel ELF is not a static executable");
        }
        if read_u16(data, 18).ok_or(TRUNCATED)? != EM_X86_64 {
            return Err("kernel ELF is not for x86_64");
        }

        let entry = read_u64(data, 24).ok_or(TRUNCATED)?;
        let table_offset = read_u64(data, 32).ok_or(TRUNCATED)? as usize;
        let program_header_size = usize::from(read_u16(data, 54).ok_or(TRUNCATED)?);
        let program_header_count = usize::from(read_u16(data, 56).ok_or(TRUNCATED)?);
        if program_header_size < PROGRAM_HEADER_SIZE {
            return Err("kernel ELF has malformed program headers");
        }
        let program_headers = table_offset
            .checked_add(program_header_size * program_header_count)
            .and_then(|end| data.get(table_offset..end))
            .ok_or(TRUNCATED)?;

        Ok(Self { data, entry, program_headers, program_header_size })
    }

    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> {
        self.program_headers.chunks_exact(self.program_header_size).map(|header| ProgramHeader {
            // The table's bounds were checked in `parse`, so these can't fail.
            kind: read_u32(header, 0).unwrap(),
            flags: read_u32(header, 4).unwrap(),
            offset: read_u64(header, 8).unwrap(),
            vaddr: read_u64(header, 16).unwrap(),
            file_size: read_u64(header, 32).unwrap(),
            mem_size: read_u64(header, 40).unwrap(),
        })
    }

    /// The bytes of a segment that are stored in the file.
    pub fn segment_data(&self, header: &ProgramHeader) -> Option<&'a [u8]> {
        let start = usize::try_from(header.offset).ok()?;
        let end = start.checked_add(usize::try_from(header.file_size).ok()?)?;
        self.data.get(start..end)
    }
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(offset..offset + 2)?.try_into().ok()?))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?))
}
