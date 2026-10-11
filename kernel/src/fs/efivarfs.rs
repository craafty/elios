//! /sys/firmware/efi/efivars: the firmware's variables as files, in
//! Linux's format, so tools written for Linux (and eliOS's installer) can
//! read and write boot entries.
//!
//! Each variable is a file named `Name-<vendor GUID>`, holding its 32-bit
//! attributes followed by its value. Writing a file (in one write, the same
//! layout) sets the variable; deleting it deletes the variable.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Attributes, FileKind, Inode, Metadata, ramfs};
use crate::drivers::block::{guid_text, parse_guid};
use crate::efi;
use crate::errno::{Errno, Result};

/// The directory of variables.
pub struct Variables {
    inode: u64,
}

pub fn create() -> Arc<dyn Inode> {
    Arc::new(Variables { inode: ramfs::next_inode_number() })
}

/// A variable's file name.
fn file_name(name: &str, vendor: &[u8; 16]) -> String {
    format!("{name}-{}", guid_text(vendor))
}

/// Splits a file name into the variable's name and vendor.
fn parse_name(file: &str) -> Option<(String, [u8; 16])> {
    let split = file.len().checked_sub(37)?;
    let (name, rest) = file.split_at(split);
    let vendor = parse_guid(rest.strip_prefix('-')?)?;
    (!name.is_empty()).then(|| (String::from(name), vendor))
}

impl Variables {
    fn file(&self, name: String, vendor: [u8; 16]) -> Arc<dyn Inode> {
        Arc::new(Variable { inode: ramfs::next_inode_number(), name, vendor })
    }
}

impl Inode for Variables {
    fn metadata(&self) -> Metadata {
        Metadata { links: 2, ..Metadata::virtual_file(FileKind::Directory, self.inode, 0o755) }
    }

    fn lookup(&self, file: &str) -> Result<Arc<dyn Inode>> {
        let (name, vendor) = parse_name(file).ok_or(Errno::ENOENT)?;
        efi::get_variable(&name, &vendor).ok_or(Errno::ENOENT)?;
        Ok(self.file(name, vendor))
    }

    /// A new variable comes into being when its file is first written.
    fn create(&self, file: &str, kind: FileKind, _attributes: Attributes) -> Result<Arc<dyn Inode>> {
        if kind != FileKind::Regular {
            return Err(Errno::EPERM);
        }
        let (name, vendor) = parse_name(file).ok_or(Errno::EINVAL)?;
        Ok(self.file(name, vendor))
    }

    fn unlink(&self, file: &str) -> Result<()> {
        let (name, vendor) = parse_name(file).ok_or(Errno::ENOENT)?;
        let (attributes, _) = efi::get_variable(&name, &vendor).ok_or(Errno::ENOENT)?;
        efi::set_variable(&name, &vendor, attributes, &[]).map_err(|_| Errno::EIO)
    }

    fn entries(&self) -> Result<Vec<(String, Arc<dyn Inode>)>> {
        Ok(efi::variable_names()
            .into_iter()
            .map(|(name, vendor)| (file_name(&name, &vendor), self.file(name, vendor)))
            .collect())
    }
}

struct Variable {
    inode: u64,
    name: String,
    vendor: [u8; 16],
}

impl Variable {
    /// The file's contents: attributes, then the value.
    fn contents(&self) -> Option<Vec<u8>> {
        let (attributes, value) = efi::get_variable(&self.name, &self.vendor)?;
        let mut contents = attributes.to_le_bytes().to_vec();
        contents.extend_from_slice(&value);
        Some(contents)
    }
}

impl Inode for Variable {
    fn metadata(&self) -> Metadata {
        let size = self.contents().map_or(0, |contents| contents.len() as u64);
        Metadata { size, ..Metadata::virtual_file(FileKind::Regular, self.inode, 0o644) }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let contents = self.contents().ok_or(Errno::ENOENT)?;
        let start = (offset as usize).min(contents.len());
        let count = buf.len().min(contents.len() - start);
        buf[..count].copy_from_slice(&contents[start..start + count]);
        Ok(count)
    }

    /// Sets the variable: the attributes and the whole value in one write.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if offset != 0 || buf.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let attributes = u32::from_le_bytes(buf[..4].try_into().unwrap());
        efi::set_variable(&self.name, &self.vendor, attributes, &buf[4..]).map_err(|_| Errno::EIO)?;
        Ok(buf.len())
    }

    fn truncate(&self, _len: u64) -> Result<()> {
        Ok(())
    }
}
