//! Unpacking the initrd, a cpio archive in the "newc" format (the same one
//! Linux uses for initramfs).
//!
//! Each entry is a 110-byte header of ASCII hex fields, the NUL-terminated
//! name, then the data, with the name and data each padded to 4 bytes. The
//! archive ends with an entry named `TRAILER!!!`.

use super::{Attributes, FileKind, lookup, lookup_parent};

const HEADER_SIZE: usize = 110;
const MAGIC: &[u8] = b"070701";

/// Field offsets within the header (each field is 8 hex digits).
const MODE: usize = 14;
const UID: usize = 22;
const GID: usize = 30;
const MODIFIED: usize = 46;
const FILE_SIZE: usize = 54;
const NAME_SIZE: usize = 94;

const TYPE_MASK: u32 = 0o170000;

/// Creates every entry of `archive` in the root file system. Returns how
/// many were unpacked.
pub fn unpack(archive: &[u8]) -> Result<usize, &'static str> {
    let mut offset = 0;
    let mut count = 0;
    loop {
        let header = archive.get(offset..offset + HEADER_SIZE).ok_or("truncated header")?;
        if &header[..6] != MAGIC {
            return Err("bad entry magic");
        }
        let field = |at: usize| -> Result<usize, &'static str> {
            let text = core::str::from_utf8(&header[at..at + 8]).map_err(|_| "bad header field")?;
            usize::from_str_radix(text, 16).map_err(|_| "bad header field")
        };
        let mode = field(MODE)? as u32;
        let attributes = Attributes { permissions: mode & 0o7777, uid: field(UID)? as u32, gid: field(GID)? as u32 };
        let modified = field(MODIFIED)? as u64;
        let file_size = field(FILE_SIZE)?;
        let name_size = field(NAME_SIZE)?;

        let name_start = offset + HEADER_SIZE;
        let name_bytes = archive.get(name_start..name_start + name_size).ok_or("truncated name")?;
        let name = core::str::from_utf8(name_bytes.strip_suffix(&[0]).unwrap_or(name_bytes))
            .map_err(|_| "file name is not UTF-8")?;
        let data_start = (name_start + name_size).next_multiple_of(4);
        let data = archive.get(data_start..data_start + file_size).ok_or("truncated file data")?;
        offset = (data_start + file_size).next_multiple_of(4);

        if name == "TRAILER!!!" {
            return Ok(count);
        }
        let path = alloc::format!("/{}", name.trim_start_matches("./"));
        if path == "/" || path == "/." {
            continue;
        }
        let kind = match mode & TYPE_MASK {
            0o040000 => FileKind::Directory,
            0o120000 => FileKind::Symlink,
            0o100000 => FileKind::Regular,
            // Device nodes and the like: /dev is populated by the kernel.
            _ => continue,
        };

        // Directories may already exist (created implicitly, or as /dev).
        if kind == FileKind::Directory
            && let Ok(existing) = lookup("/", &path, false)
        {
            let _ = existing.set_permissions(attributes.permissions);
            let _ = existing.set_owner(attributes.uid, attributes.gid);
            continue;
        }
        let (parent, name) = lookup_parent("/", &path).map_err(|_| "entry's directory is missing")?;
        let inode = parent.create(&name, kind, attributes).map_err(|_| "can't create entry")?;
        if kind != FileKind::Directory {
            inode.write_at(0, data).map_err(|_| "can't write entry")?;
        }
        let _ = inode.set_modified(modified);
        count += 1;
    }
}
