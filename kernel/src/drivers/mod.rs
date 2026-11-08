//! Device drivers.

pub mod ahci;
pub mod block;
pub mod dma;
pub mod e1000;
pub mod hda;
pub mod intel_wifi;
pub mod nvme;
pub mod pci;
pub mod usb;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::{fs, task, time};

use self::block::Disk;

/// Finds devices and starts drivers for the ones eliOS supports. Disks
/// found are registered (see [`block::register`]).
pub fn init() {
    let devices = pci::scan();
    for device in &devices {
        // The subsystem IDs name the machine's maker and model (17aa is
        // Lenovo), which makes logs from real hardware easier to read.
        let subsystem = device.read(0x2C);
        debug!(
            "pci {:02x}:{:02x}.{}: {:04x}:{:04x} rev {:02x} class {:02x}.{:02x}.{:02x} subsystem {:04x}:{:04x}",
            device.bus,
            device.slot,
            device.function,
            device.vendor,
            device.device,
            device.read(0x08) as u8,
            device.class,
            device.subclass,
            device.interface,
            subsystem as u16,
            subsystem >> 16
        );
    }
    info!("pci: {} devices", devices.len());
    for disk in ahci::probe(&devices) {
        block::register(disk as Arc<dyn Disk>);
    }
    for disk in nvme::probe(&devices) {
        block::register(disk as Arc<dyn Disk>);
    }
    e1000::probe(&devices);
    hda::probe(&devices);
    intel_wifi::probe(&devices);
    usb::probe(&devices);
}

/// How long to wait for the root file system (and the firmware file).
const FIRMWARE_WAIT_MS: u64 = 60_000;

/// Reads /lib/firmware/<name>, waiting for the root file system if it
/// isn't mounted yet (drivers start before it is).
pub fn read_firmware(name: &str) -> Result<Vec<u8>, String> {
    let path = alloc::format!("/lib/firmware/{name}");
    let deadline = time::uptime_nanos() + FIRMWARE_WAIT_MS * 1_000_000;
    let inode = loop {
        match fs::lookup("/", &path, true) {
            Ok(inode) => break inode,
            Err(_) if time::uptime_nanos() < deadline => task::sleep_ms(250),
            Err(error) => return Err(alloc::format!("can't open {path}: {error:?}")),
        }
    };
    let size = inode.metadata().size as usize;
    let mut data = alloc::vec![0; size];
    let mut done = 0;
    while done < size {
        match inode.read_at(done as u64, &mut data[done..]) {
            Ok(0) => break,
            Ok(count) => done += count,
            Err(error) => return Err(alloc::format!("reading {path}: {error:?}")),
        }
    }
    data.truncate(done);
    Ok(data)
}
