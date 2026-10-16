//! Intel High Definition Audio: the sound hardware in PCs since 2004 (the
//! ThinkPad T440p's Lynx Point chipset has it, and QEMU emulates it).
//!
//! The *controller* is a PCI device that moves sample data between memory
//! and the HD Audio link with DMA, and passes commands to the *codecs* on
//! the link (see [`codec`]). Commands go out through a ring in memory
//! (the CORB) and responses come back through another (the RIRB). Sound
//! itself flows in *streams*: each stream descriptor walks a list of
//! buffers (the BDL) in a loop, raising an interrupt at the end of each one,
//! and is tagged with a stream number that the codec's converters listen
//! for.
//!
//! eliOS uses one output stream and one input stream per controller, both
//! 16-bit stereo, and exposes them as ALSA devices (see `crate::sound`).

pub mod codec;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use self::codec::{Codec, Link, Verb};
use super::dma::DmaBuffer;
use super::pci;
use crate::arch::{apic, interrupts};
use crate::errno::{Errno, Result};
use crate::fs::PollSource;
use crate::memory::{self, CacheType};
use crate::sound::{self, Direction, Hardware, Mixer};
use crate::sync::{Mutex, SpinLock};
use crate::time;

// Controller registers.
const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const INTSTS: usize = 0x24;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5A;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;
/// Stream descriptors: 0x20 bytes each, input ones first.
const STREAMS: usize = 0x80;

// Stream descriptor registers (relative).
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const SD_RESET: u32 = 1 << 0;
const SD_RUN: u32 = 1 << 1;
const SD_INTERRUPT_ON_COMPLETION: u32 = 1 << 2;

/// The interrupt the controllers signal (MSI).
const HDA_VECTOR: u8 = interrupts::IRQ_BASE + 0x22;

/// 256 entries: four bytes each in the CORB, eight in the RIRB.
const RING_ENTRIES: usize = 256;
const RIRB_OFFSET: usize = 2048;

/// The sample rates the streams offer (the codec's converters all do these).
const RATES: [u32; 2] = [44_100, 48_000];

/// Stream tags: what codec converters listen for on the link.
const OUTPUT_TAG: u8 = 1;
const INPUT_TAG: u8 = 2;

struct Registers(u64);

impl Registers {
    fn read8(&self, offset: usize) -> u8 {
        // SAFETY: within the controller's mapped register space.
        unsafe { ((self.0 as usize + offset) as *const u8).read_volatile() }
    }
    fn read16(&self, offset: usize) -> u16 {
        // SAFETY: as above.
        unsafe { ((self.0 as usize + offset) as *const u16).read_volatile() }
    }
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: as above.
        unsafe { ((self.0 as usize + offset) as *const u32).read_volatile() }
    }
    fn write8(&self, offset: usize, value: u8) {
        // SAFETY: as above.
        unsafe { ((self.0 as usize + offset) as *mut u8).write_volatile(value) }
    }
    fn write16(&self, offset: usize, value: u16) {
        // SAFETY: as above.
        unsafe { ((self.0 as usize + offset) as *mut u16).write_volatile(value) }
    }
    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as above.
        unsafe { ((self.0 as usize + offset) as *mut u32).write_volatile(value) }
    }

    /// Waits up to `ms` milliseconds for `done`.
    fn wait(&self, ms: u64, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = time::uptime_nanos() + ms * 1_000_000;
        while time::uptime_nanos() < deadline {
            if done(self) {
                return true;
            }
            core::hint::spin_loop();
        }
        done(self)
    }
}

/// The command rings and the codec they talk to.
struct Rings {
    memory: DmaBuffer,
    /// The last CORB entry written, and the last RIRB entry read.
    written: usize,
    read: usize,
    codec: u8,
}

/// The controller's command interface, for one codec.
struct Commands<'a> {
    registers: &'a Registers,
    rings: &'a mut Rings,
}

impl Link for Commands<'_> {
    fn send(&mut self, nid: u16, verb: Verb) -> u32 {
        let command = u32::from(self.rings.codec) << 28
            | u32::from(nid & 0x7F) << 20
            | match verb {
                Verb::Short(verb, payload) => u32::from(verb) << 8 | u32::from(payload),
                Verb::Long(verb, payload) => u32::from(verb) << 16 | u32::from(payload),
            };
        let rings = &mut *self.rings;
        rings.written = (rings.written + 1) % RING_ENTRIES;
        rings.memory.write32(rings.written * 4, command);
        self.registers.write16(CORBWP, rings.written as u16);
        // The response lands in the RIRB; skip unsolicited ones (jack
        // events), which we don't ask for anyway.
        let mut response = 0;
        let answered = self.registers.wait(10, |registers| {
            while (registers.read16(RIRBWP) as usize & 0xFF) != rings.read {
                rings.read = (rings.read + 1) % RING_ENTRIES;
                let entry = RIRB_OFFSET + rings.read * 8;
                let extended = rings.memory.read32(entry + 4);
                if extended & (1 << 4) == 0 {
                    response = rings.memory.read32(entry);
                    return true;
                }
            }
            false
        });
        registers_ack(self.registers);
        if !answered {
            debug!("hda: codec {} didn't answer {command:#010x}", rings.codec);
        }
        response
    }
}

fn registers_ack(registers: &Registers) {
    // Clear "response written" and overrun flags.
    registers.write8(RIRBSTS, 0x05);
}

/// One DMA stream: its descriptor, and the buffers it loops over.
struct Stream {
    direction: Direction,
    /// Which descriptor (index into the stream registers).
    index: usize,
    tag: u8,
    state: SpinLock<StreamState>,
    /// Notified at every period boundary (and when stopped).
    events: Arc<PollSource>,
    running: AtomicBool,
}

/// What a stream descriptor is programmed with.
#[derive(Clone, Copy)]
struct Layout {
    /// The buffer descriptor list's address.
    list: u64,
    bytes: usize,
    periods: usize,
    format: u16,
}

impl StreamState {
    fn layout(&self) -> Option<Layout> {
        let list = self.list.as_ref()?.phys;
        Some(Layout { list, bytes: self.bytes, periods: self.periods, format: self.format })
    }
}

#[derive(Default)]
struct StreamState {
    buffer: Option<DmaBuffer>,
    list: Option<DmaBuffer>,
    bytes: usize,
    periods: usize,
    format: u16,
}

pub struct Controller {
    registers: Registers,
    rings: Mutex<Rings>,
    codec: Codec,
    output: Stream,
    input: Option<Stream>,
    /// Headphones were plugged in at the last check.
    headphones: AtomicBool,
    name: &'static str,
}

/// Every controller found, for the interrupt handler.
static CONTROLLERS: SpinLock<Vec<Arc<Controller>>> = SpinLock::new(Vec::new());

/// Starts the driver for each HD Audio controller, and registers a sound
/// card for each that has a codec with somewhere to play sound. Cards with
/// speakers or headphones come first, so card 0 is the laptop's own sound
/// rather than its HDMI output.
pub fn probe(devices: &[pci::Device]) {
    let mut cards: Vec<Arc<Controller>> = Vec::new();
    for device in devices.iter().filter(|d| d.class == 0x04 && d.subclass == 0x03) {
        match Controller::new(device) {
            Ok(controller) => cards.push(Arc::new(controller)),
            Err(error) => info!("hda: {:04x}:{:04x}: {error}", device.vendor, device.device),
        }
    }
    let analog = |c: &Arc<Controller>| c.codec.outputs.iter().any(|p| p.device != codec::Device::LineOut);
    cards.sort_by_key(|c| !analog(c));
    if !cards.is_empty() {
        interrupts::set_handler(HDA_VECTOR, |_| interrupt());
    }
    for controller in cards {
        CONTROLLERS.lock().push(controller.clone());
        controller.enable_interrupts();
        let playback: Arc<dyn Hardware> = Arc::new(StreamHandle { controller: controller.clone(), output: true });
        let capture: Option<Arc<dyn Hardware>> = controller
            .input
            .is_some()
            .then(|| Arc::new(StreamHandle { controller: controller.clone(), output: false }) as Arc<dyn Hardware>);
        let name = controller.name;
        sound::register_card(name, playback, capture, controller.clone() as Arc<dyn Mixer>);
        let watcher = controller.clone();
        crate::task::spawn("hda-jacks", move || watcher.watch_jacks());
    }
}

impl Controller {
    fn new(device: &pci::Device) -> core::result::Result<Self, &'static str> {
        let base = device.memory_bar(0).ok_or("no registers")?;
        device.enable_bus_mastering();
        if device.vendor == 0x8086 {
            // Intel: traffic class 0 (TCSEL) and snooped DMA (DEVC's
            // "no snoop" off), so the CPU's caches stay coherent with what
            // the controller reads and writes, as Linux does.
            let tcsel = device.read(0x44);
            device.write(0x44, tcsel & !0x7);
            let devc = device.read(0x78);
            device.write(0x78, devc & !(1 << 11));
        }
        let registers = Registers(memory::map_device(base, 0x4000, CacheType::Uncached) as u64);
        let capabilities = registers.read16(GCAP);
        let inputs = usize::from(capabilities >> 8 & 0xF);
        let outputs = usize::from(capabilities >> 12 & 0xF);
        if outputs == 0 {
            return Err("no output streams");
        }

        // Reset the controller (and so the link and codecs).
        registers.write32(GCTL, registers.read32(GCTL) & !1);
        if !registers.wait(100, |r| r.read32(GCTL) & 1 == 0) {
            return Err("didn't enter reset");
        }
        time::delay_us(100);
        registers.write32(GCTL, registers.read32(GCTL) | 1);
        if !registers.wait(100, |r| r.read32(GCTL) & 1 == 1) {
            return Err("didn't leave reset");
        }
        // Codecs announce themselves within 521 µs of the reset ending.
        time::delay_us(1000);
        let present = registers.read16(STATESTS);
        registers.write16(STATESTS, present);
        if present & 0x7FFF == 0 {
            return Err("no codecs");
        }

        let memory = DmaBuffer::new(1, 1).ok_or("out of memory")?;
        let mut rings = Rings { memory, written: 0, read: 0, codec: 0 };
        start_rings(&registers, &rings)?;

        // The first codec with an audio function.
        let mut found = None;
        for address in (0..15).filter(|a| present & (1 << a) != 0) {
            rings.codec = address as u8;
            let mut link = Commands { registers: &registers, rings: &mut rings };
            if let Some(codec) = Codec::discover(&mut link, address as u8) {
                found = Some(codec);
                break;
            }
        }
        let codec = found.ok_or("no audio codec")?;
        if codec.outputs.is_empty() {
            return Err("the codec has no usable outputs");
        }
        let analog = codec.outputs.iter().any(|p| p.device != codec::Device::LineOut);
        info!("hda: {:04x}:{:04x}: codec {} ({})", device.vendor, device.device, codec.address, codec.describe());
        let has_input = codec.input.is_some() && inputs > 0;
        let new_stream = |direction, index, tag| Stream {
            direction,
            index,
            tag,
            state: SpinLock::new(StreamState::default()),
            events: Arc::new(PollSource::new()),
            running: AtomicBool::new(false),
        };
        let controller = Controller {
            registers,
            rings: Mutex::new(rings),
            output: new_stream(Direction::Playback, inputs, OUTPUT_TAG),
            input: has_input.then(|| new_stream(Direction::Capture, 0, INPUT_TAG)),
            codec,
            headphones: AtomicBool::new(false),
            name: if analog { "HDA Intel PCH" } else { "HDA Intel HDMI" },
        };
        if device.enable_msi(apic::local_id(), HDA_VECTOR) {
            // Interrupts are turned on once the card is registered.
        } else {
            info!("hda: no MSI; positions are polled");
        }
        Ok(controller)
    }

    fn enable_interrupts(&self) {
        let mut mask = 1 << 31 | 1 << self.output.index;
        if let Some(input) = &self.input {
            mask |= 1 << input.index;
        }
        self.registers.write32(INTCTL, mask);
    }

    fn command<R>(&self, f: impl FnOnce(&Codec, &mut Commands) -> R) -> R {
        let mut rings = self.rings.lock();
        let mut link = Commands { registers: &self.registers, rings: &mut rings };
        f(&self.codec, &mut link)
    }

    fn stream(&self, output: bool) -> Option<&Stream> {
        if output { Some(&self.output) } else { self.input.as_ref() }
    }

    fn descriptor(&self, stream: &Stream) -> usize {
        STREAMS + stream.index * 0x20
    }

    /// Stops a stream's DMA and resets its descriptor.
    fn reset_stream(&self, stream: &Stream) {
        let base = self.descriptor(stream);
        let r = &self.registers;
        r.write32(base + SD_CTL, r.read32(base + SD_CTL) & !(SD_RUN | SD_INTERRUPT_ON_COMPLETION));
        r.wait(10, |r| r.read32(base + SD_CTL) & SD_RUN == 0);
        r.write8(base + SD_STS, 0x1C);
        r.write32(base + SD_CTL, r.read32(base + SD_CTL) | SD_RESET);
        r.wait(10, |r| r.read32(base + SD_CTL) & SD_RESET != 0);
        r.write32(base + SD_CTL, r.read32(base + SD_CTL) & !SD_RESET);
        r.wait(10, |r| r.read32(base + SD_CTL) & SD_RESET == 0);
    }

    /// Resets a stream's descriptor (which rewinds it) and points it at its
    /// buffers again.
    fn program(&self, stream: &Stream, layout: Layout) {
        self.reset_stream(stream);
        let base = self.descriptor(stream);
        let r = &self.registers;
        r.write32(base + SD_BDPL, layout.list as u32);
        r.write32(base + SD_BDPU, (layout.list >> 32) as u32);
        r.write32(base + SD_CBL, layout.bytes as u32);
        r.write16(base + SD_LVI, (layout.periods - 1) as u16);
        r.write16(base + SD_FMT, layout.format);
        // The stream's tag goes in bits 20-23 of the control register.
        let control = r.read32(base + SD_CTL) & !(0xF << 20);
        r.write32(base + SD_CTL, control | u32::from(stream.tag) << 20);
    }

    /// Mutes the speakers while headphones are plugged in, checking every
    /// half second (jack interrupts would need unsolicited responses).
    fn watch_jacks(self: Arc<Self>) {
        loop {
            let plugged = self.command(|codec, link| codec.headphones_plugged(link));
            if plugged != self.headphones.swap(plugged, Ordering::Relaxed) {
                info!("hda: headphones {}", if plugged { "plugged in" } else { "unplugged" });
                self.command(|codec, link| codec.set_speakers(link, !plugged));
            }
            crate::task::sleep_ms(500);
        }
    }
}

fn start_rings(registers: &Registers, rings: &Rings) -> core::result::Result<(), &'static str> {
    // Stop both rings' DMA before changing them.
    registers.write8(CORBCTL, 0);
    registers.write8(RIRBCTL, 0);
    registers.wait(10, |r| r.read8(CORBCTL) & 2 == 0 && r.read8(RIRBCTL) & 2 == 0);
    let phys = rings.memory.phys;
    registers.write32(CORBLBASE, phys as u32);
    registers.write32(CORBUBASE, (phys >> 32) as u32);
    let rirb = phys + RIRB_OFFSET as u64;
    registers.write32(RIRBLBASE, rirb as u32);
    registers.write32(RIRBUBASE, (rirb >> 32) as u32);
    // 256 entries each (size code 2), if offered (bit 6 of the capability).
    if registers.read8(CORBSIZE) & 0x40 == 0 || registers.read8(RIRBSIZE) & 0x40 == 0 {
        return Err("command rings can't hold 256 entries");
    }
    registers.write8(CORBSIZE, 2);
    registers.write8(RIRBSIZE, 2);
    // Reset the CORB read pointer (some controllers want to see the bit
    // set and then cleared) and the RIRB write pointer.
    registers.write16(CORBRP, 0x8000);
    registers.wait(1, |r| r.read16(CORBRP) & 0x8000 != 0);
    registers.write16(CORBRP, 0);
    registers.wait(1, |r| r.read16(CORBRP) & 0x8000 == 0);
    registers.write16(CORBWP, 0);
    registers.write16(RIRBWP, 0x8000);
    registers.write16(RINTCNT, 1);
    registers.write8(CORBCTL, 2);
    // DMA on, and the response flag (bit 0) too: controllers count
    // responses up to RINTCNT and only reset the count when that flag is
    // cleared, so without it they stop after the first (QEMU does). With
    // CIE off in INTCTL the flag raises no interrupt.
    registers.write8(RIRBCTL, 3);
    if !registers.wait(10, |r| r.read8(CORBCTL) & 2 != 0 && r.read8(RIRBCTL) & 2 != 0) {
        return Err("command rings didn't start");
    }
    Ok(())
}

/// The controllers' interrupt: a stream finished a period.
fn interrupt() {
    for controller in CONTROLLERS.lock().iter() {
        let status = controller.registers.read32(INTSTS);
        for stream in [Some(&controller.output), controller.input.as_ref()].into_iter().flatten() {
            if status & (1 << stream.index) != 0 {
                let base = controller.descriptor(stream);
                controller.registers.write8(base + SD_STS, 0x1C);
                stream.events.notify();
            }
        }
    }
}

/// The sound layer's view of one of a controller's streams.
struct StreamHandle {
    controller: Arc<Controller>,
    output: bool,
}

impl StreamHandle {
    fn stream(&self) -> &Stream {
        self.controller.stream(self.output).expect("handles exist only for streams that do")
    }
}

/// The stream format word for 16-bit stereo at `rate`.
fn format(rate: u32) -> u16 {
    let base = if rate == 44_100 { 1 << 14 } else { 0 };
    base | 1 << 4 | 1
}

impl Hardware for StreamHandle {
    fn direction(&self) -> Direction {
        self.stream().direction
    }

    fn rates(&self) -> &[u32] {
        &RATES
    }

    fn configure(&self, rate: u32, period_bytes: usize, periods: usize) -> Result<()> {
        if !RATES.contains(&rate) || !period_bytes.is_multiple_of(128) || !(2..=256).contains(&periods) {
            return Err(Errno::EINVAL);
        }
        let controller = &self.controller;
        let stream = self.stream();
        self.stop();
        let bytes = period_bytes * periods;
        let pages = bytes.div_ceil(memory::PAGE_SIZE as usize);
        let buffer = DmaBuffer::new(pages, 1).ok_or(Errno::ENOMEM)?;
        let list = DmaBuffer::new(1, 1).ok_or(Errno::ENOMEM)?;
        for period in 0..periods {
            let entry = period * 16;
            let address = buffer.phys + (period * period_bytes) as u64;
            list.write32(entry, address as u32);
            list.write32(entry + 4, (address >> 32) as u32);
            list.write32(entry + 8, period_bytes as u32);
            // Interrupt at the end of every period.
            list.write32(entry + 12, 1);
        }
        let state = StreamState { buffer: Some(buffer), list: Some(list), bytes, periods, format: format(rate) };
        let layout = state.layout();
        *stream.state.lock() = state;
        if let Some(layout) = layout {
            controller.program(stream, layout);
        }
        Ok(())
    }

    fn prepare(&self) {
        self.stop();
        let controller = &self.controller;
        let stream = self.stream();
        let layout = {
            let state = stream.state.lock();
            if let Some(buffer) = &state.buffer {
                buffer.zero();
            }
            state.layout()
        };
        let Some(layout) = layout else { return };
        controller.program(stream, layout);
        // The codec's converters join the stream now, so that starting and
        // stopping are just the DMA engine (and can happen anywhere).
        let tag = stream.tag;
        controller.command(|codec, link| {
            if self.output {
                codec.start_output(link, tag, layout.format);
            } else {
                codec.start_input(link, tag, layout.format);
            }
        });
        if self.output && controller.headphones.load(Ordering::Relaxed) {
            controller.command(|codec, link| codec.set_speakers(link, false));
        }
    }

    fn pause(&self, paused: bool) {
        let controller = &self.controller;
        let stream = self.stream();
        let base = controller.descriptor(stream);
        let r = &controller.registers;
        let control = r.read32(base + SD_CTL);
        if paused {
            r.write32(base + SD_CTL, control & !SD_RUN);
            r.wait(10, |r| r.read32(base + SD_CTL) & SD_RUN == 0);
        } else {
            r.write32(base + SD_CTL, control | SD_RUN | SD_INTERRUPT_ON_COMPLETION);
        }
    }

    fn release(&self) {
        self.stop();
        self.controller.command(|codec, link| codec.stop(link, self.output));
        *self.stream().state.lock() = StreamState::default();
    }

    fn write_buffer(&self, offset: usize, data: &[u8]) {
        if let Some(buffer) = &self.stream().state.lock().buffer {
            buffer.write_bytes(offset, data);
        }
    }

    fn read_buffer(&self, offset: usize, data: &mut [u8]) {
        if let Some(buffer) = &self.stream().state.lock().buffer {
            data.copy_from_slice(&buffer.read_bytes(offset, data.len()));
        }
    }

    fn start(&self) {
        let controller = &self.controller;
        let stream = self.stream();
        let base = controller.descriptor(stream);
        let r = &controller.registers;
        r.write8(base + SD_STS, 0x1C);
        r.write32(base + SD_CTL, r.read32(base + SD_CTL) | SD_RUN | SD_INTERRUPT_ON_COMPLETION);
        stream.running.store(true, Ordering::Release);
    }

    fn stop(&self) {
        let controller = &self.controller;
        let stream = self.stream();
        if !stream.running.swap(false, Ordering::AcqRel) {
            return;
        }
        let base = controller.descriptor(stream);
        let r = &controller.registers;
        r.write32(base + SD_CTL, r.read32(base + SD_CTL) & !(SD_RUN | SD_INTERRUPT_ON_COMPLETION));
        r.wait(10, |r| r.read32(base + SD_CTL) & SD_RUN == 0);
        r.write8(base + SD_STS, 0x1C);
        stream.events.notify();
    }

    fn position(&self) -> usize {
        let stream = self.stream();
        let bytes = stream.state.lock().bytes;
        let position = self.controller.registers.read32(self.controller.descriptor(stream) + SD_LPIB) as usize;
        if bytes == 0 { 0 } else { position % bytes }
    }

    fn events(&self) -> Arc<PollSource> {
        self.stream().events.clone()
    }
}

impl Mixer for Controller {
    fn volume_steps(&self, output: bool) -> Option<u32> {
        self.codec.volume_amp(output).map(|(_, _, steps)| steps.count)
    }

    fn default_volume(&self, output: bool) -> u32 {
        // 0 dB, where the codec's amp has it.
        self.codec.volume_amp(output).map_or(0, |(_, _, steps)| steps.zero)
    }

    fn set_volume(&self, output: bool, step: u32, unmuted: bool) {
        self.command(|codec, link| codec.set_volume(link, output, step, unmuted));
    }
}
