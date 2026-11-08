//! USB video class (UVC) cameras: nearly every webcam, the T440p's
//! built-in one (04f2:b39a) included.
//!
//! A UVC camera has a control interface (which describes the camera's
//! insides and takes requests like brightness) and a streaming interface.
//! The streaming interface's descriptors list the picture formats (raw
//! YUYV, Motion-JPEG...), each with frame sizes and rates. To stream, the
//! driver "probes" (proposes a format, size and rate and reads back what
//! the camera makes of it, including how many bytes it sends per
//! microframe), "commits", and selects the streaming interface's
//! alternate setting with an isochronous endpoint big enough for that.
//! Pictures then arrive in isochronous packets, each starting with a small
//! header whose frame ID bit flips at every new frame. This module parses
//! descriptors and puts frames back together; `xhci.rs` moves the packets.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::xhci::Controller;
use crate::errno::{Errno, Result};
use crate::sync::Mutex;
use crate::video::{self, FOURCC_MJPEG, FOURCC_NV12, FOURCC_YUYV, Format, FrameSize};

const CLASS_VIDEO: u8 = 0x0E;
const SUBCLASS_CONTROL: u8 = 1;
const SUBCLASS_STREAMING: u8 = 2;
const CS_INTERFACE: u8 = 0x24;
const VC_HEADER: u8 = 0x01;
const VS_FORMAT_UNCOMPRESSED: u8 = 0x04;
const VS_FRAME_UNCOMPRESSED: u8 = 0x05;
const VS_FORMAT_MJPEG: u8 = 0x06;
const VS_FRAME_MJPEG: u8 = 0x07;
const DESCRIPTOR_INTERFACE: u8 = 4;
const DESCRIPTOR_ENDPOINT: u8 = 5;
const DESCRIPTOR_SS_COMPANION: u8 = 0x30;

/// Probe and commit requests.
const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const VS_PROBE_CONTROL: u16 = 1;
const VS_COMMIT_CONTROL: u16 = 2;

/// Payload header bits.
const HEADER_FRAME_ID: u8 = 0x01;
const HEADER_END_OF_FRAME: u8 = 0x02;
const HEADER_ERROR: u8 = 0x40;

/// An alternate setting of the streaming interface: one isochronous
/// endpoint, of some bandwidth.
#[derive(Debug, Clone, Copy)]
pub struct Alternate {
    pub setting: u8,
    pub address: u8,
    /// wMaxPacketSize as given: the size in bits 0-10, extra transactions
    /// per microframe (high speed) in bits 11-12.
    pub packet_size: u16,
    pub interval: u8,
    /// SuperSpeed: from the endpoint's companion descriptor.
    pub burst: u8,
    pub mult: u8,
    /// Bytes per service interval.
    pub bandwidth: u32,
}

/// A camera's video function, from its configuration descriptor.
#[derive(Debug, Clone)]
pub struct VideoFunction {
    pub control_interface: u8,
    pub streaming_interface: u8,
    /// bcdUVC: 1.0, 1.1 and 1.5 have different probe structure lengths.
    pub version: u16,
    pub formats: Vec<Format>,
    pub alternates: Vec<Alternate>,
}

impl VideoFunction {
    /// Finds the first camera in a configuration descriptor (its control
    /// interface, and the streaming interface after it).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let mut function = Self {
            control_interface: 0,
            streaming_interface: 0,
            version: 0x0100,
            formats: Vec::new(),
            alternates: Vec::new(),
        };
        let (mut found_control, mut found_streaming) = (false, false);
        // The interface being read: number, alternate setting, subclass;
        // None outside the camera's interfaces.
        let mut current: Option<(u8, u8, u8)> = None;
        let mut offset = 0;
        while offset + 2 <= bytes.len() {
            let length = usize::from(bytes[offset]);
            if length < 2 || offset + length > bytes.len() {
                break;
            }
            let d = &bytes[offset..offset + length];
            offset += length;
            match d[1] {
                DESCRIPTOR_INTERFACE if length >= 9 => {
                    current = None;
                    if d[5] != CLASS_VIDEO {
                        continue;
                    }
                    let (number, setting, subclass) = (d[2], d[3], d[6]);
                    if subclass == SUBCLASS_CONTROL && !found_control {
                        found_control = true;
                        function.control_interface = number;
                        current = Some((number, setting, subclass));
                    } else if subclass == SUBCLASS_STREAMING && found_control {
                        if !found_streaming {
                            found_streaming = true;
                            function.streaming_interface = number;
                        }
                        if number == function.streaming_interface {
                            current = Some((number, setting, subclass));
                        }
                    }
                }
                CS_INTERFACE if length >= 3 => match current {
                    Some((_, _, SUBCLASS_CONTROL)) if d[2] == VC_HEADER && length >= 5 => {
                        function.version = u16::from_le_bytes([d[3], d[4]]);
                    }
                    Some((_, 0, SUBCLASS_STREAMING)) => function.class_descriptor(d),
                    _ => {}
                },
                DESCRIPTOR_ENDPOINT if length >= 7 => {
                    let Some((_, setting, SUBCLASS_STREAMING)) = current else { continue };
                    // Isochronous IN endpoints only (bulk-streaming cameras
                    // exist, but are rare).
                    if setting == 0 || d[3] & 3 != 1 || d[2] & 0x80 == 0 {
                        continue;
                    }
                    let packet_size = u16::from_le_bytes([d[4], d[5]]);
                    let bandwidth = u32::from(packet_size & 0x7FF) * (u32::from(packet_size >> 11 & 3) + 1);
                    function.alternates.push(Alternate {
                        setting,
                        address: d[2],
                        packet_size,
                        interval: d[6],
                        burst: 0,
                        mult: 0,
                        bandwidth,
                    });
                }
                DESCRIPTOR_SS_COMPANION if length >= 6 => {
                    if let (Some((_, setting, SUBCLASS_STREAMING)), Some(last)) =
                        (current, function.alternates.last_mut())
                        && last.setting == setting
                    {
                        last.burst = d[2];
                        last.mult = d[3] & 3;
                        last.bandwidth = u32::from(u16::from_le_bytes([d[4], d[5]]));
                    }
                }
                _ => {}
            }
        }
        function.formats.retain(|format| !format.frames.is_empty());
        (found_streaming && !function.formats.is_empty()).then_some(function)
    }

    /// A class-specific descriptor of the streaming interface: a format,
    /// or a frame size of the format before it.
    fn class_descriptor(&mut self, d: &[u8]) {
        match d[2] {
            VS_FORMAT_UNCOMPRESSED if d.len() >= 27 => {
                // The format is a GUID whose first four bytes are a FourCC
                // (UVC's "YUY2" is V4L2's YUYV).
                let fourcc = match &d[5..9] {
                    b"YUY2" => Some(FOURCC_YUYV),
                    b"NV12" => Some(FOURCC_NV12),
                    _ => None,
                };
                match fourcc {
                    Some(fourcc) => self.formats.push(Format { index: d[3], fourcc, frames: Vec::new() }),
                    None => debug!("uvc: format {} has unsupported GUID {:02x?}", d[3], &d[5..21]),
                }
            }
            VS_FORMAT_MJPEG if d.len() >= 11 => {
                self.formats.push(Format { index: d[3], fourcc: FOURCC_MJPEG, frames: Vec::new() });
            }
            VS_FRAME_UNCOMPRESSED | VS_FRAME_MJPEG if d.len() >= 26 => {
                let Some(format) = self.formats.last_mut() else { return };
                let uncompressed = format.fourcc != FOURCC_MJPEG;
                if uncompressed != (d[2] == VS_FRAME_UNCOMPRESSED) {
                    return;
                }
                let read = |at: usize| d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                let kind = d[25];
                let mut intervals: Vec<u32> = if kind == 0 {
                    // Continuous: the fastest and slowest.
                    [read(26), read(30)].into_iter().flatten().collect()
                } else {
                    (0..usize::from(kind)).filter_map(|i| read(26 + i * 4)).collect()
                };
                intervals.retain(|&interval| interval > 0);
                intervals.sort_unstable();
                intervals.dedup();
                if intervals.is_empty() {
                    intervals.push(read(21).unwrap_or(333_333).max(1));
                }
                format.frames.push(FrameSize {
                    index: d[3],
                    width: u16::from_le_bytes([d[5], d[6]]),
                    height: u16::from_le_bytes([d[7], d[8]]),
                    intervals,
                    max_bytes: read(17).unwrap_or(0),
                });
            }
            _ => {}
        }
    }

    /// The probe and commit structure's length for this UVC version.
    pub fn probe_length(&self) -> usize {
        match self.version {
            0x0150.. => 48,
            0x0110.. => 34,
            _ => 26,
        }
    }

    /// The probe structure asking for a format and frame (UVC's numbers)
    /// at an interval.
    pub fn probe(&self, format: u8, frame: u8, interval: u32) -> Vec<u8> {
        let mut probe = alloc::vec![0u8; self.probe_length()];
        // bmHint: keep the frame interval.
        probe[0] = 1;
        probe[2] = format;
        probe[3] = frame;
        probe[4..8].copy_from_slice(&interval.to_le_bytes());
        probe
    }

    /// The setting with the least bandwidth that carries `payload` bytes
    /// per interval (or the biggest, if none does).
    pub fn alternate_for(&self, payload: u32) -> Option<Alternate> {
        let enough = self.alternates.iter().filter(|a| a.bandwidth >= payload).min_by_key(|a| a.bandwidth);
        enough.or_else(|| self.alternates.iter().max_by_key(|a| a.bandwidth)).copied()
    }
}

/// UVC requests to the streaming interface.
pub fn set_cur(interface: u8, selector: u16, length: u16) -> super::SetupPacket {
    super::SetupPacket {
        request_type: 0x21,
        request: SET_CUR,
        value: selector << 8,
        index: u16::from(interface),
        length,
    }
}

pub fn get_cur(interface: u8, selector: u16, length: u16) -> super::SetupPacket {
    super::SetupPacket {
        request_type: 0xA1,
        request: GET_CUR,
        value: selector << 8,
        index: u16::from(interface),
        length,
    }
}

pub const PROBE: u16 = VS_PROBE_CONTROL;
pub const COMMIT: u16 = VS_COMMIT_CONTROL;

/// SET_INTERFACE: selects an interface's alternate setting.
pub fn set_interface(interface: u8, setting: u8) -> super::SetupPacket {
    super::SetupPacket {
        request_type: 0x01,
        request: 11,
        value: u16::from(setting),
        index: u16::from(interface),
        length: 0,
    }
}

/// Puts frames together from payloads.
pub struct Assembler {
    frame: Vec<u8>,
    frame_id: Option<bool>,
    error: bool,
    /// Uncompressed frames must be exactly this big; others are damaged.
    expected: Option<usize>,
    /// Frames bigger than this are damaged too.
    limit: usize,
    pub frames: u64,
    pub dropped: u64,
}

impl Assembler {
    pub fn new(expected: Option<usize>, limit: usize) -> Self {
        Self { frame: Vec::with_capacity(limit), frame_id: None, error: false, expected, limit, frames: 0, dropped: 0 }
    }

    /// One isochronous packet's contents.
    pub fn payload(&mut self, data: &[u8], deliver: &mut dyn FnMut(&[u8])) {
        if data.len() < 2 {
            return;
        }
        let header = usize::from(data[0]);
        let flags = data[1];
        if header < 2 || header > data.len() {
            return;
        }
        let frame_id = flags & HEADER_FRAME_ID != 0;
        if self.frame_id.is_some_and(|id| id != frame_id) && !self.frame.is_empty() {
            // A new frame began without the last one ending.
            self.finish(deliver);
        }
        self.frame_id = Some(frame_id);
        if flags & HEADER_ERROR != 0 {
            self.error = true;
        }
        let body = &data[header..];
        if self.frame.len() + body.len() > self.limit {
            self.error = true;
        } else {
            self.frame.extend_from_slice(body);
        }
        if flags & HEADER_END_OF_FRAME != 0 {
            self.finish(deliver);
        }
    }

    fn finish(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        let damaged = self.error || self.expected.is_some_and(|expected| expected != self.frame.len());
        if self.frame.is_empty() {
        } else if damaged {
            self.dropped += 1;
            if self.dropped < 5 || self.dropped.is_power_of_two() {
                debug!(
                    "uvc: dropped a frame of {} bytes (error {}), {} so far",
                    self.frame.len(),
                    self.error,
                    self.dropped
                );
            }
        } else {
            self.frames += 1;
            deliver(&self.frame);
        }
        self.frame.clear();
        self.error = false;
    }
}

/// A UVC camera, as `video` sees it.
pub struct UvcCamera {
    pub controller: Arc<Mutex<Controller>>,
    pub slot: u8,
    pub device: u64,
    pub function: VideoFunction,
}

impl video::Camera for UvcCamera {
    fn formats(&self) -> &[Format] {
        &self.function.formats
    }

    fn start(&self, format: usize, size: usize, interval: u32, sink: Arc<video::Device>) -> Result<()> {
        let mut controller = self.controller.lock();
        controller.start_video(self, format, size, interval, sink).map_err(|error| {
            warn!("uvc: can't start the stream: {error}");
            if error == "no such device" { Errno::ENODEV } else { Errno::EIO }
        })
    }

    fn stop(&self) {
        self.controller.lock().stop_video(self.slot, self.device);
    }
}

/// What `/proc/video` and the log say about a camera's function.
pub fn describe(function: &VideoFunction) -> String {
    let alternates: Vec<String> = function
        .alternates
        .iter()
        .map(|a| alloc::format!("{}:{:#06x}/{}B", a.setting, a.packet_size, a.bandwidth))
        .collect();
    alloc::format!(
        "UVC {:x}.{:02x}, interfaces {}/{}, {} formats, alternates {}",
        function.version >> 8,
        function.version & 0xFF,
        function.control_interface,
        function.streaming_interface,
        function.formats.len(),
        alternates.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small camera: one YUY2 format with two sizes, alternates of 1024
    /// and 3072 bytes.
    fn descriptors() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend([9, 2, 0, 0, 2, 1, 0, 0x80, 50]);
        bytes.extend([9, 4, 0, 0, 0, CLASS_VIDEO, SUBCLASS_CONTROL, 0, 0]);
        bytes.extend([13, CS_INTERFACE, VC_HEADER, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 1, 1]);
        bytes.extend([9, 4, 1, 0, 0, CLASS_VIDEO, SUBCLASS_STREAMING, 0, 0]);
        let mut format = alloc::vec![27, CS_INTERFACE, VS_FORMAT_UNCOMPRESSED, 1, 2];
        format.extend(b"YUY2\x00\x00\x10\x00\x80\x00\x00\xAA\x00\x38\x9B\x71");
        format.extend([16, 1, 0, 0, 0, 0]);
        bytes.extend(format);
        for (index, (width, height)) in [(640u16, 480u16), (320, 240)].into_iter().enumerate() {
            let mut frame = alloc::vec![34, CS_INTERFACE, VS_FRAME_UNCOMPRESSED, index as u8 + 1, 0];
            frame.extend(width.to_le_bytes());
            frame.extend(height.to_le_bytes());
            frame.extend([0; 8]);
            frame.extend((u32::from(width) * u32::from(height) * 2).to_le_bytes());
            frame.extend(333_333u32.to_le_bytes());
            frame.push(2);
            frame.extend(666_666u32.to_le_bytes());
            frame.extend(333_333u32.to_le_bytes());
            bytes.extend(frame);
        }
        for (setting, size) in [(1u8, 1024u16), (2, 1024 | 2 << 11)] {
            bytes.extend([9, 4, 1, setting, 1, CLASS_VIDEO, SUBCLASS_STREAMING, 0, 0]);
            bytes.extend([7, 5, 0x81, 5, size as u8, (size >> 8) as u8, 1]);
        }
        bytes
    }

    crate::ktest! {
        fn uvc_descriptors_give_formats_and_alternates() {
            let function = VideoFunction::parse(&descriptors()).unwrap();
            assert_eq!((function.control_interface, function.streaming_interface), (0, 1));
            assert_eq!(function.probe_length(), 26);
            assert_eq!(function.formats.len(), 1);
            let format = &function.formats[0];
            assert_eq!(format.fourcc, FOURCC_YUYV);
            assert_eq!(format.frames.len(), 2);
            assert_eq!((format.frames[1].width, format.frames[1].height), (320, 240));
            assert_eq!(format.frames[0].intervals, [333_333, 666_666]);
            assert_eq!(function.alternates.len(), 2);
            assert_eq!(function.alternate_for(2000).unwrap().setting, 2);
            assert_eq!(function.alternate_for(1000).unwrap().setting, 1);
            assert_eq!(function.alternate_for(9000).unwrap().setting, 2);
        }

        fn uvc_payloads_make_frames() {
            let mut assembler = Assembler::new(Some(6), 64);
            let mut frames: Vec<Vec<u8>> = Vec::new();
            let mut deliver = |frame: &[u8]| frames.push(frame.to_vec());
            assembler.payload(&[2, 0, 1, 2, 3], &mut deliver);
            assembler.payload(&[2, 0], &mut deliver);
            assembler.payload(&[2, HEADER_END_OF_FRAME, 4, 5, 6], &mut deliver);
            // The next frame ends by its frame ID flipping, not EOF.
            assembler.payload(&[2, 1, 7, 8, 9, 10, 11, 12], &mut deliver);
            // One marked as an error, then a short one: both dropped.
            assembler.payload(&[2, 0, 1, 2, 3], &mut deliver);
            assembler.payload(&[2, HEADER_ERROR | HEADER_END_OF_FRAME, 4, 5, 6], &mut deliver);
            assembler.payload(&[2, 1 | HEADER_END_OF_FRAME, 1, 2, 3], &mut deliver);
            assert_eq!(frames, [alloc::vec![1, 2, 3, 4, 5, 6], alloc::vec![7, 8, 9, 10, 11, 12]]);
            assert_eq!(assembler.dropped, 2);
        }
    }
}
