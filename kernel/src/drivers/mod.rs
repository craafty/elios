//! Device drivers.

pub mod ahci;
pub mod block;
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
    ahci::probe(&devices).into_iter().map(|disk| disk as Arc<dyn Disk>).collect()
}
