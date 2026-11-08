//! `/dev/fb0`: the screen as Linux's framebuffer device ("fbdev"). A
//! graphical program (eliOS's desktop) maps the screen's memory into its
//! address space and draws there, and asks for the screen's size and pixel
//! layout with the `FBIOGET_*` ioctls.
//!
//! While any program has it open for writing, the text console stops
//! drawing (it keeps its text, and redraws it when the last such program
//! closes it, so a desktop that crashes gives the screen back).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{FileKind, Inode, Metadata, O_ACCMODE, O_RDONLY, ramfs};
use crate::console;
use crate::errno::{Errno, Result};
use crate::memory::CacheType;
use crate::process::user;
use crate::sync::Once;

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOPUT_VSCREENINFO: u64 = 0x4601;
const FBIOGET_FSCREENINFO: u64 = 0x4602;
const FBIOPAN_DISPLAY: u64 = 0x4606;
const FBIOBLANK: u64 = 0x4611;
/// `_IOW('F', 0x20, u32)`: wait for the next vertical blank (the argument,
/// which CRTC, must be 0).
const FBIO_WAITFORVSYNC: u64 = 0x4004_4620;

/// Linux's character device major number for framebuffers.
const FB_MAJOR: u64 = 29;
/// The `video` group (as on Linux), which may use the screen.
const VIDEO_GROUP: u32 = 44;

static SCREEN: Once<bootinfo::Framebuffer> = Once::new();
/// Open files that may draw: while there are any, the console is hidden.
static WRITERS: AtomicUsize = AtomicUsize::new(0);

/// Makes /dev/fb0 for the screen the bootloader found.
pub fn init(screen: &bootinfo::Framebuffer) {
    if SCREEN.set(*screen).is_err() {
        return;
    }
    let device = Arc::new(Device { inode: ramfs::next_inode_number() });
    let _ = super::devfs::directory().link("fb0", device);
}

fn screen() -> Result<&'static bootinfo::Framebuffer> {
    SCREEN.get().ok_or(Errno::ENODEV)
}

struct Device {
    inode: u64,
}

fn metadata(inode: u64) -> Metadata {
    Metadata { rdev: FB_MAJOR << 8, gid: VIDEO_GROUP, ..Metadata::virtual_file(FileKind::CharDevice, inode, 0o660) }
}

impl Inode for Device {
    fn metadata(&self) -> Metadata {
        metadata(self.inode)
    }

    fn open_instance(&self, flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        let writer = flags & O_ACCMODE != O_RDONLY;
        if writer && WRITERS.fetch_add(1, Ordering::AcqRel) == 0 {
            console::with(|console| console.set_hidden(true));
        }
        Some(Ok(Arc::new(Client { inode: self.inode, writer })))
    }
}

/// One open file of the framebuffer.
struct Client {
    inode: u64,
    writer: bool,
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.writer && WRITERS.fetch_sub(1, Ordering::AcqRel) == 1 {
            console::with(|console| console.set_hidden(false));
        }
    }
}

/// Bit positions of red, green and blue in a pixel.
fn channel_offsets(format: bootinfo::PixelFormat) -> [u32; 3] {
    match format {
        bootinfo::PixelFormat::Rgb => [0, 8, 16],
        _ => [16, 8, 0],
    }
}

impl Inode for Client {
    fn metadata(&self) -> Metadata {
        metadata(self.inode)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn device_memory(&self) -> Option<(u64, u64, u64)> {
        let screen = SCREEN.get()?;
        let size = (u64::from(screen.stride) * u64::from(screen.height) * 4).next_multiple_of(4096);
        Some((screen.base, size, CacheType::WriteCombining.flags()))
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        let screen = screen()?;
        match request {
            FBIOGET_VSCREENINFO => {
                let mut info = [0u32; 40];
                info[0] = screen.width;
                info[1] = screen.height;
                info[2] = screen.stride;
                info[3] = screen.height;
                info[6] = 32;
                let [red, green, blue] = channel_offsets(screen.format);
                // red, green, blue, transparency: offset, length, msb_right.
                info[8..20].copy_from_slice(&[red, 8, 0, green, 8, 0, blue, 8, 0, 24, 0, 0]);
                // height and width in millimetres: unknown.
                info[22] = u32::MAX;
                info[23] = u32::MAX;
                user::write_value(argument, &info)?;
                Ok(0)
            }
            // The mode can't change; asking for the current one is fine.
            FBIOPUT_VSCREENINFO | FBIOPAN_DISPLAY | FBIOBLANK => Ok(0),
            FBIOGET_FSCREENINFO => {
                let mut info = [0u8; 80];
                info[..10].copy_from_slice(b"eliOS GOP\0");
                info[16..24].copy_from_slice(&screen.base.to_le_bytes());
                let length = screen.stride * screen.height * 4;
                info[24..28].copy_from_slice(&length.to_le_bytes());
                // Packed pixels (type 0), true color (visual 2).
                info[36..40].copy_from_slice(&2u32.to_le_bytes());
                info[48..52].copy_from_slice(&(screen.stride * 4).to_le_bytes());
                user::write_bytes(argument, &info)?;
                Ok(0)
            }
            // Only where the display's scanout can be watched (Intel
            // graphics); elsewhere programs learn they can't wait.
            FBIO_WAITFORVSYNC => {
                if crate::drivers::intel_display::wait_for_vblank() {
                    Ok(0)
                } else {
                    Err(Errno::ENOTTY)
                }
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}
