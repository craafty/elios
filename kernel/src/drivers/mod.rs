//! Device drivers.

pub mod ahci;
pub mod block;
pub mod nvme;
pub mod pci;

use alloc::sync::Arc;
use alloc::vec::Vec;

use self::block::Disk;

/// Finds devices and starts drivers for the ones eliOS supports. Returns the
/// disks found.
pub fn init() -> Vec<Arc<dyn Disk>> {
    let devices = pci::scan();
    for device in &devices {
        debug!(
            "pci {:02x}:{:02x}.{}: {:04x}:{:04x} class {:02x}.{:02x}.{:02x}",
            device.bus,
            device.slot,
            device.function,
            device.vendor,
            device.device,
            device.class,
            device.subclass,
            device.interface
        );
    }
    info!("pci: {} devices", devices.len());
    let mut disks: Vec<Arc<dyn Disk>> = Vec::new();
    disks.extend(ahci::probe(&devices).into_iter().map(|disk| disk as Arc<dyn Disk>));
    disks.extend(nvme::probe(&devices).into_iter().map(|disk| disk as Arc<dyn Disk>));
    disks
}
