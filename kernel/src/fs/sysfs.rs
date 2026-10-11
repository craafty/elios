//! /sys: devices described as files, laid out as on Linux so tools that
//! look there (and eliOS's installer) find them.
//!
//! - `/sys/block/<disk>/`: `size` (in 512-byte sectors), `removable`, `ro`,
//!   `device/model`, and a directory per partition with `partition` (its
//!   number), `start` and `size`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::Inode;
use super::procfs::{Listing, directory, generated};
use crate::drivers::block::{self, Disk, Partition};

/// The /sys directory.
pub fn create() -> Arc<dyn Inode> {
    directory(|| alloc::vec![("block".into(), directory(block_listing))])
}

fn block_listing() -> Listing {
    block::disks().into_iter().map(|disk| (disk.name().to_string(), disk_directory(disk))).collect()
}

/// A file holding a value and a newline.
fn value(text: impl ToString) -> Arc<dyn Inode> {
    let text = text.to_string();
    generated(move || format!("{text}\n"))
}

fn disk_directory(disk: Arc<dyn Disk>) -> Arc<dyn Inode> {
    directory(move || {
        let model: String = disk.model().into();
        let mut entries: Listing = alloc::vec![
            ("size".into(), value(disk.sectors())),
            ("removable".into(), value(u8::from(disk.removable()))),
            ("ro".into(), value(0)),
            ("dev".into(), value(device_text(&Partition::whole(&disk)))),
            ("device".into(), directory(move || alloc::vec![("model".into(), value(model.clone()))])),
        ];
        let partitions: Vec<Partition> = block::partitions(&disk);
        for partition in partitions {
            entries.push((partition.name(), partition_directory(partition)));
        }
        entries
    })
}

fn device_text(partition: &Partition) -> String {
    let (major, minor) = block::device_numbers(partition);
    format!("{major}:{minor}")
}

fn partition_directory(partition: Partition) -> Arc<dyn Inode> {
    directory(move || {
        alloc::vec![
            ("partition".into(), value(partition.number)),
            ("start".into(), value(partition.start)),
            ("size".into(), value(partition.sectors)),
            ("dev".into(), value(device_text(&partition))),
            ("uuid".into(), value(block::guid_text(&partition.guid))),
        ]
    })
}
