//! xHCI, the USB host controller interface of every PC from the last
//! decade. It handles USB 3 devices and, through the same interface, USB 1
//! and 2 devices too.
//!
//! The controller and the driver talk through rings of 16-byte "TRBs" in
//! memory. The driver puts commands on the command ring and transfers on
//! each endpoint's transfer ring, then rings a doorbell register; the
//! controller reports completions and port changes on the event ring. Each
//! ring has a cycle bit that flips every time it wraps, which is how either
//! side tells new entries from old ones without a shared index.
//!
//! One kernel thread owns each controller: it sets it up, finds devices on
//! its ports (and later ones plugged in), and handles their input.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::hid::BootKeyboard;
use super::{Descriptors, KEYBOARDS, SETTLED, SetupPacket};
use crate::arch::{apic, interrupts};
use crate::drivers::pci;
use crate::keyboard::{self, Key};
use crate::memory::{self, CacheType, PAGE_SIZE, frames, phys_to_virt};
use crate::task::{self, WaitQueue};
use crate::time;

// Capability registers.
const CAPLENGTH: u64 = 0x00;
const HCSPARAMS1: u64 = 0x04;
const HCSPARAMS2: u64 = 0x08;
const HCCPARAMS1: u64 = 0x10;
const DBOFF: u64 = 0x14;
const RTSOFF: u64 = 0x18;

// Operational registers.
const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;
const PORTSC: u64 = 0x400;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTERRUPTS: u32 = 1 << 2;
const STS_HALTED: u32 = 1 << 0;
const STS_EVENT_INTERRUPT: u32 = 1 << 3;
const STS_NOT_READY: u32 = 1 << 11;

// Interrupter 0's registers, in the runtime block.
const IMAN: u64 = 0x20;
const ERSTSZ: u64 = 0x28;
const ERSTBA: u64 = 0x30;
const ERDP: u64 = 0x38;
const IMAN_PENDING: u32 = 1 << 0;
const IMAN_ENABLE: u32 = 1 << 1;
const ERDP_BUSY: u64 = 1 << 3;

// Port status and control bits.
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_RESET_CHANGE: u32 = 1 << 21;
/// The status change bits, which are cleared by writing 1.
const PORT_CHANGES: u32 = 0x7F << 17;
/// Bits that keep their value when written back (the rest are read-only,
/// write-1-to-clear, or would disable the port if written as read).
const PORT_PRESERVE: u32 = 0x0E00_C3E0;

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

// TRB control bits.
const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_INTERRUPT_ON_SHORT: u32 = 1 << 2;
const TRB_INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const TRB_IMMEDIATE_DATA: u32 = 1 << 6;
const TRB_DIRECTION_IN: u32 = 1 << 16;

const COMPLETION_SUCCESS: u8 = 1;
const COMPLETION_SHORT_PACKET: u8 = 13;

/// Each ring is one page of TRBs, the last of which links back to the
/// first.
const RING_TRBS: usize = PAGE_SIZE as usize / 16;

// Endpoint types in endpoint contexts.
const ENDPOINT_CONTROL: u32 = 4;
const ENDPOINT_INTERRUPT_IN: u32 = 7;

/// Port speeds, as PORTSC reports them.
const SPEED_FULL: u32 = 1;
const SPEED_LOW: u32 = 2;
const SPEED_HIGH: u32 = 3;

const XHCI_VECTOR: u8 = interrupts::IRQ_BASE + 0x20;

/// Woken by the controllers' interrupt.
static EVENTS: WaitQueue = WaitQueue::new();
/// Whether interrupts work; without them the driver polls.
static INTERRUPTS: AtomicBool = AtomicBool::new(false);

/// Physically contiguous, zeroed memory the controller reads and writes.
struct Dma {
    phys: u64,
}

impl Dma {
    fn new(pages: usize) -> Option<Self> {
        let phys = frames::allocate_contiguous(pages, 1)?;
        // SAFETY: freshly allocated RAM, in the direct map.
        unsafe { ptr::write_bytes(phys_to_virt(phys), 0, pages * PAGE_SIZE as usize) };
        Some(Self { phys })
    }

    fn pointer<T>(&self, offset: usize) -> *mut T {
        // SAFETY: callers stay within the allocation.
        unsafe { phys_to_virt(self.phys).add(offset).cast() }
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: within the allocation; the controller may write it, hence
        // volatile.
        unsafe { ptr::read_volatile(self.pointer(offset)) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as for `read32`.
        unsafe { ptr::write_volatile(self.pointer(offset), value) };
    }

    fn write64(&self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}

/// A producer ring: the command ring, or an endpoint's transfer ring.
struct Ring {
    memory: Dma,
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    fn new() -> Option<Self> {
        let memory = Dma::new(1)?;
        // The last TRB links back to the start, flipping the cycle bit.
        let link = (RING_TRBS - 1) * 16;
        memory.write64(link, memory.phys);
        memory.write32(link + 12, TRB_LINK << 10 | TRB_TOGGLE_CYCLE);
        Some(Self { memory, enqueue: 0, cycle: true })
    }

    /// The address the controller starts dequeuing from, with its cycle
    /// state, for contexts and CRCR.
    fn start(&self) -> u64 {
        self.memory.phys | 1
    }

    /// Adds a TRB and returns its physical address. The control word is
    /// written last, with the cycle bit that hands it to the controller.
    fn push(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
        let offset = self.enqueue * 16;
        self.memory.write64(offset, parameter);
        self.memory.write32(offset + 8, status);
        self.memory.write32(offset + 12, control | u32::from(self.cycle));
        let address = self.memory.phys + offset as u64;
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            // Hand the link TRB over too, then wrap.
            let link = self.enqueue * 16;
            let control = self.memory.read32(link + 12) & !TRB_CYCLE;
            self.memory.write32(link + 12, control | u32::from(self.cycle));
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        address
    }
}

/// An entry from the event ring.
#[derive(Debug, Clone, Copy)]
struct Event {
    kind: u32,
    completion: u8,
    /// The TRB the event is about (commands, transfers), or the port in
    /// bits 24-31 (port status changes).
    parameter: u64,
    /// For transfers: how many bytes weren't transferred.
    residual: u32,
    slot: u8,
    endpoint: u8,
}

/// A HID keyboard's interrupt endpoint.
struct KeyboardEndpoint {
    interface: u8,
    endpoint: u8,
    ring: Ring,
    buffer: Dma,
    packet_size: u16,
    state: BootKeyboard,
}

struct Device {
    port: u8,
    /// The device context the controller keeps the device's state in.
    output: Dma,
    input: Dma,
    control: Ring,
    keyboard: Option<KeyboardEndpoint>,
}

pub struct Controller {
    operational: u64,
    runtime: u64,
    doorbells: u64,
    ports: u8,
    /// Bytes per context structure: 32, or 64 on controllers that say so.
    context_size: usize,
    device_contexts: Dma,
    commands: Ring,
    event_ring: Dma,
    event_dequeue: usize,
    event_cycle: bool,
    /// Events read while waiting for a particular one.
    pending: VecDeque<Event>,
    devices: BTreeMap<u8, Device>,
    name: String,
}

/// Starts a thread for each xHCI controller.
pub fn probe(devices: &[pci::Device]) -> usize {
    let mut count = 0;
    for device in devices.iter().filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.interface == 0x30) {
        let Some(base) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        let name = format!("usb{count}");
        if device.enable_msi(apic::local_id(), XHCI_VECTOR) {
            interrupts::set_handler(XHCI_VECTOR, |_| EVENTS.wake_all());
            INTERRUPTS.store(true, Ordering::Relaxed);
        }
        let thread_name = name.clone();
        task::spawn(&thread_name, move || match Controller::start(base, name) {
            Ok(controller) => controller.run(),
            Err(error) => {
                warn!("usb: controller at {base:#x}: {error}");
                SETTLED.fetch_add(1, Ordering::Release);
            }
        });
        count += 1;
    }
    count
}

impl Controller {
    fn start(base: u64, name: String) -> Result<Self, &'static str> {
        let registers = memory::map_device(base, 0x1_0000, CacheType::Uncached) as u64;
        let read = |offset: u64| mmio_read(registers + offset);
        let capability_length = u64::from(read(CAPLENGTH) & 0xFF);
        let structural = read(HCSPARAMS1);
        let parameters = read(HCCPARAMS1);
        let slots = structural as u8;
        let ports = (structural >> 24) as u8;

        take_ownership(registers, u64::from(parameters >> 16) * 4);

        let controller = Self {
            operational: registers + capability_length,
            runtime: registers + u64::from(read(RTSOFF) & !0x1F),
            doorbells: registers + u64::from(read(DBOFF) & !0x3),
            ports,
            context_size: if parameters & (1 << 2) != 0 { 64 } else { 32 },
            device_contexts: Dma::new(1).ok_or("out of memory")?,
            commands: Ring::new().ok_or("out of memory")?,
            event_ring: Dma::new(1).ok_or("out of memory")?,
            event_dequeue: 0,
            event_cycle: true,
            pending: VecDeque::new(),
            devices: BTreeMap::new(),
            name,
        };
        controller.reset()?;

        // Scratchpad: memory some controllers need for themselves.
        let scratch_parameters = read(HCSPARAMS2);
        let scratchpads = ((scratch_parameters >> 21 & 0x1F) << 5 | scratch_parameters >> 27) as usize;
        if scratchpads > 0 {
            let array = Dma::new(1).ok_or("out of memory")?;
            for index in 0..scratchpads.min(RING_TRBS * 2) {
                let page = Dma::new(1).ok_or("out of memory")?;
                array.write64(index * 8, page.phys);
            }
            controller.device_contexts.write64(0, array.phys);
        }

        let c = &controller;
        c.op_write(CONFIG, u32::from(slots));
        c.op_write64(DCBAAP, c.device_contexts.phys);
        c.op_write64(CRCR, c.commands.start());
        // The event ring is one segment, described by a one-entry table.
        let table = Dma::new(1).ok_or("out of memory")?;
        table.write64(0, c.event_ring.phys);
        table.write32(8, RING_TRBS as u32);
        mmio_write(c.runtime + ERSTSZ, 1);
        mmio_write64(c.runtime + ERDP, c.event_ring.phys);
        mmio_write64(c.runtime + ERSTBA, table.phys);
        mmio_write(c.runtime + IMAN, IMAN_ENABLE | IMAN_PENDING);
        c.op_write(USBCMD, CMD_RUN | CMD_INTERRUPTS);
        if !wait_for(|| c.op_read(USBSTS) & STS_HALTED == 0, 100) {
            return Err("the controller didn't start");
        }
        info!(
            "usb: {}: xHCI with {ports} ports, {slots} device slots{}",
            controller.name,
            if INTERRUPTS.load(Ordering::Relaxed) { "" } else { ", polled" }
        );
        Ok(controller)
    }

    fn op_read(&self, offset: u64) -> u32 {
        mmio_read(self.operational + offset)
    }

    fn op_write(&self, offset: u64, value: u32) {
        mmio_write(self.operational + offset, value);
    }

    fn op_write64(&self, offset: u64, value: u64) {
        mmio_write64(self.operational + offset, value);
    }

    fn port_status(&self, port: u8) -> u32 {
        self.op_read(PORTSC + 0x10 * u64::from(port - 1))
    }

    fn set_port_status(&self, port: u8, value: u32) {
        self.op_write(PORTSC + 0x10 * u64::from(port - 1), value);
    }

    fn reset(&self) -> Result<(), &'static str> {
        self.op_write(USBCMD, self.op_read(USBCMD) & !CMD_RUN);
        if !wait_for(|| self.op_read(USBSTS) & STS_HALTED != 0, 100) {
            return Err("the controller didn't stop");
        }
        self.op_write(USBCMD, CMD_RESET);
        if !wait_for(|| self.op_read(USBCMD) & CMD_RESET == 0 && self.op_read(USBSTS) & STS_NOT_READY == 0, 1000) {
            return Err("the controller didn't reset");
        }
        Ok(())
    }

    fn ring_doorbell(&self, slot: u8, target: u8) {
        mmio_write(self.doorbells + 4 * u64::from(slot), u32::from(target));
    }

    /// The controller's thread: finds devices, then handles events forever.
    fn run(mut self) {
        for port in 1..=self.ports {
            let status = self.port_status(port);
            if status & PORT_POWER == 0 {
                self.set_port_status(port, (status & PORT_PRESERVE) | PORT_POWER);
            }
        }
        task::sleep_ms(20);
        for port in 1..=self.ports {
            let status = self.port_status(port);
            self.set_port_status(port, (status & PORT_PRESERVE) | (status & PORT_CHANGES));
            if status & PORT_CONNECTED != 0 {
                self.attach(port);
            }
        }
        SETTLED.fetch_add(1, Ordering::Release);

        let mut keys = Vec::new();
        loop {
            while let Some(event) = self.next_event() {
                self.handle(event, &mut keys);
            }
            let now = time::ticks();
            for device in self.devices.values_mut() {
                if let Some(keyboard) = &mut device.keyboard
                    && let Some(key) = keyboard.state.repeat(now)
                {
                    keys.push(key);
                }
            }
            keys.drain(..).for_each(keyboard::send_key);

            // Sleep until an event, a key repeat, or (without interrupts)
            // the next poll.
            let repeat = self.devices.values().filter_map(|d| d.keyboard.as_ref()?.state.next_repeat()).min();
            let poll = (!INTERRUPTS.load(Ordering::Relaxed)).then(|| now + 10);
            let deadline = [repeat, poll].into_iter().flatten().min();
            let this = &self;
            EVENTS.wait_until_deadline(deadline, || this.event_ready().then_some(()));
        }
    }

    fn handle(&mut self, event: Event, keys: &mut Vec<Key>) {
        match event.kind {
            TRB_TRANSFER_EVENT => self.on_transfer(event, keys),
            TRB_PORT_STATUS_CHANGE => {
                let port = (event.parameter >> 24) as u8;
                let status = self.port_status(port);
                self.set_port_status(port, (status & PORT_PRESERVE) | (status & PORT_CHANGES));
                let known = self.devices.values().any(|device| device.port == port);
                if status & PORT_CONNECTED != 0 && !known {
                    self.attach(port);
                } else if status & PORT_CONNECTED == 0 && known {
                    self.detach(port);
                }
            }
            _ => {}
        }
    }

    fn on_transfer(&mut self, event: Event, keys: &mut Vec<Key>) {
        let Some(device) = self.devices.get_mut(&event.slot) else { return };
        let Some(keyboard) = &mut device.keyboard else { return };
        if event.endpoint != keyboard.endpoint {
            return;
        }
        if matches!(event.completion, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET) {
            let length = usize::from(keyboard.packet_size).saturating_sub(event.residual as usize);
            // SAFETY: the buffer holds a page; the controller is done with
            // it until the next transfer is queued.
            let report = unsafe { core::slice::from_raw_parts(keyboard.buffer.pointer::<u8>(0), length.min(64)) };
            let leds_changed = keyboard.state.report(report, time::ticks(), keys);
            let (interface, leds) = (keyboard.interface, keyboard.state.leds());
            self.queue_report(event.slot);
            if leds_changed {
                let setup = SetupPacket::class_interface_out(0x09, 0x0200, interface, 1);
                let _ = self.control_transfer(event.slot, setup, Some(&mut [leds]));
            }
        } else {
            debug!("usb: keyboard transfer failed (completion code {})", event.completion);
            self.queue_report(event.slot);
        }
    }

    /// Asks the keyboard on `slot` for its next report.
    fn queue_report(&mut self, slot: u8) {
        let Some(keyboard) = self.devices.get_mut(&slot).and_then(|d| d.keyboard.as_mut()) else { return };
        let control = TRB_NORMAL << 10 | TRB_INTERRUPT_ON_COMPLETION | TRB_INTERRUPT_ON_SHORT;
        keyboard.ring.push(keyboard.buffer.phys, u32::from(keyboard.packet_size), control);
        let endpoint = keyboard.endpoint;
        self.ring_doorbell(slot, endpoint);
    }

    /// Whether the event ring has an entry for us.
    fn event_ready(&self) -> bool {
        let control = self.event_ring.read32(self.event_dequeue * 16 + 12);
        (control & TRB_CYCLE != 0) == self.event_cycle
    }

    fn next_event(&mut self) -> Option<Event> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        // Acknowledge the interrupt before reading, so a new event raises
        // another.
        mmio_write(self.runtime + IMAN, IMAN_ENABLE | IMAN_PENDING);
        self.op_write(USBSTS, STS_EVENT_INTERRUPT);
        if !self.event_ready() {
            return None;
        }
        let offset = self.event_dequeue * 16;
        let ring = &self.event_ring;
        let status = ring.read32(offset + 8);
        let control = ring.read32(offset + 12);
        let event = Event {
            kind: control >> 10 & 0x3F,
            completion: (status >> 24) as u8,
            parameter: u64::from(ring.read32(offset)) | u64::from(ring.read32(offset + 4)) << 32,
            residual: status & 0xFF_FFFF,
            slot: (control >> 24) as u8,
            endpoint: (control >> 16 & 0x1F) as u8,
        };
        self.event_dequeue += 1;
        if self.event_dequeue == RING_TRBS {
            self.event_dequeue = 0;
            self.event_cycle = !self.event_cycle;
        }
        let dequeue = self.event_ring.phys + self.event_dequeue as u64 * 16;
        mmio_write64(self.runtime + ERDP, dequeue | ERDP_BUSY);
        Some(event)
    }

    /// Waits (up to `timeout_ms`) for an event `wanted` accepts; others are
    /// kept for later.
    fn wait_event(&mut self, timeout_ms: u64, wanted: impl Fn(&Event) -> bool) -> Option<Event> {
        let deadline = time::ticks() + timeout_ms;
        let mut skipped = VecDeque::new();
        let result = loop {
            match self.next_event() {
                Some(event) if wanted(&event) => break Some(event),
                Some(event) => skipped.push_back(event),
                None if time::ticks() >= deadline => break None,
                None => task::sleep_ms(1),
            }
        };
        skipped.append(&mut self.pending);
        self.pending = skipped;
        result
    }

    /// Runs a command and returns its completion event.
    fn command(&mut self, parameter: u64, control: u32) -> Result<Event, String> {
        let address = self.commands.push(parameter, 0, control);
        self.ring_doorbell(0, 0);
        let event = self
            .wait_event(1000, |e| e.kind == TRB_COMMAND_COMPLETION && e.parameter == address)
            .ok_or("command timed out")?;
        if event.completion != COMPLETION_SUCCESS {
            return Err(format!("command failed (completion code {})", event.completion));
        }
        Ok(event)
    }

    /// A control transfer on the device's endpoint 0, with an optional data
    /// stage (in or out, as the setup packet says).
    fn control_transfer(&mut self, slot: u8, setup: SetupPacket, data: Option<&mut [u8]>) -> Result<usize, String> {
        let device = self.devices.get_mut(&slot).ok_or("no such device")?;
        let length = data.as_ref().map_or(0, |data| data.len());
        let input = setup.request_type & 0x80 != 0;
        let buffer = Dma::new(1).ok_or("out of memory")?;
        if let Some(data) = &data
            && !input
        {
            // SAFETY: the buffer is a page; data is at most wLength bytes.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buffer.pointer(0), data.len().min(4096)) };
        }
        let transfer_type = match (length, input) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        let ring = &mut device.control;
        ring.push(setup.as_u64(), 8, TRB_SETUP << 10 | TRB_IMMEDIATE_DATA | transfer_type << 16);
        if length > 0 {
            let direction = if input { TRB_DIRECTION_IN } else { 0 };
            ring.push(buffer.phys, length as u32, TRB_DATA << 10 | direction);
        }
        // The status stage goes the other way from the data.
        let direction = if length > 0 && input { 0 } else { TRB_DIRECTION_IN };
        let status = ring.push(0, 0, TRB_STATUS << 10 | direction | TRB_INTERRUPT_ON_COMPLETION);
        self.ring_doorbell(slot, 1);

        let event = self.wait_event(1000, |e| e.kind == TRB_TRANSFER_EVENT && e.slot == slot && e.endpoint == 1);
        let result = match event {
            None => Err(String::from("control transfer timed out")),
            Some(event) if !matches!(event.completion, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET) => {
                Err(format!("control transfer failed (completion code {})", event.completion))
            }
            Some(event) if event.parameter != status => {
                // A short data stage ends the transfer early: the event is
                // for the data TRB, with what's left in `residual`.
                Ok(length.saturating_sub(event.residual as usize))
            }
            Some(_) => Ok(length),
        };
        if let (Ok(received), Some(data)) = (&result, data)
            && input
        {
            let count = (*received).min(data.len());
            // SAFETY: the controller wrote up to `length` bytes.
            unsafe { ptr::copy_nonoverlapping(buffer.pointer::<u8>(0), data.as_mut_ptr(), count) };
        }
        frames::free(buffer.phys);
        result
    }

    /// Resets the device on `port` (if it's a USB 2 port), gives it an
    /// address, reads its descriptors, and starts a driver if there is one.
    fn attach(&mut self, port: u8) {
        if let Err(error) = self.try_attach(port) {
            warn!("usb: {}: port {port}: {error}", self.name);
        }
    }

    fn try_attach(&mut self, port: u8) -> Result<(), String> {
        let status = self.port_status(port);
        if status & PORT_ENABLED == 0 {
            // USB 2 ports need a reset to enable the device; USB 3 ports
            // come up enabled by themselves.
            self.set_port_status(port, (status & PORT_PRESERVE) | PORT_RESET);
            if !wait_for(|| self.port_status(port) & PORT_RESET_CHANGE != 0, 500) {
                return Err("port reset timed out".into());
            }
            let status = self.port_status(port);
            self.set_port_status(port, (status & PORT_PRESERVE) | PORT_RESET_CHANGE);
            if status & PORT_ENABLED == 0 {
                return Err("port didn't enable".into());
            }
            // Reset recovery time, as USB requires.
            task::sleep_ms(10);
        }
        let speed = self.port_status(port) >> 10 & 0xF;

        let slot = (self.command(0, TRB_ENABLE_SLOT << 10)?.slot_id())?;
        let device = Device {
            port,
            output: Dma::new(1).ok_or("out of memory")?,
            input: Dma::new(1).ok_or("out of memory")?,
            control: Ring::new().ok_or("out of memory")?,
            keyboard: None,
        };
        self.device_contexts.write64(usize::from(slot) * 8, device.output.phys);
        let packet_size = match speed {
            SPEED_LOW | SPEED_FULL => 8,
            SPEED_HIGH => 64,
            _ => 512,
        };
        // Input context: the input control context (adding the slot and
        // endpoint 0), the slot context, then endpoint 0's.
        let size = self.context_size;
        let input = &device.input;
        input.write32(4, 0b11);
        input.write32(size, speed << 20 | 1 << 27);
        input.write32(size + 4, u32::from(port) << 16);
        input.write32(2 * size + 4, 3 << 1 | ENDPOINT_CONTROL << 3 | packet_size << 16);
        input.write64(2 * size + 8, device.control.start());
        input.write32(2 * size + 16, 8);
        let input_phys = input.phys;
        self.devices.insert(slot, device);
        if let Err(error) = self.command(input_phys, TRB_ADDRESS_DEVICE << 10 | u32::from(slot) << 24) {
            self.forget(slot);
            return Err(error);
        }

        let result = self.identify(slot, speed, packet_size);
        if result.is_err() {
            self.forget(slot);
        }
        result
    }

    fn identify(&mut self, slot: u8, speed: u32, packet_size: u32) -> Result<(), String> {
        // The first 8 bytes of the device descriptor say how big endpoint
        // 0's packets really are (full-speed devices vary).
        let mut header = [0u8; 8];
        self.control_transfer(slot, SetupPacket::get_descriptor(1, 0, 8), Some(&mut header))?;
        let actual = if speed > SPEED_HIGH { 1 << header[7].min(9) } else { u32::from(header[7]) };
        if actual != packet_size && actual >= 8 {
            let size = self.context_size;
            let input = &self.devices[&slot].input;
            input.write32(0, 0);
            input.write32(4, 0b10);
            let word = input.read32(2 * size + 4) & 0xFFFF;
            input.write32(2 * size + 4, word | actual << 16);
            let phys = input.phys;
            self.command(phys, TRB_EVALUATE_CONTEXT << 10 | u32::from(slot) << 24)?;
        }

        let mut device_descriptor = [0u8; 18];
        self.control_transfer(slot, SetupPacket::get_descriptor(1, 0, 18), Some(&mut device_descriptor))?;
        let vendor = u16::from_le_bytes([device_descriptor[8], device_descriptor[9]]);
        let product = u16::from_le_bytes([device_descriptor[10], device_descriptor[11]]);
        let product_name = match device_descriptor[15] {
            0 => None,
            index => self.string(slot, index),
        };

        let mut header = [0u8; 9];
        self.control_transfer(slot, SetupPacket::get_descriptor(2, 0, 9), Some(&mut header))?;
        let total = usize::from(u16::from_le_bytes([header[2], header[3]])).clamp(9, 4096);
        let mut configuration = alloc::vec![0u8; total];
        self.control_transfer(slot, SetupPacket::get_descriptor(2, 0, total as u16), Some(&mut configuration))?;
        let descriptors = Descriptors::parse(&configuration);
        let port = self.devices[&slot].port;
        let description = product_name.unwrap_or_else(|| format!("{vendor:04x}:{product:04x}"));

        let Some(keyboard) = descriptors.boot_keyboard() else {
            info!("usb: {}: port {port}: {description} (no driver)", self.name);
            return Ok(());
        };
        self.control_transfer(slot, SetupPacket::set_configuration(descriptors.configuration), None)?;
        // Boot protocol, and reports only when something changes.
        self.control_transfer(slot, SetupPacket::class_interface_out(0x0B, 0, keyboard.interface, 0), None)?;
        let _ = self.control_transfer(slot, SetupPacket::class_interface_out(0x0A, 0, keyboard.interface, 0), None);

        // Configure the interrupt endpoint. Its context index is 2n + 1 for
        // IN endpoint n.
        let endpoint = (keyboard.address & 0xF) * 2 + 1;
        let packet_size = keyboard.packet_size.clamp(8, 64);
        let interval = if speed >= SPEED_HIGH {
            u32::from(keyboard.interval.clamp(1, 16) - 1)
        } else {
            // In frames (1 ms); the context counts in 125 µs units, as a
            // power of two.
            let microframes = u32::from(keyboard.interval.max(1)) * 8;
            (31 - microframes.leading_zeros()).clamp(3, 10)
        };
        let ring = Ring::new().ok_or("out of memory")?;
        let size = self.context_size;
        let device = &self.devices[&slot];
        let input = &device.input;
        let context = (1 + usize::from(endpoint)) * size;
        for offset in (0..size * 33).step_by(4) {
            input.write32(offset, 0);
        }
        input.write32(4, 1 | 1 << endpoint);
        // The slot context, now with entries up to the new endpoint.
        input.write32(size, speed << 20 | u32::from(endpoint) << 27);
        input.write32(size + 4, u32::from(device.port) << 16);
        input.write32(context, interval << 16);
        input.write32(context + 4, 3 << 1 | ENDPOINT_INTERRUPT_IN << 3 | u32::from(packet_size) << 16);
        input.write64(context + 8, ring.start());
        input.write32(context + 16, u32::from(packet_size) << 16 | u32::from(packet_size));
        let phys = input.phys;
        self.command(phys, TRB_CONFIGURE_ENDPOINT << 10 | u32::from(slot) << 24)?;

        let buffer = Dma::new(1).ok_or("out of memory")?;
        let state = BootKeyboard::default();
        let interface = keyboard.interface;
        self.devices.get_mut(&slot).unwrap().keyboard =
            Some(KeyboardEndpoint { interface, endpoint, ring, buffer, packet_size, state });
        let leds = SetupPacket::class_interface_out(0x09, 0x0200, interface, 1);
        let _ = self.control_transfer(slot, leds, Some(&mut [0x01]));
        self.queue_report(slot);
        KEYBOARDS.fetch_add(1, Ordering::Relaxed);
        info!("usb: {}: port {port}: {description}: keyboard", self.name);
        Ok(())
    }

    /// A string descriptor, in US English.
    fn string(&mut self, slot: u8, index: u8) -> Option<String> {
        let mut buffer = [0u8; 255];
        let length = self
            .control_transfer(slot, SetupPacket::get_descriptor(3, index, 255).language(0x0409), Some(&mut buffer))
            .ok()?;
        let length = length.min(usize::from(buffer[0]));
        let units: Vec<u16> =
            buffer[2..length].as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        let text = String::from_utf16_lossy(&units);
        let text = text.trim();
        (!text.is_empty()).then(|| text.into())
    }

    fn detach(&mut self, port: u8) {
        let slots: Vec<u8> = self.devices.iter().filter(|(_, d)| d.port == port).map(|(&slot, _)| slot).collect();
        for slot in slots {
            if self.devices[&slot].keyboard.is_some() {
                KEYBOARDS.fetch_sub(1, Ordering::Relaxed);
            }
            let _ = self.command(0, TRB_DISABLE_SLOT << 10 | u32::from(slot) << 24);
            self.forget(slot);
            info!("usb: {}: port {port}: device removed", self.name);
        }
    }

    /// Drops what we know about a device and frees its memory.
    fn forget(&mut self, slot: u8) {
        self.device_contexts.write64(usize::from(slot) * 8, 0);
        if let Some(device) = self.devices.remove(&slot) {
            let mut pages = alloc::vec![device.output.phys, device.input.phys, device.control.memory.phys];
            if let Some(keyboard) = device.keyboard {
                pages.extend([keyboard.ring.memory.phys, keyboard.buffer.phys]);
            }
            pages.into_iter().for_each(frames::free);
        }
    }
}

impl Event {
    fn slot_id(&self) -> Result<u8, String> {
        if self.slot == 0 { Err("no free device slot".into()) } else { Ok(self.slot) }
    }
}

/// Takes the controller over from the firmware, which may be using it
/// (to emulate a PS/2 keyboard, say), through the "USB legacy support"
/// extended capability.
fn take_ownership(registers: u64, first_capability: u64) {
    let mut offset = first_capability;
    for _ in 0..64 {
        if offset == 0 {
            return;
        }
        let header = mmio_read(registers + offset);
        if header & 0xFF == 1 {
            const BIOS_OWNED: u32 = 1 << 16;
            const OS_OWNED: u32 = 1 << 24;
            mmio_write(registers + offset, header | OS_OWNED);
            if !wait_for(|| mmio_read(registers + offset) & BIOS_OWNED == 0, 1000) {
                warn!("usb: the firmware didn't let go of the controller");
            }
            // Turn off the firmware's system management interrupts, and
            // clear their status.
            let control = registers + offset + 4;
            let keep = (0x7 << 1) | (0xFF << 5) | (0x7 << 17);
            mmio_write(control, (mmio_read(control) & keep) | 0x7 << 29);
            return;
        }
        let next = u64::from(header >> 8 & 0xFF) * 4;
        offset = if next == 0 { 0 } else { offset + next };
    }
}

/// Polls `done` every millisecond, for up to `timeout_ms`.
fn wait_for(done: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = time::uptime_nanos() + timeout_ms * 1_000_000;
    loop {
        if done() {
            return true;
        }
        if time::uptime_nanos() >= deadline {
            return false;
        }
        task::sleep_ms(1);
    }
}

fn mmio_read(address: u64) -> u32 {
    // SAFETY: an address in the controller's mapped register block.
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn mmio_write(address: u64, value: u32) {
    // SAFETY: as for `mmio_read`.
    unsafe { ptr::write_volatile(address as *mut u32, value) };
}

/// 64-bit registers are written low half first.
fn mmio_write64(address: u64, value: u64) {
    mmio_write(address, value as u32);
    mmio_write(address + 4, (value >> 32) as u32);
}
