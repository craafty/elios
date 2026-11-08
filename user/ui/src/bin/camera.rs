//! camera: shows what the webcam sees, as a mirror does, to check the
//! camera works (and how it looks) before a call.
//!
//! It uses the camera through V4L2 as any Linux program would: picks raw
//! YUYV at the size nearest 640x480, maps four buffers, and converts each
//! frame to RGB, scaled to the window. If there's no camera, or it goes
//! away, it says so and looks again every second.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eli::sys::call;
use ui::sys;
use ui::widgets::theme;
use ui::{Event, Font, Key, Rect, Window};

eli::main!(main);

const fn ioctl(direction: usize, number: usize, size: usize) -> usize {
    direction << 30 | size << 16 | (b'V' as usize) << 8 | number
}
const QUERYCAP: usize = ioctl(2, 0, 104);
const S_FMT: usize = ioctl(3, 5, 208);
const REQBUFS: usize = ioctl(3, 8, 20);
const QUERYBUF: usize = ioctl(3, 9, 88);
const QBUF: usize = ioctl(3, 15, 88);
const DQBUF: usize = ioctl(3, 17, 88);
const STREAMON: usize = ioctl(1, 18, 4);
const ENUM_FRAMESIZES: usize = ioctl(3, 74, 44);

const CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const YUYV: u32 = u32::from_le_bytes(*b"YUYV");
const MMAP: usize = 9;
const BUFFERS: u32 = 4;

fn get(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn request(fd: i32, number: usize, bytes: &mut [u8], what: &str) -> Result<(), String> {
    // SAFETY: `bytes` is the size the request's number says.
    unsafe { sys::ioctl(fd, number, bytes.as_mut_ptr() as usize) }.map(|_| ()).map_err(|e| format!("{what}: {e}"))
}

/// A streaming camera.
struct Camera {
    fd: i32,
    name: String,
    width: usize,
    height: usize,
    buffers: Vec<*const u8>,
}

impl Camera {
    fn open() -> Result<Self, String> {
        let fd = sys::open("/dev/video0", sys::O_RDWR | sys::O_NONBLOCK).map_err(|e| format!("No camera ({e})"))?;
        match Self::start(fd) {
            Ok(camera) => Ok(camera),
            Err(error) => {
                let _ = eli::io::close(fd);
                Err(error)
            }
        }
    }

    fn start(fd: i32) -> Result<Self, String> {
        let mut capability = [0u8; 104];
        request(fd, QUERYCAP, &mut capability, "QUERYCAP")?;
        let name = &capability[16..48];
        let name = String::from_utf8_lossy(&name[..name.iter().position(|&b| b == 0).unwrap_or(32)]).into_owned();

        // The YUYV size nearest 640x480 (big enough to see, small enough
        // for USB 2's bandwidth at full rate).
        let mut best: Option<(u32, u32)> = None;
        for index in 0..64 {
            let mut size = [0u8; 44];
            put(&mut size, 0, index);
            put(&mut size, 4, YUYV);
            if request(fd, ENUM_FRAMESIZES, &mut size, "").is_err() {
                break;
            }
            let (width, height) = (get(&size, 12), get(&size, 16));
            let distance = |(w, h): (u32, u32)| (w * h).abs_diff(640 * 480);
            if best.is_none_or(|best| distance((width, height)) < distance(best)) {
                best = Some((width, height));
            }
        }
        let (width, height) = best.ok_or_else(|| format!("{name} has no raw (YUYV) pictures"))?;

        let mut format = [0u8; 208];
        put(&mut format, 0, CAPTURE);
        put(&mut format, 8, width);
        put(&mut format, 12, height);
        put(&mut format, 16, YUYV);
        request(fd, S_FMT, &mut format, "S_FMT")?;
        let (width, height) = (get(&format, 8) as usize, get(&format, 12) as usize);

        let mut buffers = [0u8; 20];
        put(&mut buffers, 0, BUFFERS);
        put(&mut buffers, 4, CAPTURE);
        put(&mut buffers, 8, MEMORY_MMAP);
        request(fd, REQBUFS, &mut buffers, "REQBUFS")?;
        let mut mapped = Vec::new();
        for index in 0..get(&buffers, 0) {
            let mut buffer = [0u8; 88];
            put(&mut buffer, 0, index);
            put(&mut buffer, 4, CAPTURE);
            put(&mut buffer, 60, MEMORY_MMAP);
            request(fd, QUERYBUF, &mut buffer, "QUERYBUF")?;
            let (offset, length) = (get(&buffer, 64) as usize, get(&buffer, 72) as usize);
            if length < width * height * 2 {
                return Err(String::from("the camera's buffers are too small"));
            }
            // SAFETY: a fresh mapping, placed by the kernel.
            let address = unsafe { call(MMAP, [0, length, sys::PROT_READ, sys::MAP_SHARED, fd as usize, offset]) }
                .map_err(|e| format!("mmap: {e}"))?;
            mapped.push(address as *const u8);
            request(fd, QBUF, &mut buffer, "QBUF")?;
        }
        let mut kind = CAPTURE.to_le_bytes();
        request(fd, STREAMON, &mut kind, "STREAMON")?;
        Ok(Self { fd, name, width, height, buffers: mapped })
    }

    /// Takes the next frame, if one is ready, converting it into `rgb`.
    /// `Err` if the camera's gone.
    fn take(&self, rgb: &mut [u32]) -> Result<bool, String> {
        let mut buffer = [0u8; 88];
        put(&mut buffer, 4, CAPTURE);
        put(&mut buffer, 60, MEMORY_MMAP);
        // SAFETY: the buffer is a v4l2_buffer's size.
        match unsafe { sys::ioctl(self.fd, DQBUF, buffer.as_mut_ptr() as usize) } {
            Ok(_) => {}
            Err(eli::sys::Error::AGAIN) => return Ok(false),
            Err(error) => return Err(format!("The camera stopped ({error})")),
        }
        let index = get(&buffer, 0) as usize;
        if let Some(&frame) = self.buffers.get(index)
            && get(&buffer, 8) as usize >= self.width * self.height * 2
        {
            // SAFETY: the kernel mapped at least this much (checked when
            // mapping), and won't write it until it's queued again.
            let frame = unsafe { core::slice::from_raw_parts(frame, self.width * self.height * 2) };
            yuyv_to_rgb(frame, rgb);
        }
        request(self.fd, QBUF, &mut buffer, "QBUF")?;
        Ok(true)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // Closing stops the stream and frees the buffers (their mappings
        // go when the program does).
        let _ = eli::io::close(self.fd);
    }
}

fn clamp(value: i32) -> u32 {
    value.clamp(0, 255) as u32
}

/// YUYV (BT.601, limited range: what webcams send) to XRGB.
fn yuyv_to_rgb(frame: &[u8], rgb: &mut [u32]) {
    for (pair, out) in frame.as_chunks::<4>().0.iter().zip(rgb.as_chunks_mut::<2>().0) {
        let (u, v) = (i32::from(pair[1]) - 128, i32::from(pair[3]) - 128);
        for (luma, pixel) in [pair[0], pair[2]].into_iter().zip(out.iter_mut()) {
            let c = 298 * (i32::from(luma) - 16) + 128;
            let (r, g, b) = (clamp((c + 409 * v) >> 8), clamp((c - 100 * u - 208 * v) >> 8), clamp((c + 516 * u) >> 8));
            *pixel = 0xFF00_0000 | r << 16 | g << 8 | b;
        }
    }
}

struct Viewer {
    font: Font,
    camera: Result<Camera, String>,
    picture: Vec<u32>,
    /// Frames this second, and last second's count.
    frames: u32,
    rate: u32,
    second: u64,
    mirrored: bool,
}

impl Viewer {
    fn draw(&mut self, window: &mut Window) {
        let (width, height) = (window.width, window.height);
        let bar = self.font.line_height + 12;
        let mut canvas = window.canvas();
        canvas.fill(Rect::new(0, 0, width, height), 0xFF00_0000);
        let area = Rect::new(0, 0, width, height - bar);
        match &self.camera {
            Ok(camera) if !self.picture.is_empty() => {
                // Scaled to fit, keeping its shape (nearest pixel: fast,
                // and fine at these sizes).
                let (source_width, source_height) = (camera.width as i64, camera.height as i64);
                let scale_w = i64::from(area.width) * source_height;
                let scale_h = i64::from(area.height) * source_width;
                let (w, h) = if scale_w < scale_h {
                    (i64::from(area.width), i64::from(area.width) * source_height / source_width)
                } else {
                    (i64::from(area.height) * source_width / source_height, i64::from(area.height))
                };
                let (left, top) = ((i64::from(area.width) - w) / 2, (i64::from(area.height) - h) / 2);
                let stride = width as usize;
                let pixels = canvas.pixels();
                for y in 0..h {
                    let source_row = (y * source_height / h) as usize * camera.width;
                    let row = (top + y) as usize * stride + left as usize;
                    for x in 0..w {
                        let sx = (x * source_width / w) as usize;
                        let sx = if self.mirrored { camera.width - 1 - sx } else { sx };
                        pixels[row + x as usize] = self.picture[source_row + sx];
                    }
                }
            }
            Ok(_) => {
                canvas.text(&mut self.font, 20, area.height / 2, "Waiting for the first picture...", theme::DIM_TEXT);
            }
            Err(message) => {
                canvas.text(&mut self.font, 20, area.height / 2, message, theme::TEXT);
            }
        }
        canvas.fill(Rect::new(0, height - bar, width, bar), theme::PANEL);
        let status = match &self.camera {
            Ok(camera) => format!(
                "{}  ·  {}x{}  ·  {} fps  ·  M: {}",
                camera.name,
                camera.width,
                camera.height,
                self.rate,
                if self.mirrored { "mirrored" } else { "as others see it" }
            ),
            Err(_) => String::from("Looking for a camera..."),
        };
        canvas.text(&mut self.font, 10, height - bar + 6, &status, theme::DIM_TEXT);
        window.present_all();
    }

    /// Reads every frame that's ready; whether there was one.
    fn read_frames(&mut self) -> bool {
        let Ok(camera) = &self.camera else { return false };
        let size = camera.width * camera.height;
        if self.picture.len() != size {
            self.picture = vec![0; size];
        }
        let mut got = false;
        loop {
            match camera.take(&mut self.picture) {
                Ok(true) => {
                    got = true;
                    self.frames += 1;
                }
                Ok(false) => break,
                Err(error) => {
                    self.camera = Err(error);
                    self.picture.clear();
                    return true;
                }
            }
        }
        got
    }
}

fn main() -> i32 {
    let font = match Font::load(15.0) {
        Ok(font) => font,
        Err(error) => {
            eli::eprintln!("camera: {error}");
            return 1;
        }
    };
    let mut window = match Window::open("Camera", "camera", 680, 540) {
        Ok(window) => window,
        Err(error) => {
            eli::eprintln!("camera: {error}");
            return 1;
        }
    };
    let mut viewer = Viewer {
        font,
        camera: Camera::open(),
        picture: Vec::new(),
        frames: 0,
        rate: 0,
        second: eli::time::monotonic_nanos() / 1_000_000_000,
        mirrored: true,
    };
    let mut dirty = true;
    loop {
        let watch: Vec<i32> = viewer.camera.as_ref().map(|camera| camera.fd).into_iter().collect();
        let timeout = if viewer.camera.is_ok() { 500 } else { 1000 };
        let events = window.wait(&watch, timeout);
        for event in &events {
            match event {
                Event::Close => return 0,
                Event::Key { key: Key::Char('m'), .. } => viewer.mirrored = !viewer.mirrored,
                Event::Key { key: Key::Escape, .. } | Event::Key { key: Key::Char('q'), .. } => return 0,
                _ => {}
            }
            if !matches!(event, Event::Readable(_)) {
                dirty = true;
            }
        }
        if viewer.read_frames() {
            dirty = true;
        }
        let second = eli::time::monotonic_nanos() / 1_000_000_000;
        if second != viewer.second {
            viewer.second = second;
            viewer.rate = viewer.frames;
            viewer.frames = 0;
            dirty = true;
            if viewer.camera.is_err() {
                viewer.camera = Camera::open();
            }
        }
        if dirty && window.ready() {
            viewer.draw(&mut window);
            dirty = false;
        }
    }
}
