//! /sys: devices described as files, laid out as on Linux so tools that
//! look there (and eliOS's installer) find them.
//!
//! - `/sys/block/<disk>/`: `size` (in 512-byte sectors), `removable`, `ro`,
//!   `device/model`, and a directory per partition with `partition` (its
//!   number), `start` and `size`.
//! - `/sys/firmware/efi/efivars/`: UEFI variables (see `efivarfs.rs`).
//! - `/sys/class/power_supply/`: on ThinkPads, `AC` (`online`) and `BAT0`
//!   (`status`, `capacity` in percent, energy in µWh or charge in µAh,
//!   power in µW or current in µA, `voltage_now` in µV), read from the
//!   embedded controller (`drivers/thinkpad_ec.rs`).
//! - `/sys/class/backlight/intel_backlight/`: `brightness` (which the
//!   `netdev` group, everyone at the computer, may write),
//!   `max_brightness`, `actual_brightness`, `type`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::Inode;
use super::procfs::{Listing, NETWORK_GROUP, control_by, directory, generated};
use crate::drivers::block::{self, Disk, Partition};
use crate::drivers::thinkpad_ec::{self, State};

/// The /sys directory.
pub fn create() -> Arc<dyn Inode> {
    directory(|| {
        let mut entries: Listing = alloc::vec![
            ("block".into(), directory(block_listing)),
            (
                "class".into(),
                directory(|| {
                    alloc::vec![
                        ("power_supply".into(), directory(power_supplies)),
                        ("backlight".into(), directory(backlights)),
                    ]
                })
            ),
        ];
        // /sys/firmware/efi exists on UEFI systems, which is how programs
        // tell; its efivars directory holds the firmware's variables.
        if crate::efi::available() {
            let efi = directory(|| alloc::vec![("efivars".into(), super::efivarfs::create())]);
            entries.push(("firmware".into(), directory(move || alloc::vec![("efi".into(), efi.clone())])));
        }
        entries
    })
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
        let name: String = disk.name().into();
        let mut entries: Listing = alloc::vec![
            ("size".into(), value(disk.sectors())),
            ("stat".into(), generated(move || block::statistics_text(&name))),
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

fn power_supplies() -> Listing {
    let Some(reading) = thinkpad_ec::reading() else { return Listing::new() };
    let mut entries: Listing = alloc::vec![(
        "AC".into(),
        directory(move || {
            alloc::vec![("type".into(), value("Mains")), ("online".into(), value(u8::from(reading.plugged_in)))]
        }),
    )];
    if let Some(battery) = reading.battery {
        entries.push(("BAT0".into(), directory(move || battery_files(battery))));
    }
    entries
}

fn battery_files(battery: thinkpad_ec::Battery) -> Listing {
    let status = match battery.state {
        State::Charging => "Charging",
        State::Discharging => "Discharging",
        State::Idle if battery.percent() >= 95 => "Full",
        State::Idle => "Not charging",
    };
    // Linux names the values by their unit: energy (µWh) and power (µW),
    // or charge (µAh) and current (µA).
    let (amount, flow) = if battery.in_mwh { ("energy", "power") } else { ("charge", "current") };
    alloc::vec![
        ("type".into(), value("Battery")),
        ("present".into(), value(1)),
        ("status".into(), value(status)),
        ("capacity".into(), value(battery.percent())),
        ("technology".into(), value("Li-ion")),
        (format!("{amount}_now"), value(u64::from(battery.remaining) * 1000)),
        (format!("{amount}_full"), value(u64::from(battery.full) * 1000)),
        (format!("{amount}_full_design"), value(u64::from(battery.design) * 1000)),
        (format!("{flow}_now"), value(u64::from(battery.rate) * 1000)),
        ("voltage_now".into(), value(u64::from(battery.millivolts) * 1000)),
    ]
}

fn backlights() -> Listing {
    use crate::drivers::intel_display;
    let Some((_, most)) = intel_display::backlight() else { return Listing::new() };
    let brightness = || intel_display::backlight().map_or(0, |(brightness, _)| brightness);
    let set = |text: &[u8]| {
        let value = core::str::from_utf8(text).ok().and_then(|text| text.trim().parse().ok());
        match value {
            Some(value) if intel_display::set_backlight(value) => Ok(()),
            _ => Err(crate::errno::Errno::EINVAL),
        }
    };
    alloc::vec![(
        "intel_backlight".into(),
        directory(move || {
            alloc::vec![
                ("type".into(), value("raw")),
                ("max_brightness".into(), value(most)),
                ("brightness".into(), control_by(move || format!("{}\n", brightness()), set, Some(NETWORK_GROUP))),
                ("actual_brightness".into(), generated(move || format!("{}\n", brightness()))),
            ]
        }),
    )]
}
