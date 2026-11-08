//! Input devices as Linux presents them: `/dev/input/eventN` files ("evdev")
//! that read as a stream of `struct input_event`s, each a type (key,
//! relative motion, absolute position, ...), a code (which key or axis) and
//! a value, with a `SYN_REPORT` event closing each group that belongs
//! together. Graphical programs (and the libraries they use, like libinput)
//! read input this way, so keyboards and mice appear here whatever their
//! driver: PS/2 or USB.
//!
//! Every open file gets its own queue, so two readers each see every event.
//! A reader can "grab" a device (`EVIOCGRAB`), and then only it sees the
//! events; a grabbed keyboard also stops typing at the text console, which
//! is how a graphical session takes over the keyboard.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, ramfs};
use crate::process::{self, user};
use crate::sync::SpinLock;
use crate::time;

// Event types.
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_MSC: u16 = 0x04;
pub const EV_LED: u16 = 0x11;
pub const EV_REP: u16 = 0x14;

const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// `INPUT_PROP_POINTER`: the device moves a pointer (a mouse or touchpad).
pub const PROP_POINTER: u32 = 1 << 0;

pub const BUS_USB: u16 = 0x03;
pub const BUS_BLUETOOTH: u16 = 0x05;
pub const BUS_I8042: u16 = 0x11;

/// The highest key code (`KEY_MAX`).
const KEY_MAX: usize = 0x2FF;
const QUEUE_LIMIT: usize = 1024;
/// Linux's character device major number for input devices; evdev files
/// are minors 64 and up.
const INPUT_MAJOR: u64 = 13;
/// The `input` group, whose members (the computer's users) may read input
/// devices.
const INPUT_GROUP: u32 = 105;

/// What a device can report, for the `EVIOCGBIT` family of ioctls.
#[derive(Clone)]
pub struct Capabilities {
    pub keys: Vec<u16>,
    pub relative: Vec<u16>,
    pub leds: Vec<u16>,
    pub properties: u32,
}

/// One input device and its file.
pub struct Device {
    pub name: String,
    /// Bus type, vendor, product, version (`struct input_id`).
    id: [u16; 4],
    capabilities: Capabilities,
    inode: u64,
    minor: u64,
    clients: SpinLock<Vec<Weak<Client>>>,
    /// Keys held down, one bit per key code.
    keys_down: SpinLock<[u64; KEY_MAX / 64 + 1]>,
    /// The reader that grabbed the device, if one has (by its address).
    grabbed_by: SpinLock<Option<usize>>,
}

/// One open file of a device: its own queue of events.
pub struct Client {
    device: Arc<Device>,
    queue: SpinLock<VecDeque<Event>>,
    changed: Arc<fs::PollSource>,
    /// Report times on the monotonic clock instead of the real-time one.
    monotonic: core::sync::atomic::AtomicBool,
}

#[derive(Clone, Copy)]
struct Event {
    /// Nanoseconds since boot.
    time: u64,
    kind: u16,
    code: u16,
    value: i32,
}

static DEVICES: SpinLock<Vec<Arc<Device>>> = SpinLock::new(Vec::new());
static NEXT_NUMBER: AtomicUsize = AtomicUsize::new(0);

/// Registers a device and makes its `/dev/input/eventN` file.
pub fn register(name: &str, id: [u16; 4], capabilities: Capabilities) -> Arc<Device> {
    let number = NEXT_NUMBER.fetch_add(1, Ordering::Relaxed);
    let device = Arc::new(Device {
        name: String::from(name),
        id,
        capabilities,
        inode: ramfs::next_inode_number(),
        minor: 64 + number as u64,
        clients: SpinLock::new(Vec::new()),
        keys_down: SpinLock::new([0; KEY_MAX / 64 + 1]),
        grabbed_by: SpinLock::new(None),
    });
    DEVICES.lock().push(device.clone());
    if let Some(directory) = directory() {
        let _ = directory.link(&alloc::format!("event{number}"), device.clone());
    }
    debug!("input: event{number}: {name}");
    device
}

/// Removes a device that's gone (a USB keyboard unplugged). Readers get
/// `ENODEV` once their queue is empty.
pub fn unregister(device: &Arc<Device>) {
    DEVICES.lock().retain(|other| !Arc::ptr_eq(other, device));
    if let Some(directory) = directory() {
        let name = alloc::format!("event{}", device.minor - 64);
        let _ = directory.unlink(&name);
    }
    device.wake_clients();
}

/// The device called `name`, if there is one.
#[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
pub fn find(name: &str) -> Option<Arc<Device>> {
    DEVICES.lock().iter().find(|device| device.name == name).cloned()
}

/// /dev/input, made on first use.
fn directory() -> Option<Arc<dyn Inode>> {
    let dev = fs::devfs::directory();
    if let Ok(directory) = dev.lookup("input") {
        return Some(directory);
    }
    let directory: Arc<dyn Inode> = ramfs::Directory::new(fs::Attributes::root(0o755));
    dev.link("input", directory.clone()).ok()?;
    Some(directory)
}

/// Whether some reader has grabbed a keyboard, so key presses shouldn't
/// also go to the text console.
pub fn keyboard_grabbed() -> bool {
    DEVICES.lock().iter().any(|device| device.capabilities.keys.len() > 32 && device.grabbed_by.lock().is_some())
}

impl Device {
    /// Reports events (`(type, code, value)`), followed by a `SYN_REPORT`.
    /// Key events also update the state `EVIOCGKEY` reports.
    pub fn report(&self, events: &[(u16, u16, i32)]) {
        let now = time::uptime_nanos();
        {
            let mut keys = self.keys_down.lock();
            for &(kind, code, value) in events {
                if kind == EV_KEY && usize::from(code) <= KEY_MAX {
                    let (word, bit) = (usize::from(code) / 64, code % 64);
                    match value {
                        0 => keys[word] &= !(1 << bit),
                        _ => keys[word] |= 1 << bit,
                    }
                }
            }
        }
        let grabbed_by = *self.grabbed_by.lock();
        let clients: Vec<Arc<Client>> = {
            let mut clients = self.clients.lock();
            clients.retain(|client| client.strong_count() > 0);
            clients.iter().filter_map(Weak::upgrade).collect()
        };
        for client in clients {
            if grabbed_by.is_some_and(|owner| owner != Arc::as_ptr(&client) as usize) {
                continue;
            }
            let mut queue = client.queue.lock();
            if queue.len() + events.len() + 1 > QUEUE_LIMIT {
                // The reader fell behind: drop what's queued and say so,
                // as Linux does; it can then reread the state.
                queue.clear();
                queue.push_back(Event { time: now, kind: EV_SYN, code: SYN_DROPPED, value: 0 });
            }
            for &(kind, code, value) in events {
                queue.push_back(Event { time: now, kind, code, value });
            }
            queue.push_back(Event { time: now, kind: EV_SYN, code: SYN_REPORT, value: 0 });
            drop(queue);
            client.changed.notify();
        }
    }

    fn wake_clients(&self) {
        for client in self.clients.lock().iter().filter_map(Weak::upgrade) {
            client.changed.notify();
        }
    }

    fn rdev(&self) -> u64 {
        (self.minor & 0xFF) | INPUT_MAJOR << 8 | (self.minor & !0xFF) << 12
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            rdev: self.rdev(),
            gid: INPUT_GROUP,
            ..Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o660)
        }
    }

    fn present(self: &Arc<Self>) -> bool {
        DEVICES.lock().iter().any(|device| Arc::ptr_eq(device, self))
    }
}

impl Inode for Device {
    fn metadata(&self) -> Metadata {
        Device::metadata(self)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        let device = DEVICES.lock().iter().find(|device| device.inode == self.inode).cloned();
        let Some(device) = device else { return Some(Err(Errno::ENODEV)) };
        let client = Arc::new(Client {
            device: device.clone(),
            queue: SpinLock::new(VecDeque::new()),
            changed: Arc::new(fs::PollSource::new()),
            monotonic: core::sync::atomic::AtomicBool::new(false),
        });
        device.clients.lock().push(Arc::downgrade(&client));
        Some(Ok(client))
    }
}

// ioctl request encoding: direction (bits 30-31), size (16-29), type
// ('E' for evdev, bits 8-15) and number (0-7).
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

fn ioctl_parts(request: u64) -> (u64, usize, u8, u8) {
    (request >> 30 & 3, (request >> 16 & 0x3FFF) as usize, (request >> 8) as u8, request as u8)
}

/// Copies as much of a bit mask as fits in the caller's `size` bytes;
/// returns the size copied (what Linux returns).
fn write_bits(address: u64, size: usize, bits: &[u16]) -> Result<u64> {
    let mut bytes = alloc::vec![0u8; size];
    for &bit in bits {
        if let Some(byte) = bytes.get_mut(usize::from(bit) / 8) {
            *byte |= 1 << (bit % 8);
        }
    }
    user::write_bytes(address, &bytes)?;
    Ok(size as u64)
}

impl Client {
    fn handle_ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        let (direction, size, kind, number) = ioctl_parts(request);
        if kind != b'E' {
            return Err(Errno::ENOTTY);
        }
        let device = &self.device;
        let capabilities = &device.capabilities;
        match (direction, number) {
            // EVIOCGVERSION
            (IOC_READ, 0x01) => user::write_value(argument, &0x0001_0001u32).map(|()| 0),
            // EVIOCGID
            (IOC_READ, 0x02) => user::write_value(argument, &device.id).map(|()| 0),
            // EVIOCGREP: repeat delay and period, in milliseconds.
            (IOC_READ, 0x03) => user::write_value(argument, &[250u32, 33]).map(|()| 0),
            // EVIOCGNAME, EVIOCGPHYS, EVIOCGUNIQ
            (IOC_READ, 0x06..=0x08) => {
                let text = match number {
                    0x06 => device.name.as_str(),
                    0x07 => "",
                    _ => return Err(Errno::ENOENT),
                };
                let mut bytes = Vec::from(text.as_bytes());
                bytes.push(0);
                bytes.truncate(size);
                user::write_bytes(argument, &bytes)?;
                Ok(bytes.len() as u64)
            }
            // EVIOCGPROP
            (IOC_READ, 0x09) => {
                let properties: Vec<u16> = (0..32).filter(|bit| capabilities.properties & 1 << bit != 0).collect();
                write_bits(argument, size, &properties)
            }
            // EVIOCGKEY: which keys are held.
            (IOC_READ, 0x18) => {
                let keys = *device.keys_down.lock();
                let bytes: Vec<u8> = keys.iter().flat_map(|word| word.to_le_bytes()).take(size).collect();
                user::write_bytes(argument, &bytes)?;
                Ok(bytes.len() as u64)
            }
            // EVIOCGLED, EVIOCGSND, EVIOCGSW: nothing on.
            (IOC_READ, 0x19..=0x1B) => write_bits(argument, size, &[]),
            // EVIOCGBIT(type): the event types, or the codes of one type.
            (IOC_READ, 0x20..=0x3F) => {
                let codes: Vec<u16> = match u16::from(number - 0x20) {
                    0 => {
                        let mut types = alloc::vec![EV_SYN];
                        if !capabilities.keys.is_empty() {
                            types.extend([EV_KEY, EV_MSC, EV_REP]);
                        }
                        if !capabilities.relative.is_empty() {
                            types.push(EV_REL);
                        }
                        if !capabilities.leds.is_empty() {
                            types.push(EV_LED);
                        }
                        types
                    }
                    EV_KEY => capabilities.keys.clone(),
                    EV_REL => capabilities.relative.clone(),
                    EV_LED => capabilities.leds.clone(),
                    // MSC_SCAN
                    EV_MSC if !capabilities.keys.is_empty() => alloc::vec![4],
                    _ => Vec::new(),
                };
                write_bits(argument, size, &codes)
            }
            // EVIOCGABS: no absolute axes yet.
            (IOC_READ, 0x40..=0x7F) => Err(Errno::EINVAL),
            // EVIOCGRAB
            (IOC_WRITE, 0x90) => {
                let me = self as *const Self as usize;
                let mut owner = device.grabbed_by.lock();
                if argument != 0 {
                    if owner.is_some_and(|owner| owner != me) {
                        return Err(Errno::EBUSY);
                    }
                    *owner = Some(me);
                } else if *owner == Some(me) {
                    *owner = None;
                } else {
                    return Err(Errno::EINVAL);
                }
                Ok(0)
            }
            // EVIOCSCLOCKID
            (IOC_WRITE, 0xA0) => {
                let clock: i32 = user::read_value(argument)?;
                // CLOCK_REALTIME is 0, CLOCK_MONOTONIC 1, CLOCK_BOOTTIME 7.
                self.monotonic.store(clock != 0, Ordering::Relaxed);
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

impl Inode for Client {
    fn metadata(&self) -> Metadata {
        self.device.metadata()
    }

    fn seekable(&self) -> bool {
        false
    }

    /// Reads whole events (24 bytes each: seconds, microseconds, type,
    /// code, value), waiting for at least one.
    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        const SIZE: usize = 24;
        if buf.len() < SIZE {
            return Err(Errno::EINVAL);
        }
        let monotonic = self.monotonic.load(Ordering::Relaxed);
        let offset = if monotonic { 0 } else { time::unix_nanos() - time::uptime_nanos() };
        self.changed.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut queue = self.queue.lock();
            if queue.is_empty() {
                if !self.device.present() {
                    return Some(Err(Errno::ENODEV));
                }
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            let count = (buf.len() / SIZE).min(queue.len());
            for (chunk, event) in buf.as_chunks_mut::<SIZE>().0.iter_mut().zip(queue.drain(..count)) {
                let nanos = event.time + offset;
                chunk[0..8].copy_from_slice(&(nanos / 1_000_000_000).to_le_bytes());
                chunk[8..16].copy_from_slice(&(nanos % 1_000_000_000 / 1000).to_le_bytes());
                chunk[16..18].copy_from_slice(&event.kind.to_le_bytes());
                chunk[18..20].copy_from_slice(&event.code.to_le_bytes());
                chunk[20..24].copy_from_slice(&event.value.to_le_bytes());
            }
            Some(Ok(count * SIZE))
        })
    }

    /// Writing events (to set LEDs, say) is accepted and ignored.
    fn write_stream(&self, buf: &[u8], _nonblocking: bool) -> Result<usize> {
        Ok(buf.len())
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        self.handle_ioctl(request, argument)
    }

    fn poll(&self, _flags: u32) -> u16 {
        let ready = if self.queue.lock().is_empty() { 0 } else { fs::POLLIN };
        if !self.device.present() { ready | fs::POLLHUP | fs::POLLERR } else { ready | fs::POLLOUT }
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<fs::PollSource>>, _depth: usize) -> bool {
        sources.push(self.changed.clone());
        true
    }
}

impl Drop for Client {
    /// Closing the file ends its grab.
    fn drop(&mut self) {
        let me = self as *const Self as usize;
        let mut owner = self.device.grabbed_by.lock();
        if *owner == Some(me) {
            *owner = None;
        }
    }
}

/// Every key code a standard keyboard can produce, for its capabilities.
pub fn keyboard_keys() -> Vec<u16> {
    let mut keys: Vec<u16> = (1..=0x58).collect();
    keys.extend(SET1_EXTENDED.iter().map(|&(_, key)| key));
    keys.extend(USB_EXTRA_KEYS);
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Scancode set 1 codes after an `E0` prefix, and the Linux key codes they
/// are. (Codes without the prefix, 0x01-0x58, are the key codes
/// themselves: Linux's numbering started as set 1's.)
const SET1_EXTENDED: [(u8, u16); 26] = [
    (0x1C, 96),  // keypad Enter
    (0x1D, 97),  // right Ctrl
    (0x35, 98),  // keypad /
    (0x37, 99),  // Print Screen
    (0x38, 100), // right Alt
    (0x47, 102), // Home
    (0x48, 103), // Up
    (0x49, 104), // Page Up
    (0x4B, 105), // Left
    (0x4D, 106), // Right
    (0x4F, 107), // End
    (0x50, 108), // Down
    (0x51, 109), // Page Down
    (0x52, 110), // Insert
    (0x53, 111), // Delete
    (0x5B, 125), // left Windows (Meta)
    (0x5C, 126), // right Windows
    (0x5D, 127), // Menu
    (0x20, 113), // Mute
    (0x2E, 114), // Volume Down
    (0x30, 115), // Volume Up
    (0x5E, 116), // Power
    (0x5F, 142), // Sleep
    (0x22, 164), // Play/Pause
    (0x10, 165), // Previous track
    (0x19, 163), // Next track
];

/// Turns scancode set 1 bytes into (Linux key code, pressed) pairs.
#[derive(Default)]
pub struct Set1Decoder {
    extended: bool,
}

impl Set1Decoder {
    pub fn feed(&mut self, scancode: u8) -> Option<(u16, bool)> {
        if scancode == 0xE0 {
            self.extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let pressed = scancode & 0x80 == 0;
        let code = scancode & 0x7F;
        let key = if extended {
            // Fake shifts some keyboards send around extended keys.
            if code == 0x2A || code == 0x36 {
                return None;
            }
            SET1_EXTENDED.iter().find(|&&(scan, _)| scan == code)?.1
        } else {
            match code {
                0x01..=0x58 => u16::from(code),
                _ => return None,
            }
        };
        Some((key, pressed))
    }
}

/// Key codes USB keyboards have beyond the ones set 1 gives.
const USB_EXTRA_KEYS: [u16; 4] = [85, 86, 117, 119];

/// The Linux key code for a USB HID keyboard usage (page 7), from the HID
/// usage tables.
pub fn usb_key(usage: u8) -> Option<u16> {
    #[rustfmt::skip]
    const TABLE: [u8; 0x68] = [
        0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,        // 0x00: a-l
        50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3,     // 0x10: m-z, 1, 2
        4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26,          // 0x20: 3-0, Enter...
        27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,   // 0x30
        65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106, // 0x40
        105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, // 0x50
        72, 73, 82, 83, 86, 127, 116, 117,                                 // 0x60
    ];
    match usage {
        // Modifiers: left Ctrl, Shift, Alt, Meta, then the right ones.
        0xE0..=0xE7 => Some([29, 42, 56, 125, 97, 54, 100, 126][usize::from(usage - 0xE0)]),
        0x7F => Some(113),
        0x80 => Some(115),
        0x81 => Some(114),
        _ => TABLE.get(usize::from(usage)).copied().filter(|&key| key != 0).map(u16::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn set1_scancodes_become_key_codes() {
            let mut decoder = Set1Decoder::default();
            // a down, a up, E0 48 (Up) down, E0 C8 up, E0 2A (fake shift) ignored.
            assert_eq!(decoder.feed(0x1E), Some((30, true)));
            assert_eq!(decoder.feed(0x9E), Some((30, false)));
            assert_eq!(decoder.feed(0xE0), None);
            assert_eq!(decoder.feed(0x48), Some((103, true)));
            assert_eq!(decoder.feed(0xE0), None);
            assert_eq!(decoder.feed(0xC8), Some((103, false)));
            assert_eq!(decoder.feed(0xE0), None);
            assert_eq!(decoder.feed(0x2A), None);
        }

        fn usb_usages_become_key_codes() {
            assert_eq!(usb_key(0x04), Some(30)); // a
            assert_eq!(usb_key(0x1D), Some(44)); // z
            assert_eq!(usb_key(0x27), Some(11)); // 0
            assert_eq!(usb_key(0x28), Some(28)); // Enter
            assert_eq!(usb_key(0x52), Some(103)); // Up
            assert_eq!(usb_key(0xE1), Some(42)); // left Shift
            assert_eq!(usb_key(0x00), None);
        }

        fn readers_get_their_own_copy_of_events() {
            let capabilities =
                Capabilities { keys: alloc::vec![BTN_LEFT], relative: alloc::vec![REL_X], leds: Vec::new(), properties: 0 };
            let device = register("test device", [0, 0, 0, 0], capabilities);
            let first = device.open_instance(0).unwrap().unwrap();
            let second = device.open_instance(0).unwrap().unwrap();
            device.report(&[(EV_REL, REL_X, -5)]);
            for reader in [&first, &second] {
                let mut buffer = [0u8; 24 * 4];
                let count = reader.read_stream(&mut buffer, true).unwrap();
                assert_eq!(count, 48);
                assert_eq!(&buffer[16..24], &[2, 0, 0, 0, 0xFB, 0xFF, 0xFF, 0xFF]);
                // Then SYN_REPORT.
                assert_eq!(&buffer[40..48], &[0; 8]);
                assert_eq!(reader.read_stream(&mut buffer, true), Err(Errno::EAGAIN));
            }
            unregister(&device);
            let mut buffer = [0u8; 24];
            assert_eq!(first.read_stream(&mut buffer, true), Err(Errno::ENODEV));
        }

        fn a_grabbing_reader_gets_events_alone() {
            let capabilities =
                Capabilities { keys: alloc::vec![BTN_LEFT], relative: Vec::new(), leds: Vec::new(), properties: 0 };
            let device = register("grab test", [0, 0, 0, 0], capabilities);
            let first = device.open_instance(0).unwrap().unwrap();
            let second = device.open_instance(0).unwrap().unwrap();
            let client = |inode: &Arc<dyn Inode>| inode.ioctl(0x4004_4590, 1);
            assert_eq!(client(&first), Ok(0));
            assert_eq!(client(&second), Err(Errno::EBUSY));
            device.report(&[(EV_KEY, BTN_LEFT, 1)]);
            let mut buffer = [0u8; 48];
            assert_eq!(first.read_stream(&mut buffer, true), Ok(48));
            assert_eq!(second.read_stream(&mut buffer, true), Err(Errno::EAGAIN));
            drop(first);
            device.report(&[(EV_KEY, BTN_LEFT, 0)]);
            assert_eq!(second.read_stream(&mut buffer, true), Ok(48));
            unregister(&device);
        }
    }
}
