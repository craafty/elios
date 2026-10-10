//! Hardware information the kernel needs that only the firmware knows.

use core::ffi::c_void;
use core::ptr;

use bootinfo::{Framebuffer, PixelFormat};

use crate::uefi::{
    self, ACPI_10_TABLE, ACPI_20_TABLE, GRAPHICS_OUTPUT_PROTOCOL, GraphicsOutput, PIXEL_BGR_RESERVED_8BIT,
    PIXEL_RGB_RESERVED_8BIT,
};

/// The linear framebuffer of the current video mode, if there is one we can
/// draw to directly.
pub fn framebuffer() -> Option<Framebuffer> {
    let mut gop: *mut GraphicsOutput = ptr::null_mut();
    // SAFETY: firmware call with a valid out-pointer; on success the protocol
    // and its mode information are valid while boot services run.
    unsafe {
        let status = (uefi::boot_services().locate_protocol)(
            &GRAPHICS_OUTPUT_PROTOCOL,
            ptr::null_mut(),
            (&raw mut gop).cast::<*mut c_void>(),
        );
        if status.is_error() {
            return None;
        }
        let mode = &*(*gop).mode;
        let info = &*mode.info;
        let format = match info.pixel_format {
            PIXEL_RGB_RESERVED_8BIT => PixelFormat::Rgb,
            PIXEL_BGR_RESERVED_8BIT => PixelFormat::Bgr,
            // Bitmask formats are vanishingly rare, and "blit only" modes
            // have no framebuffer at all.
            _ => return None,
        };
        Some(Framebuffer {
            base: mode.frame_buffer_base,
            size: mode.frame_buffer_size as u64,
            width: info.horizontal_resolution,
            height: info.vertical_resolution,
            stride: info.pixels_per_scan_line,
            format,
        })
    }
}

/// Physical address of the ACPI RSDP, preferring the ACPI 2.0+ version.
pub fn find_rsdp() -> Option<u64> {
    let tables = uefi::system_table()?.configuration_tables();
    let find = |guid| tables.iter().find(|t| t.vendor_guid == guid).map(|t| t.vendor_table as u64);
    find(ACPI_20_TABLE).or_else(|| find(ACPI_10_TABLE))
}
