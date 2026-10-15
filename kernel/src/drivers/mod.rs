//! Device drivers.

pub mod ahci;
pub mod block;
pub mod dma;
pub mod e1000;
pub mod intel_wifi;
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
    intel_wifi::probe(&devices);
    usb::probe(&devices);
}
