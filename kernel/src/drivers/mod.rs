//! Device drivers.

pub mod ahci;
pub mod block;
pub mod e1000;
pub mod nvme;
pub mod pci;
pub mod usb;

use alloc::sync::Arc;

use self::block::Disk;

/// Finds devices and starts drivers for the ones eliOS supports. Disks
/// found are registered (see [`block::register`]).
pub fn init() {
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
    for disk in ahci::probe(&devices) {
        block::register(disk as Arc<dyn Disk>);
    }
    for disk in nvme::probe(&devices) {
        block::register(disk as Arc<dyn Disk>);
    }
    e1000::probe(&devices);
    usb::probe(&devices);
}
