//! DEFLATE decompression (RFC 1951) and the gzip wrapper (RFC 1952).
//!
//! DEFLATE data is a series of blocks: stored (uncompressed), or compressed
//! with Huffman codes, either the fixed ones the format defines or ones
//! described at the start of the block. Compressed data is a mix of
//! literal bytes and back-references ("copy 12 bytes from 300 bytes ago").

use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The data ended in the middle.
    Truncated,
    /// Not gzip, or not valid DEFLATE.
    Invalid,
    /// The gzip checksum or length didn't match the data.
    Checksum,
    /// It would decompress to more than the limit given.
    TooBig,
}

/// Reads bits, least significant first, as DEFLATE packs them.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
    buffer: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0, buffer: 0, count: 0 }
    }

    fn refill(&mut self) {
        while self.count <= 56 && self.position < self.data.len() {
            self.buffer |= u64::from(self.data[self.position]) << self.count;
            self.position += 1;
            self.count += 8;
        }
    }

    fn take(&mut self, bits: u32) -> Result<u32, Error> {
        if bits == 0 {
            return Ok(0);
        }
        if self.count < bits {
            self.refill();
            if self.count < bits {
                return Err(Error::Truncated);
            }
        }
        let value = (self.buffer & ((1 << bits) - 1)) as u32;
        self.buffer >>= bits;
        self.count -= bits;
        Ok(value)
    }

    /// Skips to the next byte boundary (stored blocks start on one).
    fn align(&mut self) {
        let drop = self.count % 8;
        self.buffer >>= drop;
        self.count -= drop;
    }

    /// Where the next unread byte is (whole bytes still buffered count as
    /// unread).
    fn byte_position(&self) -> usize {
        self.position - (self.count / 8) as usize
    }
}

/// A canonical Huffman code, decoded a bit at a time with per-length
/// counts (simple, and fast enough for package-sized data).
struct Huffman {
    /// How many codes there are of each length (0-15).
    counts: [u16; 16],
    /// The symbols, ordered by code.
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, Error> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            counts[usize::from(length)] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for length in 1..16 {
            offsets[length] = offsets[length - 1] + counts[length - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[usize::from(offsets[usize::from(length)])] = symbol as u16;
                offsets[usize::from(length)] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, Error> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.counts[length]);
            if code - count < first {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::Invalid)
    }
}

const LENGTH_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// The order code length code lengths are stored in.
const CODE_LENGTH_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Decompresses raw DEFLATE data. Returns the output and how many input
/// bytes it used (data may follow it, as in a gzip file). Refuses to make
/// more than `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut bits = Bits::new(data);
    let mut output = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let header = [bits.take(8)?, bits.take(8)?, bits.take(8)?, bits.take(8)?];
                let len = (header[0] | header[1] << 8) as usize;
                let complement = (header[2] | header[3] << 8) as usize;
                if len != !complement & 0xFFFF {
                    return Err(Error::Invalid);
                }
                // Whole bytes from here: the bit buffer is byte-aligned.
                for _ in 0..len {
                    output.push(bits.take(8)? as u8);
                }
            }
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);
                let literals = Huffman::new(&lengths)?;
                let distances = Huffman::new(&[5; 30])?;
                block(&mut bits, &literals, &distances, &mut output, limit)?;
            }
            2 => {
                let literal_count = bits.take(5)? as usize + 257;
                let distance_count = bits.take(5)? as usize + 1;
                let code_count = bits.take(4)? as usize + 4;
                let mut code_lengths = [0u8; 19];
                for &index in &CODE_LENGTH_ORDER[..code_count] {
                    code_lengths[index] = bits.take(3)? as u8;
                }
                let codes = Huffman::new(&code_lengths)?;
                let mut lengths = vec![0u8; literal_count + distance_count];
                let mut index = 0;
                while index < lengths.len() {
                    let symbol = codes.decode(&mut bits)?;
                    let (value, repeat) = match symbol {
                        0..=15 => (symbol as u8, 1),
                        16 => {
                            let previous = *lengths.get(index.wrapping_sub(1)).ok_or(Error::Invalid)?;
                            (previous, 3 + bits.take(2)? as usize)
                        }
                        17 => (0, 3 + bits.take(3)? as usize),
                        18 => (0, 11 + bits.take(7)? as usize),
                        _ => return Err(Error::Invalid),
                    };
                    if index + repeat > lengths.len() {
                        return Err(Error::Invalid);
                    }
                    lengths[index..index + repeat].fill(value);
                    index += repeat;
                }
                let literals = Huffman::new(&lengths[..literal_count])?;
                let distances = Huffman::new(&lengths[literal_count..])?;
                block(&mut bits, &literals, &distances, &mut output, limit)?;
            }
            _ => return Err(Error::Invalid),
        }
        if output.len() > limit {
            return Err(Error::TooBig);
        }
        if last {
            return Ok((output, bits.byte_position()));
        }
    }
}

/// One Huffman-coded block: literals and back-references until symbol 256.
fn block(
    bits: &mut Bits,
    literals: &Huffman,
    distances: &Huffman,
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), Error> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => output.push(symbol as u8),
            256 => return Ok(()),
            257..=285 => {
                let index = usize::from(symbol - 257);
                let length = usize::from(LENGTH_BASE[index]) + bits.take(u32::from(LENGTH_EXTRA[index]))? as usize;
                let code = usize::from(distances.decode(bits)?);
                let distance = usize::from(*DISTANCE_BASE.get(code).ok_or(Error::Invalid)?)
                    + bits.take(u32::from(DISTANCE_EXTRA[code]))? as usize;
                if distance > output.len() {
                    return Err(Error::Invalid);
                }
                if output.len() + length > limit {
                    return Err(Error::TooBig);
                }
                // Byte by byte: the copy may overlap what it's producing.
                let start = output.len() - distance;
                for offset in 0..length {
                    output.push(output[start + offset]);
                }
            }
            _ => return Err(Error::Invalid),
        }
    }
}

/// CRC-32 (as gzip and zip use it).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut value = index as u32;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 != 0 { (value >> 1) ^ 0xEDB8_8320 } else { value >> 1 };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    };
    !data.iter().fold(0xFFFF_FFFF, |crc, &byte| TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8))
}

/// Decompresses one gzip member. Returns the data and how many bytes of
/// `data` the member took (gzip files can be several members one after
/// another, as Alpine's packages are).
pub fn gunzip(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), Error> {
    if data.len() < 18 {
        return Err(Error::Truncated);
    }
    if data[0] != 0x1F || data[1] != 0x8B || data[2] != 8 {
        return Err(Error::Invalid);
    }
    let flags = data[3];
    let mut offset = 10;
    if flags & 0x04 != 0 {
        let extra = usize::from(u16::from_le_bytes([data[10], data[11]]));
        offset += 2 + extra;
    }
    for flag in [0x08, 0x10] {
        // A file name or comment, NUL-terminated.
        if flags & flag != 0 {
            let end =
                data.get(offset..).ok_or(Error::Truncated)?.iter().position(|&b| b == 0).ok_or(Error::Truncated)?;
            offset += end + 1;
        }
    }
    if flags & 0x02 != 0 {
        offset += 2;
    }
    let (output, used) = inflate(data.get(offset..).ok_or(Error::Truncated)?, limit)?;
    let trailer = offset + used;
    let footer = data.get(trailer..trailer + 8).ok_or(Error::Truncated)?;
    let crc = u32::from_le_bytes(footer[0..4].try_into().unwrap());
    let size = u32::from_le_bytes(footer[4..8].try_into().unwrap());
    if crc != crc32(&output) || size != output.len() as u32 {
        return Err(Error::Checksum);
    }
    Ok((output, trailer + 8))
}
