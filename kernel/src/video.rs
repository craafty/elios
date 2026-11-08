//! Cameras, through Linux's V4L2 (Video4Linux 2) interface, so programs
//! built for Linux (Firefox's WebRTC, for video calls) capture on eliOS
//! unchanged.
//!
//! Each camera is a `/dev/videoN` file. A program asks what pictures the
//! camera makes (formats, then sizes, then frame rates), picks one, asks
//! for a few buffers and maps them, queues them, and starts the stream.
//! The driver ([`Camera`]; `drivers::usb::video` for USB cameras) fills
//! the queued buffers in turn with whole frames; the program dequeues each,
//! reads it in place, and queues it again. A frame that arrives with no
//! buffer queued is dropped, as on Linux, so a slow program sees fewer
//! frames rather than older ones.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, POLLERR, POLLHUP, POLLIN, PollSource, ramfs};
use crate::memory::PAGE_SIZE;
use crate::memory::object::MemoryObject;
use crate::process::{self, user};
use crate::sync::SpinLock;
use crate::time;

/// The `video` group, which may use cameras.
const VIDEO_GROUP: u32 = 44;
/// Video4Linux's character device major number.
const MAJOR: u64 = 81;

/// `YUYV`: 4:2:2 luma and chroma interleaved, two bytes a pixel.
pub const FOURCC_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// `MJPG`: each frame a JPEG picture.
pub const FOURCC_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
/// `NV12`: a luma plane, then interleaved half-size chroma.
pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A picture size a format comes in, and the frame rates it can be had at.
#[derive(Debug, Clone)]
pub struct FrameSize {
    /// The driver's number for it.
    pub index: u8,
    pub width: u16,
    pub height: u16,
    /// Time between frames, in 100 ns units (UVC's unit), fastest first.
    pub intervals: Vec<u32>,
    /// The largest frame, in bytes.
    pub max_bytes: u32,
}

#[derive(Debug, Clone)]
pub struct Format {
    /// The driver's number for it.
    pub index: u8,
    pub fourcc: u32,
    pub frames: Vec<FrameSize>,
}

impl Format {
    pub fn compressed(&self) -> bool {
        self.fourcc == FOURCC_MJPEG
    }

    fn description(&self) -> &'static str {
        match self.fourcc {
            FOURCC_YUYV => "YUYV 4:2:2",
            FOURCC_MJPEG => "Motion-JPEG",
            FOURCC_NV12 => "Y/UV 4:2:0",
            _ => "Unknown",
        }
    }

    /// Bytes per row and per frame at `size`.
    pub fn layout(&self, size: &FrameSize) -> (u32, u32) {
        let (width, height) = (u32::from(size.width), u32::from(size.height));
        match self.fourcc {
            FOURCC_YUYV => (width * 2, width * height * 2),
            FOURCC_NV12 => (width, width * height * 3 / 2),
            _ => (0, size.max_bytes),
        }
    }
}

/// What a camera's driver does.
pub trait Camera: Send + Sync {
    fn formats(&self) -> &[Format];
    /// Starts streaming frames of `format` (index into [`Camera::formats`])
    /// at `size` (index into its frames) and `interval`, handing each to
    /// [`Device::frame`].
    fn start(&self, format: usize, size: usize, interval: u32, device: Arc<Device>) -> Result<()>;
    fn stop(&self);
}

struct Buffer {
    queued: bool,
    done: bool,
    bytes_used: u32,
    sequence: u32,
    /// When the frame was finished, in nanoseconds since boot.
    timestamp: u64,
}

struct State {
    format: usize,
    size: usize,
    interval: u32,
    buffers: Vec<Buffer>,
    /// Each buffer's share of the memory object, whole pages.
    buffer_bytes: u64,
    memory: Option<Arc<MemoryObject>>,
    /// The open file the buffers belong to.
    owner: Option<usize>,
    queued: VecDeque<usize>,
    done: VecDeque<usize>,
    streaming: bool,
    sequence: u32,
}

/// A camera, as `/dev/videoN`.
pub struct Device {
    pub index: usize,
    inode: u64,
    pub name: String,
    camera: Arc<dyn Camera>,
    state: SpinLock<State>,
    events: Arc<PollSource>,
    gone: AtomicBool,
}

static DEVICES: SpinLock<Vec<Arc<Device>>> = SpinLock::new(Vec::new());
static NEXT_FILE: AtomicUsize = AtomicUsize::new(1);

/// Makes a `/dev/videoN` for a camera.
pub fn register(name: String, camera: Arc<dyn Camera>) -> Option<Arc<Device>> {
    if camera.formats().iter().all(|format| format.frames.is_empty()) {
        return None;
    }
    let mut devices = DEVICES.lock();
    let index = (0..).find(|index| devices.iter().all(|d| d.index != *index)).unwrap_or(0);
    let format = camera.formats().iter().position(|format| !format.frames.is_empty()).unwrap_or(0);
    let interval = camera.formats()[format].frames[0].intervals.first().copied().unwrap_or(333_333);
    let device = Arc::new(Device {
        index,
        inode: ramfs::next_inode_number(),
        name,
        camera,
        state: SpinLock::new(State {
            format,
            size: 0,
            interval,
            buffers: Vec::new(),
            buffer_bytes: 0,
            memory: None,
            owner: None,
            queued: VecDeque::new(),
            done: VecDeque::new(),
            streaming: false,
            sequence: 0,
        }),
        events: Arc::new(PollSource::new()),
        gone: AtomicBool::new(false),
    });
    devices.push(device.clone());
    drop(devices);
    let node = Arc::new(VideoNode { device: device.clone() });
    let _ = fs::devfs::directory().link(&alloc::format!("video{index}"), node);
    info!("video: /dev/video{index}: {}", device.name);
    Some(device)
}

/// Removes an unplugged camera's file; programs using it get `ENODEV`.
pub fn unregister(device: &Arc<Device>) {
    device.gone.store(true, Ordering::Release);
    device.state.lock().streaming = false;
    device.events.notify();
    DEVICES.lock().retain(|d| !Arc::ptr_eq(d, device));
    let _ = fs::devfs::directory().unlink(&alloc::format!("video{}", device.index));
    info!("video: /dev/video{}: removed", device.index);
}

impl Device {
    /// A finished frame from the driver: copied into the next queued
    /// buffer, if there is one.
    pub fn frame(&self, data: &[u8]) {
        let (index, memory, offset, capacity) = {
            let mut state = self.state.lock();
            if !state.streaming {
                return;
            }
            state.sequence = state.sequence.wrapping_add(1);
            let Some(index) = state.queued.pop_front() else { return };
            let Some(memory) = state.memory.clone() else { return };
            (index, memory, index as u64 * state.buffer_bytes, state.buffer_bytes as usize)
        };
        let length = data.len().min(capacity);
        // The pages exist already (made when the buffers were), so this
        // doesn't allocate.
        let _ = memory.write(offset, &data[..length]);
        let mut state = self.state.lock();
        let sequence = state.sequence.wrapping_sub(1);
        if let Some(buffer) = state.buffers.get_mut(index) {
            buffer.queued = false;
            buffer.done = true;
            buffer.bytes_used = length as u32;
            buffer.sequence = sequence;
            buffer.timestamp = time::uptime_nanos();
            state.done.push_back(index);
        }
        drop(state);
        self.events.notify();
    }

    fn check(&self) -> Result<()> {
        if self.gone.load(Ordering::Acquire) { Err(Errno::ENODEV) } else { Ok(()) }
    }
}

/// `/dev/videoN` itself; each open gets a [`VideoFile`].
struct VideoNode {
    device: Arc<Device>,
}

fn device_metadata(device: &Device) -> Metadata {
    Metadata {
        rdev: MAJOR << 8 | device.index as u64,
        gid: VIDEO_GROUP,
        ..Metadata::virtual_file(FileKind::CharDevice, device.inode, 0o660)
    }
}

impl Inode for VideoNode {
    fn metadata(&self) -> Metadata {
        device_metadata(&self.device)
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        if let Err(error) = self.device.check() {
            return Some(Err(error));
        }
        let file = VideoFile { device: self.device.clone(), id: NEXT_FILE.fetch_add(1, Ordering::Relaxed) };
        Some(Ok(Arc::new(file)))
    }
}

/// An open camera. Any open file may ask about formats; the one that
/// makes buffers owns the stream until it frees them or closes.
struct VideoFile {
    device: Arc<Device>,
    id: usize,
}

// ioctl numbers: direction (1 write, 2 read, 3 both) << 30, size << 16,
// 'V' << 8, number.
const fn ioctl(direction: u64, number: u64, size: u64) -> u64 {
    direction << 30 | size << 16 | (b'V' as u64) << 8 | number
}
const QUERYCAP: u64 = ioctl(2, 0, 104);
const ENUM_FMT: u64 = ioctl(3, 2, 64);
const G_FMT: u64 = ioctl(3, 4, 208);
const S_FMT: u64 = ioctl(3, 5, 208);
const REQBUFS: u64 = ioctl(3, 8, 20);
const QUERYBUF: u64 = ioctl(3, 9, 88);
const QBUF: u64 = ioctl(3, 15, 88);
const DQBUF: u64 = ioctl(3, 17, 88);
const STREAMON: u64 = ioctl(1, 18, 4);
const STREAMOFF: u64 = ioctl(1, 19, 4);
const G_PARM: u64 = ioctl(3, 21, 204);
const S_PARM: u64 = ioctl(3, 22, 204);
const ENUMINPUT: u64 = ioctl(3, 26, 80);
const G_CTRL: u64 = ioctl(3, 27, 8);
const S_CTRL: u64 = ioctl(3, 28, 8);
const QUERYCTRL: u64 = ioctl(3, 36, 68);
const G_INPUT: u64 = ioctl(2, 38, 4);
const S_INPUT: u64 = ioctl(3, 39, 4);
const TRY_FMT: u64 = ioctl(3, 64, 208);
const ENUM_FRAMESIZES: u64 = ioctl(3, 74, 44);
const ENUM_FRAMEINTERVALS: u64 = ioctl(3, 75, 52);
const QUERY_EXT_CTRL: u64 = ioctl(3, 103, 232);
const G_EXT_CTRLS: u64 = ioctl(3, 71, 32);
const S_EXT_CTRLS: u64 = ioctl(3, 72, 32);
const TRY_EXT_CTRLS: u64 = ioctl(3, 73, 32);
const ENUMAUDIO: u64 = ioctl(3, 65, 52);
const G_SELECTION: u64 = ioctl(3, 94, 64);

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const COLORSPACE_SRGB: u32 = 8;
/// `priv` says the format's later fields (flags, encodings) are valid.
const PIX_FMT_PRIV_MAGIC: u32 = 0xFEED_CAFE;

const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_EXT_PIX_FORMAT: u32 = 0x0020_0000;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const CAP_TIMEPERFRAME: u32 = 0x1000;
const FMT_FLAG_COMPRESSED: u32 = 0x1;
const BUF_FLAG_MAPPED: u32 = 0x1;
const BUF_FLAG_QUEUED: u32 = 0x2;
const BUF_FLAG_DONE: u32 = 0x4;
const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;
const BUF_FLAG_TSTAMP_SRC_EOF: u32 = 0;
const BUF_CAP_SUPPORTS_MMAP: u32 = 0x1;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const INPUT_TYPE_CAMERA: u32 = 2;

/// At most this many buffers per stream.
const MAX_BUFFERS: usize = 32;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_text(bytes: &mut [u8], text: &str) {
    let length = text.len().min(bytes.len() - 1);
    bytes[..length].copy_from_slice(&text.as_bytes()[..length]);
}

/// `interval` (100 ns units) as a fraction of a second, simplest first: a
/// whole number of frames a second is 1/N, as Linux's UVC driver says it.
fn interval_fraction(interval: u32) -> (u32, u32) {
    let interval = interval.max(1);
    let rate = (10_000_000 + interval / 2) / interval;
    if rate > 0 && (10_000_000u64).abs_diff(u64::from(rate) * u64::from(interval)) * 1000 < 10_000_000 {
        return (1, rate);
    }
    let divisor = gcd(interval, 10_000_000);
    (interval / divisor, 10_000_000 / divisor)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl VideoFile {
    fn formats(&self) -> &[Format] {
        self.device.camera.formats()
    }

    fn query_capabilities(&self, argument: u64) -> Result<u64> {
        let mut capability = [0u8; 104];
        put_text(&mut capability[0..16], "uvcvideo");
        put_text(&mut capability[16..48], &self.device.name);
        put_text(&mut capability[48..80], &alloc::format!("usb-eliOS-{}", self.device.index));
        // The version of Linux whose interface this follows: 6.6.0.
        put_u32(&mut capability, 80, 6 << 16 | 6 << 8);
        let device_caps = CAP_VIDEO_CAPTURE | CAP_STREAMING | CAP_EXT_PIX_FORMAT;
        put_u32(&mut capability, 84, device_caps | CAP_DEVICE_CAPS);
        put_u32(&mut capability, 88, device_caps);
        user::write_bytes(argument, &capability)?;
        Ok(0)
    }

    fn enumerate_formats(&self, argument: u64) -> Result<u64> {
        let mut description = user::read_bytes(argument, 64)?;
        if u32_at(&description, 4) != BUF_TYPE_VIDEO_CAPTURE {
            return Err(Errno::EINVAL);
        }
        let usable: Vec<&Format> = self.formats().iter().filter(|f| !f.frames.is_empty()).collect();
        let format = usable.get(u32_at(&description, 0) as usize).ok_or(Errno::EINVAL)?;
        put_u32(&mut description, 8, if format.compressed() { FMT_FLAG_COMPRESSED } else { 0 });
        description[12..44].fill(0);
        put_text(&mut description[12..44], format.description());
        put_u32(&mut description, 44, format.fourcc);
        description[48..64].fill(0);
        user::write_bytes(argument, &description)?;
        Ok(0)
    }

    /// The format and size nearest what `format` (a `v4l2_format`) asks
    /// for: its pixel format if offered (otherwise the current one), and
    /// the size closest in area.
    fn choose(&self, format: &[u8]) -> (usize, usize) {
        let formats = self.formats();
        let current = self.device.state.lock().format;
        let wanted = u32_at(format, 16);
        let chosen = formats.iter().position(|f| f.fourcc == wanted && !f.frames.is_empty()).unwrap_or(current);
        let (width, height) = (u32_at(format, 8), u32_at(format, 12));
        let area = u64::from(width) * u64::from(height);
        let size = formats[chosen]
            .frames
            .iter()
            .enumerate()
            .min_by_key(|(_, size)| {
                let exact = u32::from(size.width) == width && u32::from(size.height) == height;
                (!exact, (u64::from(size.width) * u64::from(size.height)).abs_diff(area))
            })
            .map_or(0, |(index, _)| index);
        (chosen, size)
    }

    /// Fills in a `v4l2_format` for a format and size.
    fn describe(&self, bytes: &mut [u8], format: usize, size: usize) {
        let format = &self.formats()[format];
        let frame = &format.frames[size];
        let (per_line, image) = format.layout(frame);
        bytes[8..208].fill(0);
        put_u32(bytes, 8, u32::from(frame.width));
        put_u32(bytes, 12, u32::from(frame.height));
        put_u32(bytes, 16, format.fourcc);
        put_u32(bytes, 20, FIELD_NONE);
        put_u32(bytes, 24, per_line);
        put_u32(bytes, 28, image);
        put_u32(bytes, 32, COLORSPACE_SRGB);
        put_u32(bytes, 36, PIX_FMT_PRIV_MAGIC);
    }

    fn format(&self, request: u64, argument: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 208)?;
        if u32_at(&bytes, 0) != BUF_TYPE_VIDEO_CAPTURE {
            return Err(Errno::EINVAL);
        }
        let (format, size) = match request {
            G_FMT => {
                let state = self.device.state.lock();
                (state.format, state.size)
            }
            _ => self.choose(&bytes),
        };
        if request == S_FMT {
            let mut state = self.device.state.lock();
            if state.owner.is_some_and(|owner| owner != self.id) || state.streaming {
                return Err(Errno::EBUSY);
            }
            if !state.buffers.is_empty() && (state.format, state.size) != (format, size) {
                return Err(Errno::EBUSY);
            }
            let intervals = &self.formats()[format].frames[size].intervals;
            if !intervals.contains(&state.interval) {
                state.interval = intervals.first().copied().unwrap_or(333_333);
            }
            (state.format, state.size) = (format, size);
        }
        self.describe(&mut bytes, format, size);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn enumerate_sizes(&self, argument: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 44)?;
        let format = self.formats().iter().find(|f| f.fourcc == u32_at(&bytes, 4)).ok_or(Errno::EINVAL)?;
        let size = format.frames.get(u32_at(&bytes, 0) as usize).ok_or(Errno::EINVAL)?;
        put_u32(&mut bytes, 8, FRMSIZE_TYPE_DISCRETE);
        bytes[12..44].fill(0);
        put_u32(&mut bytes, 12, u32::from(size.width));
        put_u32(&mut bytes, 16, u32::from(size.height));
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn enumerate_intervals(&self, argument: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 52)?;
        let format = self.formats().iter().find(|f| f.fourcc == u32_at(&bytes, 4)).ok_or(Errno::EINVAL)?;
        let (width, height) = (u32_at(&bytes, 8), u32_at(&bytes, 12));
        let size = format
            .frames
            .iter()
            .find(|s| u32::from(s.width) == width && u32::from(s.height) == height)
            .ok_or(Errno::EINVAL)?;
        let interval = *size.intervals.get(u32_at(&bytes, 0) as usize).ok_or(Errno::EINVAL)?;
        put_u32(&mut bytes, 16, FRMSIZE_TYPE_DISCRETE);
        bytes[20..52].fill(0);
        let (numerator, denominator) = interval_fraction(interval);
        put_u32(&mut bytes, 20, numerator);
        put_u32(&mut bytes, 24, denominator);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn parameters(&self, request: u64, argument: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 204)?;
        if u32_at(&bytes, 0) != BUF_TYPE_VIDEO_CAPTURE {
            return Err(Errno::EINVAL);
        }
        let mut state = self.device.state.lock();
        if request == S_PARM {
            if state.owner.is_some_and(|owner| owner != self.id) || state.streaming {
                return Err(Errno::EBUSY);
            }
            let (numerator, denominator) = (u64::from(u32_at(&bytes, 12)), u64::from(u32_at(&bytes, 16)));
            if numerator != 0 && denominator != 0 {
                let wanted = numerator * 10_000_000 / denominator;
                let intervals = &self.formats()[state.format].frames[state.size].intervals;
                if let Some(&nearest) = intervals.iter().min_by_key(|&&i| u64::from(i).abs_diff(wanted)) {
                    state.interval = nearest;
                }
            }
        }
        let (numerator, denominator) = interval_fraction(state.interval);
        drop(state);
        bytes[4..204].fill(0);
        put_u32(&mut bytes, 4, CAP_TIMEPERFRAME);
        put_u32(&mut bytes, 12, numerator);
        put_u32(&mut bytes, 16, denominator);
        // readbuffers: how many buffers make sense.
        put_u32(&mut bytes, 24, 4);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn request_buffers(&self, argument: u64) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 20)?;
        if u32_at(&bytes, 4) != BUF_TYPE_VIDEO_CAPTURE || u32_at(&bytes, 8) != MEMORY_MMAP {
            return Err(Errno::EINVAL);
        }
        let count = (u32_at(&bytes, 0) as usize).min(MAX_BUFFERS);
        {
            let state = self.device.state.lock();
            if state.owner.is_some_and(|owner| owner != self.id) {
                return Err(Errno::EBUSY);
            }
            if state.streaming {
                return Err(Errno::EBUSY);
            }
        }
        self.free_buffers();
        if count > 0 {
            let (format, size) = {
                let state = self.device.state.lock();
                (state.format, state.size)
            };
            let format = &self.formats()[format];
            let image = format.layout(&format.frames[size]).1.max(format.frames[size].max_bytes);
            let buffer_bytes = u64::from(image).div_ceil(PAGE_SIZE) * PAGE_SIZE;
            let memory = MemoryObject::new();
            // Make every page now: frames are copied in by the driver's
            // thread, which shouldn't allocate.
            for page in 0..buffer_bytes * count as u64 / PAGE_SIZE {
                crate::memory::frames::free(memory.page(page)?);
            }
            let _ = memory.write(buffer_bytes * count as u64 - 1, &[0]);
            let mut state = self.device.state.lock();
            state.buffers = (0..count)
                .map(|_| Buffer { queued: false, done: false, bytes_used: 0, sequence: 0, timestamp: 0 })
                .collect();
            state.buffer_bytes = buffer_bytes;
            state.memory = Some(memory);
            state.owner = Some(self.id);
        }
        put_u32(&mut bytes, 0, count as u32);
        put_u32(&mut bytes, 12, BUF_CAP_SUPPORTS_MMAP);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn free_buffers(&self) {
        let mut state = self.device.state.lock();
        state.buffers.clear();
        state.queued.clear();
        state.done.clear();
        state.memory = None;
        state.owner = None;
    }

    /// Fills in a `v4l2_buffer` for buffer `index`.
    fn describe_buffer(state: &State, index: usize, bytes: &mut [u8]) {
        let buffer = &state.buffers[index];
        let mut flags = BUF_FLAG_MAPPED | BUF_FLAG_TIMESTAMP_MONOTONIC | BUF_FLAG_TSTAMP_SRC_EOF;
        if buffer.queued {
            flags |= BUF_FLAG_QUEUED;
        }
        if buffer.done {
            flags |= BUF_FLAG_DONE;
        }
        bytes[8..88].fill(0);
        put_u32(bytes, 0, index as u32);
        put_u32(bytes, 4, BUF_TYPE_VIDEO_CAPTURE);
        put_u32(bytes, 8, buffer.bytes_used);
        put_u32(bytes, 12, flags);
        put_u32(bytes, 16, FIELD_NONE);
        let microseconds = buffer.timestamp / 1000;
        bytes[24..32].copy_from_slice(&(microseconds / 1_000_000).to_le_bytes());
        bytes[32..40].copy_from_slice(&(microseconds % 1_000_000).to_le_bytes());
        put_u32(bytes, 56, buffer.sequence);
        put_u32(bytes, 60, MEMORY_MMAP);
        put_u32(bytes, 64, (index as u64 * state.buffer_bytes) as u32);
        put_u32(bytes, 72, state.buffer_bytes as u32);
    }

    /// Reads a `v4l2_buffer` and checks it names one of our buffers.
    fn buffer_argument(&self, state: &State, argument: u64) -> Result<(Vec<u8>, usize)> {
        let bytes = user::read_bytes(argument, 88)?;
        if u32_at(&bytes, 4) != BUF_TYPE_VIDEO_CAPTURE || state.owner.is_some_and(|owner| owner != self.id) {
            return Err(Errno::EINVAL);
        }
        let index = u32_at(&bytes, 0) as usize;
        if index >= state.buffers.len() {
            return Err(Errno::EINVAL);
        }
        Ok((bytes, index))
    }

    fn query_buffer(&self, argument: u64) -> Result<u64> {
        let state = self.device.state.lock();
        let (mut bytes, index) = self.buffer_argument(&state, argument)?;
        Self::describe_buffer(&state, index, &mut bytes);
        drop(state);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn queue_buffer(&self, argument: u64) -> Result<u64> {
        let mut state = self.device.state.lock();
        let (mut bytes, index) = self.buffer_argument(&state, argument)?;
        if u32_at(&bytes, 60) != MEMORY_MMAP || state.buffers[index].queued {
            return Err(Errno::EINVAL);
        }
        if state.buffers[index].done {
            state.done.retain(|&i| i != index);
        }
        let buffer = &mut state.buffers[index];
        buffer.queued = true;
        buffer.done = false;
        buffer.bytes_used = 0;
        state.queued.push_back(index);
        Self::describe_buffer(&state, index, &mut bytes);
        drop(state);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn dequeue_buffer(&self, argument: u64, nonblocking: bool) -> Result<u64> {
        let mut bytes = user::read_bytes(argument, 88)?;
        if u32_at(&bytes, 4) != BUF_TYPE_VIDEO_CAPTURE {
            return Err(Errno::EINVAL);
        }
        let take = || -> Option<Result<usize>> {
            if let Err(error) = self.device.check() {
                return Some(Err(error));
            }
            let mut state = self.device.state.lock();
            if !state.streaming || state.owner != Some(self.id) {
                return Some(Err(Errno::EINVAL));
            }
            let index = state.done.pop_front()?;
            state.buffers[index].done = false;
            Some(Ok(index))
        };
        let index = loop {
            if let Some(result) = take() {
                break result?;
            }
            if nonblocking {
                return Err(Errno::EAGAIN);
            }
            let deadline = time::ticks() + 1000;
            let woken = self.device.events.queue().wait_until_deadline(Some(deadline), || {
                if process::signal_pending() {
                    return Some(Err(Errno::EINTR));
                }
                take().map(Ok)
            });
            match woken {
                Some(Ok(result)) => break result?,
                Some(Err(error)) => return Err(error),
                None => {}
            }
        };
        let state = self.device.state.lock();
        if index < state.buffers.len() {
            Self::describe_buffer(&state, index, &mut bytes);
            // Dequeued: neither queued nor done any more.
            let flags = u32_at(&bytes, 12) & !BUF_FLAG_DONE;
            put_u32(&mut bytes, 12, flags);
        }
        drop(state);
        user::write_bytes(argument, &bytes)?;
        Ok(0)
    }

    fn stream_on(&self, argument: u64) -> Result<u64> {
        if user::read_value::<u32>(argument)? != BUF_TYPE_VIDEO_CAPTURE {
            return Err(Errno::EINVAL);
        }
        let (format, size, interval) = {
            let mut state = self.device.state.lock();
            if state.owner != Some(self.id) || state.buffers.is_empty() {
                return Err(Errno::EINVAL);
            }
            if state.streaming {
                return Ok(0);
            }
            state.streaming = true;
            (state.format, state.size, state.interval)
        };
        if let Err(error) = self.device.camera.start(format, size, interval, self.device.clone()) {
            self.device.state.lock().streaming = false;
            return Err(error);
        }
        Ok(0)
    }

    fn stream_off(&self) {
        let was_streaming = {
            let mut state = self.device.state.lock();
            let was = state.streaming;
            state.streaming = false;
            state.queued.clear();
            state.done.clear();
            for buffer in &mut state.buffers {
                buffer.queued = false;
                buffer.done = false;
            }
            was
        };
        if was_streaming {
            self.device.camera.stop();
        }
        self.device.events.notify();
    }

    fn input(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            ENUMINPUT => {
                let mut bytes = user::read_bytes(argument, 80)?;
                if u32_at(&bytes, 0) != 0 {
                    return Err(Errno::EINVAL);
                }
                bytes[4..80].fill(0);
                put_text(&mut bytes[4..36], "Camera 1");
                put_u32(&mut bytes, 36, INPUT_TYPE_CAMERA);
                user::write_bytes(argument, &bytes)?;
            }
            G_INPUT => user::write_value(argument, &0u32)?,
            _ => {
                if user::read_value::<u32>(argument)? != 0 {
                    return Err(Errno::EINVAL);
                }
            }
        }
        Ok(0)
    }
}

impl Inode for VideoFile {
    fn metadata(&self) -> Metadata {
        device_metadata(&self.device)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn ioctl_with_flags(&self, flags: u32, request: u64, argument: u64) -> Result<u64> {
        self.device.check()?;
        match request {
            QUERYCAP => self.query_capabilities(argument),
            ENUM_FMT => self.enumerate_formats(argument),
            G_FMT | S_FMT | TRY_FMT => self.format(request, argument),
            ENUM_FRAMESIZES => self.enumerate_sizes(argument),
            ENUM_FRAMEINTERVALS => self.enumerate_intervals(argument),
            G_PARM | S_PARM => self.parameters(request, argument),
            REQBUFS => self.request_buffers(argument),
            QUERYBUF => self.query_buffer(argument),
            QBUF => self.queue_buffer(argument),
            DQBUF => self.dequeue_buffer(argument, flags & fs::O_NONBLOCK != 0),
            STREAMON => self.stream_on(argument),
            STREAMOFF => {
                if user::read_value::<u32>(argument)? != BUF_TYPE_VIDEO_CAPTURE {
                    return Err(Errno::EINVAL);
                }
                if self.device.state.lock().owner.is_some_and(|owner| owner != self.id) {
                    return Err(Errno::EBUSY);
                }
                self.stream_off();
                Ok(0)
            }
            ENUMINPUT | G_INPUT | S_INPUT => self.input(request, argument),
            // No controls (brightness and so on) yet, no audio, no cropping.
            QUERYCTRL | QUERY_EXT_CTRL | G_CTRL | S_CTRL | G_EXT_CTRLS | S_EXT_CTRLS | TRY_EXT_CTRLS | ENUMAUDIO
            | G_SELECTION => Err(Errno::EINVAL),
            _ => Err(Errno::ENOTTY),
        }
    }

    fn poll(&self, _flags: u32) -> u16 {
        if self.device.gone.load(Ordering::Acquire) {
            return POLLERR | POLLHUP;
        }
        let state = self.device.state.lock();
        if !state.streaming || state.owner != Some(self.id) {
            POLLERR
        } else if state.done.is_empty() {
            0
        } else {
            POLLIN | 0x40
        }
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.device.events.clone());
        true
    }

    fn memory_object(&self) -> Option<Arc<MemoryObject>> {
        let state = self.device.state.lock();
        if state.owner != Some(self.id) {
            return None;
        }
        state.memory.clone()
    }

    fn on_close(&self, _flags: u32) {
        let owner = self.device.state.lock().owner == Some(self.id);
        if owner {
            self.stream_off();
            self.free_buffers();
        }
    }
}

/// `/proc/video`: each camera and what it offers.
pub fn status_text() -> String {
    let mut text = String::new();
    for device in DEVICES.lock().iter() {
        text.push_str(&alloc::format!("video{}: {}\n", device.index, device.name));
        for format in device.camera.formats() {
            let fourcc = format.fourcc.to_le_bytes();
            let name = core::str::from_utf8(&fourcc).unwrap_or("????");
            text.push_str(&alloc::format!("  {name}:"));
            for size in &format.frames {
                let rates: Vec<String> =
                    size.intervals.iter().map(|&i| alloc::format!("{}", interval_fraction(i).1)).collect();
                text.push_str(&alloc::format!(" {}x{} ({} fps)", size.width, size.height, rates.join("/")));
            }
            text.push('\n');
        }
        let state = device.state.lock();
        text.push_str(&alloc::format!(
            "  streaming: {}, {} buffers, frame {}\n",
            if state.streaming { "yes" } else { "no" },
            state.buffers.len(),
            state.sequence
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn frame_intervals_read_as_frames_per_second() {
            assert_eq!(interval_fraction(333_333), (1, 30));
            assert_eq!(interval_fraction(666_666), (1, 15));
            assert_eq!(interval_fraction(400_000), (1, 25));
            assert_eq!(interval_fraction(3_000_000), (3, 10));
        }

        fn ioctl_numbers_match_linux() {
            assert_eq!(QUERYCAP, 0x8068_5600);
            assert_eq!(S_FMT, 0xC0D0_5605);
            assert_eq!(QBUF, 0xC058_560F);
            assert_eq!(DQBUF, 0xC058_5611);
            assert_eq!(STREAMON, 0x4004_5612);
            assert_eq!(ENUM_FRAMESIZES, 0xC02C_564A);
        }
    }
}
