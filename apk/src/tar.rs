//! Reading tar archives: 512-byte headers, each followed by the file's data
//! padded to 512 bytes. Names longer than 100 bytes come from the ustar
//! prefix field, a PAX extended header (`path=`, `linkpath=`), or a GNU
//! long-name entry, all of which Alpine's archives use.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    /// A hard link: another name for a file earlier in the archive.
    HardLink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub path: String,
    pub kind: Kind,
    /// Permission bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub modified: u64,
    /// For links: what they point to.
    pub link: String,
    pub data: &'a [u8],
}

fn octal(field: &[u8]) -> u64 {
    // Big values use a binary form (top bit set), which these archives
    // don't need; treat it as zero rather than misread it.
    if field.first().is_some_and(|&b| b & 0x80 != 0) {
        return 0;
    }
    field
        .iter()
        .skip_while(|&&b| b == b' ')
        .take_while(|&&b| (b'0'..=b'7').contains(&b))
        .fold(0, |value, &digit| value * 8 + u64::from(digit - b'0'))
}

fn text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// PAX records: `<length> <key>=<value>\n`.
fn pax_records(data: &[u8]) -> Vec<(String, String)> {
    let mut records = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some(space) = rest.iter().position(|&b| b == b' ') else { break };
        let Ok(length) = core::str::from_utf8(&rest[..space]).unwrap_or("").parse::<usize>() else { break };
        if length <= space + 1 || length > rest.len() {
            break;
        }
        let record = &rest[space + 1..length - 1];
        if let Some(equals) = record.iter().position(|&b| b == b'=') {
            records.push((text(&record[..equals]), String::from_utf8_lossy(&record[equals + 1..]).into_owned()));
        }
        rest = &rest[length..];
    }
    records
}

/// The entries of a tar archive (in a slice of memory). Stops at the
/// end-of-archive block or the end of the data; a damaged header stops it
/// with an error.
pub fn entries(archive: &[u8]) -> Result<Vec<Entry<'_>>, &'static str> {
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut long_path: Option<String> = None;
    let mut long_link: Option<String> = None;
    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let stored = octal(&header[148..156]);
        let sum: u64 =
            header.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
        if stored != sum {
            return Err("a tar header's checksum is wrong");
        }
        let size = octal(&header[124..136]) as usize;
        let data_start = offset + 512;
        let data = archive.get(data_start..data_start + size).ok_or("the archive is cut short")?;
        offset = data_start + size.next_multiple_of(512);

        let kind_byte = header[156];
        match kind_byte {
            // PAX extended header for the next entry.
            b'x' => {
                for (key, value) in pax_records(data) {
                    match key.as_str() {
                        "path" => long_path = Some(value),
                        "linkpath" => long_link = Some(value),
                        _ => {}
                    }
                }
                continue;
            }
            // PAX global header: nothing these archives need.
            b'g' => continue,
            // GNU long name and long link name.
            b'L' => {
                long_path = Some(text(data));
                continue;
            }
            b'K' => {
                long_link = Some(text(data));
                continue;
            }
            _ => {}
        }
        let mut path = text(&header[0..100]);
        let ustar = &header[257..262] == b"ustar";
        if ustar && header[345] != 0 {
            path = alloc::format!("{}/{path}", text(&header[345..500]));
        }
        let path = long_path.take().unwrap_or(path);
        let link = long_link.take().unwrap_or_else(|| text(&header[157..257]));
        let kind = match kind_byte {
            b'0' | 0 | b'7' => {
                if path.ends_with('/') {
                    Kind::Directory
                } else {
                    Kind::File
                }
            }
            b'5' => Kind::Directory,
            b'2' => Kind::Symlink,
            b'1' => Kind::HardLink,
            _ => Kind::Other,
        };
        entries.push(Entry {
            path: path.trim_start_matches("./").trim_end_matches('/').to_string(),
            kind,
            mode: octal(&header[100..108]) as u32 & 0o7777,
            uid: octal(&header[108..116]) as u32,
            gid: octal(&header[116..124]) as u32,
            modified: octal(&header[136..148]),
            link,
            data: if kind == Kind::File { data } else { &[] },
        });
    }
    Ok(entries)
}
