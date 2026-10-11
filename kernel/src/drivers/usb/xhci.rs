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
//! Each controller has a kernel thread: it sets it up, finds devices on its
//! ports (and later ones plugged in), and handles their input. The
//! controller is behind a (sleeping) lock, because disks on it are also
//! read and written from whatever thread needs them.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::hid::BootKeyboard;
use super::storage::{self, Found, Status, UsbDisk};
use super::{Descriptors, InterruptInterface, KEYBOARDS, SETTLED, SetupPacket, StorageInterface};
use crate::arch::{apic, interrupts};
use crate::drivers::block;
use crate::drivers::pci;
use crate::keyboard::{self, Key};
use crate::memory::{self, CacheType, PAGE_SIZE, frames, phys_to_virt};
use crate::sync::{Mutex, SpinLock};
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
const TRB_RESET_ENDPOINT: u32 = 14;
const TRB_SET_TR_DEQUEUE: u32 = 16;
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
const COMPLETION_STALL: u8 = 6;
const COMPLETION_SHORT_PACKET: u8 = 13;

/// Each ring is one page of TRBs, the last of which links back to the
/// first.
const RING_TRBS: usize = PAGE_SIZE as usize / 16;

// Endpoint types in endpoint contexts.
const ENDPOINT_BULK_OUT: u32 = 2;
const ENDPOINT_CONTROL: u32 = 4;
const ENDPOINT_BULK_IN: u32 = 6;
const ENDPOINT_INTERRUPT_IN: u32 = 7;

/// Port speeds, as PORTSC reports them.
const SPEED_FULL: u32 = 1;
const SPEED_LOW: u32 = 2;
const SPEED_HIGH: u32 = 3;
const SPEED_SUPER: u32 = 4;

/// Hubs can be chained five deep (the route string has five places).
const MAX_DEPTH: u8 = 5;

// Hub class features (USB 2.0 section 11.24.2, USB 3 section 10.16.2).
const FEATURE_PORT_RESET: u16 = 4;
const FEATURE_PORT_POWER: u16 = 8;
const FEATURE_C_PORT_RESET: u16 = 20;

const XHCI_VECTOR: u8 = interrupts::IRQ_BASE + 0x20;

/// Woken by the controllers' interrupt.
static EVENTS: WaitQueue = WaitQueue::new();
/// Set by the interrupt, so a controller's thread knows to look at its
/// events.
static SIGNAL: AtomicBool = AtomicBool::new(false);
/// Numbers devices, so a disk can tell its device from a later one given
/// the same slot.
static NEXT_DEVICE: AtomicU64 = AtomicU64::new(1);
/// The names of USB disks: (device, LUN) to `sdX`.
static DISK_NAMES: SpinLock<BTreeMap<(u64, u8), String>> = SpinLock::new(BTreeMap::new());
/// The largest storage transfer: 64 KiB, one TRB (which mustn't cross a
/// 64 KiB boundary).
const STORAGE_BUFFER_PAGES: usize = 16;
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

/// A mass storage device's bulk endpoints and buffers.
struct StorageEndpoints {
    interface: u8,
    /// Device context indexes (2n for OUT endpoint n, 2n + 1 for IN).
    input_index: u8,
    output_index: u8,
    /// Endpoint addresses, for CLEAR_FEATURE when one stalls.
    input_address: u8,
    output_address: u8,
    input_ring: Ring,
    output_ring: Ring,
    /// Command and status wrappers (one page), and the data buffer.
    wrappers: Dma,
    buffer: Dma,
    tag: u32,
}

/// A hub's status change endpoint, and its ports.
struct HubState {
    ports: u8,
    /// The transaction translator's think time and whether there's one per
    /// port, for the slot context (high-speed hubs).
    think_time: u32,
    multiple_translators: bool,
    endpoint: u8,
    ring: Ring,
    buffer: Dma,
    packet_size: u16,
}

/// Where a device is: which root port, through which hubs.
#[derive(Clone, Copy)]
struct Location {
    root_port: u8,
    /// Hub port numbers on the way, four bits each, nearest the root first.
    route: u32,
    /// 1 on a root port, 2 behind one hub, and so on.
    depth: u8,
    speed: u32,
    /// The hub and port it's plugged into, if not a root port.
    parent: Option<(u8, u8)>,
    /// For a low- or full-speed device behind a high-speed hub: the hub
    /// whose transaction translator talks to it, and the port.
    translator: Option<(u8, u8)>,
}

struct Device {
    /// Unique among all devices ever attached.
    id: u64,
    /// The root port it's on (directly or through hubs).
    port: u8,
    speed: u32,
    location: Location,
    hub: Option<HubState>,
    /// The device context the controller keeps the device's state in.
    output: Dma,
    input: Dma,
    control: Ring,
    keyboard: Option<KeyboardEndpoint>,
    storage: Option<StorageEndpoints>,
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
    /// Disks found, to register once the lock is released (registering
    /// reads their partition tables, through this controller).
    found: Vec<Found>,
    /// Devices removed whose disks should go.
    removed: Vec<u64>,
}

/// Starts a thread for each xHCI controller.
pub fn probe(devices: &[pci::Device]) -> usize {
    let mut count = 0;
    for device in devices.iter().filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.interface == 0x30) {
        let Some(base) = device.memory_bar(0) else { continue };
        device.enable_bus_mastering();
        let name = format!("usb{count}");
        if device.enable_msi(apic::local_id(), XHCI_VECTOR) {
            interrupts::set_handler(XHCI_VECTOR, |_| {
                SIGNAL.store(true, Ordering::Release);
                EVENTS.wake_all();
            });
            INTERRUPTS.store(true, Ordering::Relaxed);
        }
        let thread_name = name.clone();
        task::spawn(&thread_name, move || match Controller::start(base, name) {
            Ok(controller) => Controller::run(Arc::new(Mutex::new(controller))),
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
            found: Vec::new(),
            removed: Vec::new(),
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
    fn run(shared: Arc<Mutex<Self>>) {
        let ports = {
            let controller = shared.lock();
            for port in 1..=controller.ports {
                let status = controller.port_status(port);
                if status & PORT_POWER == 0 {
                    controller.set_port_status(port, (status & PORT_PRESERVE) | PORT_POWER);
                }
            }
            controller.ports
        };
        task::sleep_ms(20);
        {
            let mut controller = shared.lock();
            for port in 1..=ports {
                let status = controller.port_status(port);
                controller.set_port_status(port, (status & PORT_PRESERVE) | (status & PORT_CHANGES));
                if status & PORT_CONNECTED != 0 {
                    controller.attach(port);
                }
            }
        }
        Self::update_disks(&shared);
        SETTLED.fetch_add(1, Ordering::Release);

        let mut keys = Vec::new();
        loop {
            let deadline = {
                let mut controller = shared.lock();
                while let Some(event) = controller.next_event() {
                    controller.handle(event, &mut keys);
                }
                let now = time::ticks();
                for device in controller.devices.values_mut() {
                    if let Some(keyboard) = &mut device.keyboard
                        && let Some(key) = keyboard.state.repeat(now)
                    {
                        keys.push(key);
                    }
                }
                // Sleep until an event, a key repeat, or (without
                // interrupts) the next poll; and look now and then anyway,
                // in case another thread took the event the interrupt was
                // for.
                let repeat = controller.devices.values().filter_map(|d| d.keyboard.as_ref()?.state.next_repeat()).min();
                let poll = if INTERRUPTS.load(Ordering::Relaxed) { now + 100 } else { now + 10 };
                repeat.map_or(poll, |repeat| repeat.min(poll))
            };
            keys.drain(..).for_each(keyboard::send_key);
            Self::update_disks(&shared);
            EVENTS.wait_until_deadline(Some(deadline), || SIGNAL.swap(false, Ordering::AcqRel).then_some(()));
        }
    }

    /// Registers the disks found and unregisters the ones removed (outside
    /// the controller's lock: registering reads from the disk).
    fn update_disks(shared: &Arc<Mutex<Self>>) {
        let (found, removed) = {
            let mut controller = shared.lock();
            (core::mem::take(&mut controller.found), core::mem::take(&mut controller.removed))
        };
        for device in removed {
            let names: Vec<((u64, u8), String)> =
                DISK_NAMES.lock().iter().filter(|((id, _), _)| *id == device).map(|(k, v)| (*k, v.clone())).collect();
            for (key, name) in names {
                DISK_NAMES.lock().remove(&key);
                block::unregister(&name);
                info!("usb: {name} removed");
            }
        }
        for disk in found {
            let name = block::next_sd_name();
            info!(
                "usb: {name}: {}, {} MiB{}",
                disk.model,
                (disk.blocks * u64::from(disk.block_size)) >> 20,
                if disk.block_size == 512 { String::new() } else { format!(" ({}-byte blocks)", disk.block_size) }
            );
            DISK_NAMES.lock().insert((disk.device, disk.lun), name.clone());
            block::register(Arc::new(UsbDisk::new(name, shared.clone(), disk)));
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
        if device.hub.as_ref().is_some_and(|hub| hub.endpoint == event.endpoint) {
            self.on_hub_change(event);
            return;
        }
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
    ///
    /// With interrupts, the controller's interrupt wakes the wait as soon as
    /// an event arrives (a disk does thousands of small transfers, so
    /// sleeping a timer tick per transfer would make it crawl). Events kept
    /// for later make sure the controller's thread looks at them.
    fn wait_event(&mut self, timeout_ms: u64, wanted: impl Fn(&Event) -> bool) -> Option<Event> {
        let deadline = time::ticks() + timeout_ms;
        let mut skipped = VecDeque::new();
        let result = loop {
            match self.next_event() {
                Some(event) if wanted(&event) => break Some(event),
                Some(event) => skipped.push_back(event),
                None if time::ticks() >= deadline => break None,
                None if INTERRUPTS.load(Ordering::Relaxed) => {
                    let this = &*self;
                    EVENTS.wait_until_deadline(Some(deadline.min(time::ticks() + 10)), || {
                        this.event_ready().then_some(())
                    });
                }
                None => task::sleep_ms(1),
            }
        };
        skipped.append(&mut self.pending);
        self.pending = skipped;
        if !self.pending.is_empty() {
            SIGNAL.store(true, Ordering::Release);
            EVENTS.wake_all();
        }
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
            // The device refused the request; endpoint 0 is halted until
            // the controller resets it (the device itself recovers at the
            // next setup packet).
            Some(event) if event.completion == COMPLETION_STALL => {
                let _ = self.clear_halt(slot, 1, None);
                Err(String::from("the device refused the request"))
            }
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
        let location = Location { root_port: port, route: 0, depth: 1, speed, parent: None, translator: None };
        self.enumerate(location).map(|_| ())
    }

    /// The slot context for a device: its place in the tree, speed, the
    /// number of endpoint contexts in use, and for a hub, its ports.
    fn slot_context(device: &Device, entries: u32) -> [u32; 3] {
        let location = &device.location;
        let mut words = [location.route | device.speed << 20 | entries << 27, u32::from(location.root_port) << 16, 0];
        if let Some((hub, port)) = location.translator {
            words[2] |= u32::from(hub) | u32::from(port) << 8;
        }
        if let Some(hub) = &device.hub {
            words[0] |= 1 << 26 | u32::from(hub.multiple_translators) << 25;
            words[1] |= u32::from(hub.ports) << 24;
            words[2] |= hub.think_time << 16;
        }
        words
    }

    /// Gives a newly reset device a slot and an address, reads its
    /// descriptors and starts a driver for it. Returns its slot.
    fn enumerate(&mut self, location: Location) -> Result<u8, String> {
        let speed = location.speed;
        let slot = (self.command(0, TRB_ENABLE_SLOT << 10)?.slot_id())?;
        let device = Device {
            id: NEXT_DEVICE.fetch_add(1, Ordering::Relaxed),
            port: location.root_port,
            speed,
            location,
            output: Dma::new(1).ok_or("out of memory")?,
            input: Dma::new(1).ok_or("out of memory")?,
            control: Ring::new().ok_or("out of memory")?,
            keyboard: None,
            storage: None,
            hub: None,
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
        for (index, word) in Self::slot_context(&device, 1).into_iter().enumerate() {
            input.write32(size + index * 4, word);
        }
        input.write32(2 * size + 4, 3 << 1 | ENDPOINT_CONTROL << 3 | packet_size << 16);
        input.write64(2 * size + 8, device.control.start());
        input.write32(2 * size + 16, 8);
        let input_phys = input.phys;
        self.devices.insert(slot, device);
        if let Err(error) = self.command(input_phys, TRB_ADDRESS_DEVICE << 10 | u32::from(slot) << 24) {
            self.forget(slot);
            return Err(error);
        }

        match self.identify(slot, speed, packet_size) {
            Ok(()) => Ok(slot),
            Err(error) => {
                self.forget(slot);
                Err(error)
            }
        }
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

        if let Some(hub) = descriptors.hub() {
            self.control_transfer(slot, SetupPacket::set_configuration(descriptors.configuration), None)?;
            // Protocol 2: a transaction translator per port.
            self.start_hub(slot, &hub, device_descriptor[6] == 2)?;
            info!("usb: {}: port {port}: {description}: hub", self.name);
        } else if let Some(keyboard) = descriptors.boot_keyboard() {
            self.control_transfer(slot, SetupPacket::set_configuration(descriptors.configuration), None)?;
            self.start_keyboard(slot, &keyboard)?;
            info!("usb: {}: port {port}: {description}: keyboard", self.name);
        } else if let Some(interface) = descriptors.storage() {
            self.control_transfer(slot, SetupPacket::set_configuration(descriptors.configuration), None)?;
            self.start_storage(slot, &interface)?;
            info!("usb: {}: port {port}: {description}: storage", self.name);
        } else {
            info!("usb: {}: port {port}: {description} (no driver)", self.name);
        }
        Ok(())
    }

    /// Configures endpoints of a device (adding to any configured before):
    /// each is (context index, type, packet size, interval as the context
    /// encodes it, ring).
    fn configure_endpoints(&mut self, slot: u8, endpoints: &[(u8, u32, u16, u32, u64)]) -> Result<(), String> {
        let size = self.context_size;
        let device = self.devices.get(&slot).ok_or("no such device")?;
        let input = &device.input;
        for offset in (0..size * 33).step_by(4) {
            input.write32(offset, 0);
        }
        let mut add = 1;
        // The slot context's entry count must cover the highest endpoint
        // configured so far (the device context remembers earlier ones).
        let mut last = (device.output.read32(0) >> 27) & 0x1F;
        for &(index, kind, packet_size, interval, ring) in endpoints {
            add |= 1 << index;
            last = last.max(u32::from(index));
            let context = (1 + usize::from(index)) * size;
            input.write32(context, interval << 16);
            input.write32(context + 4, 3 << 1 | kind << 3 | u32::from(packet_size) << 16);
            input.write64(context + 8, ring);
            // Average transfer size: small for interrupt endpoints, a few
            // KiB for bulk (the values the specification suggests).
            let average = if kind == ENDPOINT_INTERRUPT_IN { u32::from(packet_size) } else { 3072 };
            let payload = if kind == ENDPOINT_INTERRUPT_IN { u32::from(packet_size) } else { 0 };
            input.write32(context + 16, payload << 16 | average);
        }
        input.write32(4, add);
        for (index, word) in Self::slot_context(device, last).into_iter().enumerate() {
            input.write32(size + index * 4, word);
        }
        let phys = input.phys;
        self.command(phys, TRB_CONFIGURE_ENDPOINT << 10 | u32::from(slot) << 24)?;
        Ok(())
    }

    fn start_keyboard(&mut self, slot: u8, keyboard: &super::InterruptInterface) -> Result<(), String> {
        // Boot protocol, and reports only when something changes.
        self.control_transfer(slot, SetupPacket::class_interface_out(0x0B, 0, keyboard.interface, 0), None)?;
        let _ = self.control_transfer(slot, SetupPacket::class_interface_out(0x0A, 0, keyboard.interface, 0), None);

        // The interrupt endpoint. Its context index is 2n + 1 for IN
        // endpoint n.
        let speed = self.devices[&slot].speed;
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
        self.configure_endpoints(slot, &[(endpoint, ENDPOINT_INTERRUPT_IN, packet_size, interval, ring.start())])?;

        let buffer = Dma::new(1).ok_or("out of memory")?;
        let state = BootKeyboard::default();
        let interface = keyboard.interface;
        self.devices.get_mut(&slot).unwrap().keyboard =
            Some(KeyboardEndpoint { interface, endpoint, ring, buffer, packet_size, state });
        let leds = SetupPacket::class_interface_out(0x09, 0x0200, interface, 1);
        let _ = self.control_transfer(slot, leds, Some(&mut [0x01]));
        self.queue_report(slot);
        KEYBOARDS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Sets up a mass storage device: its bulk endpoints, then, for each
    /// logical unit (card readers have several), whether there's a disk and
    /// how big it is.
    fn start_storage(&mut self, slot: u8, interface: &StorageInterface) -> Result<(), String> {
        let input_index = (interface.input_address & 0xF) * 2 + 1;
        let output_index = (interface.output_address & 0xF) * 2;
        let input_ring = Ring::new().ok_or("out of memory")?;
        let output_ring = Ring::new().ok_or("out of memory")?;
        self.configure_endpoints(
            slot,
            &[
                (output_index, ENDPOINT_BULK_OUT, interface.output_packet_size, 0, output_ring.start()),
                (input_index, ENDPOINT_BULK_IN, interface.input_packet_size, 0, input_ring.start()),
            ],
        )?;
        let buffer = frames::allocate_contiguous(STORAGE_BUFFER_PAGES, STORAGE_BUFFER_PAGES).ok_or("out of memory")?;
        let storage = StorageEndpoints {
            interface: interface.interface,
            input_index,
            output_index,
            input_address: interface.input_address,
            output_address: interface.output_address,
            input_ring,
            output_ring,
            wrappers: Dma::new(1).ok_or("out of memory")?,
            buffer: Dma { phys: buffer },
            tag: 0,
        };
        let device = self.devices.get_mut(&slot).ok_or("no such device")?;
        device.storage = Some(storage);
        let id = device.id;

        // GET MAX LUN; devices with one unit may stall it.
        let request = SetupPacket {
            request_type: 0xA1,
            request: 0xFE,
            value: 0,
            index: u16::from(interface.interface),
            length: 1,
        };
        let mut luns = [0u8];
        let count = match self.control_transfer(slot, request, Some(&mut luns)) {
            Ok(1) => luns[0].min(7) + 1,
            _ => 1,
        };
        for lun in 0..count {
            match self.probe_unit(slot, lun) {
                Ok(Some((block_size, blocks, model))) => {
                    self.found.push(Found { slot, device: id, lun, block_size, blocks, model });
                }
                Ok(None) => {}
                Err(error) => debug!("usb: {}: unit {lun}: {error}", self.name),
            }
        }
        Ok(())
    }

    /// Sets up a hub: tells the controller it's a hub (and how many ports
    /// it has), powers its ports, finds what's plugged in, and starts
    /// listening for changes.
    fn start_hub(
        &mut self,
        slot: u8,
        interface: &InterruptInterface,
        multiple_translators: bool,
    ) -> Result<(), String> {
        let device = self.devices.get(&slot).ok_or("no such device")?;
        let (speed, depth) = (device.speed, device.location.depth);
        if depth >= MAX_DEPTH {
            return Err("hubs are chained too deep".into());
        }
        // USB 3 hubs have their own descriptor type and need to know how
        // deep they are (for routing).
        let super_speed = speed >= SPEED_SUPER;
        let mut descriptor = [0u8; 12];
        let kind = if super_speed { 0x2A00 } else { 0x2900 };
        let request = SetupPacket { request_type: 0xA0, request: 6, value: kind, index: 0, length: 12 };
        self.control_transfer(slot, request, Some(&mut descriptor))?;
        let ports = descriptor[2].min(15);
        let characteristics = u16::from_le_bytes([descriptor[3], descriptor[4]]);
        let power_good_ms = u64::from(descriptor[5]) * 2;
        if super_speed {
            let request =
                SetupPacket { request_type: 0x20, request: 12, value: u16::from(depth - 1), index: 0, length: 0 };
            self.control_transfer(slot, request, None)?;
        }

        let ring = Ring::new().ok_or("out of memory")?;
        let buffer = Dma::new(1).ok_or("out of memory")?;
        let endpoint = (interface.address & 0xF) * 2 + 1;
        let packet_size = interface.packet_size.clamp(1, 64);
        let interval = if speed >= SPEED_HIGH {
            u32::from(interface.interval.clamp(1, 16) - 1)
        } else {
            let microframes = u32::from(interface.interval.max(1)) * 8;
            (31 - microframes.leading_zeros()).clamp(3, 10)
        };
        let ring_start = ring.start();
        self.devices.get_mut(&slot).ok_or("no such device")?.hub = Some(HubState {
            ports,
            think_time: u32::from(characteristics >> 5 & 0x3),
            multiple_translators: multiple_translators && speed == SPEED_HIGH,
            endpoint,
            ring,
            buffer,
            packet_size,
        });
        self.configure_endpoints(slot, &[(endpoint, ENDPOINT_INTERRUPT_IN, packet_size, interval, ring_start)])?;

        for port in 1..=ports {
            self.hub_request(slot, 3, FEATURE_PORT_POWER, port)?;
        }
        task::sleep_ms(power_good_ms.max(20));
        for port in 1..=ports {
            self.hub_port_changed(slot, port);
        }
        self.queue_hub_status(slot);
        Ok(())
    }

    /// SET_FEATURE (3) or CLEAR_FEATURE (1) for a hub port.
    fn hub_request(&mut self, slot: u8, request: u8, feature: u16, port: u8) -> Result<(), String> {
        let packet = SetupPacket { request_type: 0x23, request, value: feature, index: u16::from(port), length: 0 };
        self.control_transfer(slot, packet, None).map(|_| ())
    }

    /// A hub port's status and change bits.
    fn hub_port_status(&mut self, slot: u8, port: u8) -> Result<(u16, u16), String> {
        let mut status = [0u8; 4];
        let packet = SetupPacket { request_type: 0xA3, request: 0, value: 0, index: u16::from(port), length: 4 };
        self.control_transfer(slot, packet, Some(&mut status))?;
        Ok((u16::from_le_bytes([status[0], status[1]]), u16::from_le_bytes([status[2], status[3]])))
    }

    /// Asks a hub which ports changed (its interrupt endpoint answers when
    /// one does).
    fn queue_hub_status(&mut self, slot: u8) {
        let Some(hub) = self.devices.get_mut(&slot).and_then(|d| d.hub.as_mut()) else { return };
        let control = TRB_NORMAL << 10 | TRB_INTERRUPT_ON_COMPLETION | TRB_INTERRUPT_ON_SHORT;
        hub.ring.push(hub.buffer.phys, u32::from(hub.packet_size), control);
        let endpoint = hub.endpoint;
        self.ring_doorbell(slot, endpoint);
    }

    /// A hub said some ports changed: a bitmap, bit n for port n.
    fn on_hub_change(&mut self, event: Event) {
        let slot = event.slot;
        let Some(hub) = self.devices.get(&slot).and_then(|d| d.hub.as_ref()) else { return };
        if matches!(event.completion, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET) {
            let length = usize::from(hub.packet_size).saturating_sub(event.residual as usize).min(4);
            let mut bitmap = [0u8; 4];
            // SAFETY: the controller wrote `length` bytes into the buffer.
            unsafe { ptr::copy_nonoverlapping(hub.buffer.pointer::<u8>(0), bitmap.as_mut_ptr(), length) };
            let ports = hub.ports;
            for port in 1..=ports {
                if bitmap[usize::from(port / 8)] & (1 << (port % 8)) != 0 {
                    self.hub_port_changed(slot, port);
                }
            }
        }
        self.queue_hub_status(slot);
    }

    /// Looks at a hub port: acknowledges its changes, and attaches or
    /// detaches whatever is (or was) plugged in.
    fn hub_port_changed(&mut self, hub: u8, port: u8) {
        if let Err(error) = self.try_hub_port(hub, port) {
            warn!("usb: {}: hub {hub} port {port}: {error}", self.name);
        }
    }

    fn try_hub_port(&mut self, hub: u8, port: u8) -> Result<(), String> {
        let (status, change) = self.hub_port_status(hub, port)?;
        let super_speed = self.devices.get(&hub).is_some_and(|d| d.speed >= SPEED_SUPER);
        // Acknowledge each change: the feature to clear for change bit n.
        let features: &[(u16, u16)] = if super_speed {
            &[(0, 16), (3, 19), (4, 20), (5, 29), (6, 25), (7, 26)]
        } else {
            &[(0, 16), (1, 17), (2, 18), (3, 19), (4, 20)]
        };
        for &(bit, feature) in features {
            if change & (1 << bit) != 0 {
                self.hub_request(hub, 1, feature, port)?;
            }
        }
        let child = self.devices.iter().find(|(_, d)| d.location.parent == Some((hub, port))).map(|(&slot, _)| slot);
        let connected = status & 1 != 0;
        if let Some(child) = child
            && (!connected || change & 1 != 0)
        {
            self.detach_subtree(child);
        }
        if !connected || (child.is_some() && change & 1 == 0) {
            return Ok(());
        }

        // Reset the port, which enables it and tells us the speed.
        self.hub_request(hub, 3, FEATURE_PORT_RESET, port)?;
        let mut reset_done = false;
        for _ in 0..50 {
            task::sleep_ms(10);
            let (_, change) = self.hub_port_status(hub, port)?;
            if change & (1 << 4) != 0 {
                reset_done = true;
                break;
            }
        }
        if !reset_done {
            return Err("port reset timed out".into());
        }
        self.hub_request(hub, 1, FEATURE_C_PORT_RESET, port)?;
        let (status, _) = self.hub_port_status(hub, port)?;
        if status & 2 == 0 {
            return Err("port didn't enable".into());
        }
        task::sleep_ms(10);

        let parent = self.devices.get(&hub).ok_or("the hub is gone")?;
        let speed = if super_speed {
            SPEED_SUPER
        } else if status & (1 << 9) != 0 {
            SPEED_LOW
        } else if status & (1 << 10) != 0 {
            SPEED_HIGH
        } else {
            SPEED_FULL
        };
        // A slow device behind a high-speed hub is reached through that
        // hub's transaction translator (or its parent's).
        let translator = match (speed, parent.speed) {
            (SPEED_LOW | SPEED_FULL, SPEED_HIGH) => Some((hub, port)),
            (SPEED_LOW | SPEED_FULL, _) => parent.location.translator,
            _ => None,
        };
        let location = Location {
            root_port: parent.location.root_port,
            route: parent.location.route | u32::from(port) << (4 * u32::from(parent.location.depth - 1)),
            depth: parent.location.depth + 1,
            speed,
            parent: Some((hub, port)),
            translator,
        };
        self.enumerate(location).map(|_| ())
    }

    /// Asks a logical unit what it is: a disk's (block size, blocks, model),
    /// or `None` if it isn't a disk or has no medium.
    fn probe_unit(&mut self, slot: u8, lun: u8) -> Result<Option<(u32, u64, String)>, String> {
        let mut inquiry = [0u8; 36];
        let command = [storage::INQUIRY, 0, 0, 0, 36, 0];
        if self.storage_command(slot, lun, &command, &mut inquiry, false)? != Status::Passed {
            return Ok(None);
        }
        // Direct access (0) or simplified direct access (0xE): disks.
        if !matches!(inquiry[0] & 0x1F, 0x00 | 0x0E) {
            return Ok(None);
        }
        let text = |bytes: &[u8]| String::from(String::from_utf8_lossy(bytes).trim());
        let model = format!("{} {}", text(&inquiry[8..16]), text(&inquiry[16..32])).trim().into();

        // A disk spinning up, or a card reader noticing its card, reports
        // "not ready" for a while.
        let attempts = if lun == 0 { 50 } else { 3 };
        let mut ready = false;
        for _ in 0..attempts {
            if self.storage_command(slot, lun, &[storage::TEST_UNIT_READY, 0, 0, 0, 0, 0], &mut [], false)?
                == Status::Passed
            {
                ready = true;
                break;
            }
            let mut sense = [0u8; 18];
            let _ = self.storage_command(slot, lun, &[storage::REQUEST_SENSE, 0, 0, 0, 18, 0], &mut sense, false);
            task::sleep_ms(100);
        }
        if !ready {
            return Ok(None);
        }

        let mut capacity = [0u8; 8];
        let command = [storage::READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        if self.storage_command(slot, lun, &command, &mut capacity, false)? != Status::Passed {
            return Err("READ CAPACITY failed".into());
        }
        let last = u32::from_be_bytes(capacity[0..4].try_into().unwrap());
        let mut block_size = u32::from_be_bytes(capacity[4..8].try_into().unwrap());
        let mut blocks = u64::from(last) + 1;
        if last == u32::MAX {
            // Over 2 TiB: the 16-byte form.
            let mut long = [0u8; 32];
            let mut command = [0u8; 16];
            command[0] = storage::SERVICE_ACTION_IN;
            command[1] = storage::READ_CAPACITY_16;
            command[13] = 32;
            if self.storage_command(slot, lun, &command, &mut long, false)? != Status::Passed {
                return Err("READ CAPACITY (16) failed".into());
            }
            blocks = u64::from_be_bytes(long[0..8].try_into().unwrap()) + 1;
            block_size = u32::from_be_bytes(long[8..12].try_into().unwrap());
        }
        if ![512, 1024, 2048, 4096].contains(&block_size) {
            return Err(format!("unsupported block size {block_size}"));
        }
        Ok(Some((block_size, blocks, model)))
    }

    /// One bulk transfer, in or out, through the device's data buffer at
    /// `phys`. Returns the bytes moved, or the completion code it failed
    /// with.
    fn bulk(&mut self, slot: u8, index: u8, phys: u64, length: usize) -> Result<usize, u8> {
        let device = self.devices.get_mut(&slot).ok_or(0)?;
        let storage = device.storage.as_mut().ok_or(0)?;
        let input = index == storage.input_index;
        let ring = if input { &mut storage.input_ring } else { &mut storage.output_ring };
        let flags = if input { TRB_INTERRUPT_ON_SHORT } else { 0 };
        ring.push(phys, length as u32, TRB_NORMAL << 10 | TRB_INTERRUPT_ON_COMPLETION | flags);
        self.ring_doorbell(slot, index);
        // Writes to slow flash can take a while.
        let event = self
            .wait_event(20_000, |e| e.kind == TRB_TRANSFER_EVENT && e.slot == slot && e.endpoint == index)
            .ok_or(0)?;
        match event.completion {
            COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET => Ok(length.saturating_sub(event.residual as usize)),
            code => Err(code),
        }
    }

    /// Recovers a stalled (halted) endpoint: the controller resets its
    /// side and skips what was queued, and the device is told to clear the
    /// halt (for endpoints other than 0).
    fn clear_halt(&mut self, slot: u8, index: u8, address: Option<u8>) -> Result<(), String> {
        self.command(0, TRB_RESET_ENDPOINT << 10 | u32::from(slot) << 24 | u32::from(index) << 16)?;
        let device = self.devices.get_mut(&slot).ok_or("no such device")?;
        let ring = match (index, device.storage.as_mut()) {
            (1, _) => &mut device.control,
            (index, Some(storage)) if index == storage.input_index => &mut storage.input_ring,
            (_, Some(storage)) => &mut storage.output_ring,
            _ => return Err("no such endpoint".into()),
        };
        let dequeue = (ring.memory.phys + ring.enqueue as u64 * 16) | u64::from(ring.cycle);
        self.command(dequeue, TRB_SET_TR_DEQUEUE << 10 | u32::from(slot) << 24 | u32::from(index) << 16)?;
        if let Some(address) = address {
            // CLEAR_FEATURE(ENDPOINT_HALT) to the endpoint.
            let request =
                SetupPacket { request_type: 0x02, request: 1, value: 0, index: u16::from(address), length: 0 };
            self.control_transfer(slot, request, None)?;
        }
        Ok(())
    }

    /// Runs a SCSI command on logical unit `lun`, moving `data.len()` bytes
    /// (in, unless `write`).
    fn storage_command(
        &mut self,
        slot: u8,
        lun: u8,
        command: &[u8],
        data: &mut [u8],
        write: bool,
    ) -> Result<Status, String> {
        let device = self.devices.get_mut(&slot).ok_or("the device is gone")?;
        let storage = device.storage.as_mut().ok_or("not a storage device")?;
        storage.tag = storage.tag.wrapping_add(1);
        let tag = storage.tag;
        let (input_index, output_index) = (storage.input_index, storage.output_index);
        let (input_address, output_address) = (storage.input_address, storage.output_address);
        let (wrappers, buffer) = (storage.wrappers.phys, storage.buffer.phys);
        let wrapper = storage::command_block(tag, data.len() as u32, !write && !data.is_empty(), lun, command);
        // SAFETY: the wrappers page is ours; the controller isn't using it.
        unsafe { ptr::copy_nonoverlapping(wrapper.as_ptr(), phys_to_virt(wrappers), wrapper.len()) };
        if let Err(code) = self.bulk(slot, output_index, wrappers, storage::CBW_LENGTH) {
            self.reset_storage(slot)?;
            return Err(format!("sending the command failed (completion code {code})"));
        }

        if !data.is_empty() {
            let (index, address) = if write { (output_index, output_address) } else { (input_index, input_address) };
            if write {
                // SAFETY: at most the buffer's 64 KiB (callers split).
                unsafe { ptr::copy_nonoverlapping(data.as_ptr(), phys_to_virt(buffer), data.len()) };
            }
            match self.bulk(slot, index, buffer, data.len()) {
                Ok(moved) if !write => {
                    // SAFETY: the controller wrote `moved` bytes.
                    unsafe { ptr::copy_nonoverlapping(phys_to_virt(buffer), data.as_mut_ptr(), moved.min(data.len())) };
                }
                Ok(_) => {}
                // A stall here means the device refused the data; the
                // status still follows.
                Err(COMPLETION_STALL) => self.clear_halt(slot, index, Some(address))?,
                Err(code) => {
                    self.reset_storage(slot)?;
                    return Err(format!("the data transfer failed (completion code {code})"));
                }
            }
        }

        let status_at = wrappers + 512;
        let mut result = self.bulk(slot, input_index, status_at, storage::CSW_LENGTH);
        if result == Err(COMPLETION_STALL) {
            self.clear_halt(slot, input_index, Some(input_address))?;
            result = self.bulk(slot, input_index, status_at, storage::CSW_LENGTH);
        }
        if let Err(code) = result {
            self.reset_storage(slot)?;
            return Err(format!("reading the status failed (completion code {code})"));
        }
        // SAFETY: the controller wrote the status wrapper there.
        let status = unsafe { core::slice::from_raw_parts(phys_to_virt(status_at), storage::CSW_LENGTH) };
        match storage::command_status(status, tag) {
            Some(Status::PhaseError) | None => {
                self.reset_storage(slot)?;
                Err("the device lost track of the command".into())
            }
            Some(status) => Ok(status),
        }
    }

    /// The Bulk-Only "reset recovery": a class reset of the interface, then
    /// both endpoints' halts cleared.
    fn reset_storage(&mut self, slot: u8) -> Result<(), String> {
        let storage = self.devices.get(&slot).and_then(|d| d.storage.as_ref()).ok_or("not a storage device")?;
        let (interface, input, output) = (storage.interface, storage.input_index, storage.output_index);
        let (input_address, output_address) = (storage.input_address, storage.output_address);
        let reset = SetupPacket { request_type: 0x21, request: 0xFF, value: 0, index: u16::from(interface), length: 0 };
        self.control_transfer(slot, reset, None)?;
        self.clear_halt(slot, input, Some(input_address))?;
        self.clear_halt(slot, output, Some(output_address))
    }

    /// Reads or writes blocks of a USB disk (for [`UsbDisk`]).
    #[allow(clippy::too_many_arguments, reason = "one disk request, as its parts")]
    pub fn storage_io(
        &mut self,
        slot: u8,
        device: u64,
        lun: u8,
        block: u64,
        block_size: u32,
        data: &mut [u8],
        write: bool,
    ) -> Result<(), &'static str> {
        if self.devices.get(&slot).is_none_or(|d| d.id != device) {
            return Err("the USB disk was removed");
        }
        let per_command = STORAGE_BUFFER_PAGES * PAGE_SIZE as usize / block_size as usize;
        for (index, chunk) in data.chunks_mut(per_command * block_size as usize).enumerate() {
            let first = block + (index * per_command) as u64;
            let count = (chunk.len() / block_size as usize) as u32;
            let command = storage::read_write(write, first, count);
            // One retry: a device may report a passing condition (like
            // "medium changed") on the first command after an event.
            let mut status = self.storage_command(slot, lun, &command, chunk, write);
            if status.as_ref().is_ok_and(|status| *status == Status::Failed) {
                let mut sense = [0u8; 18];
                let _ = self.storage_command(slot, lun, &[storage::REQUEST_SENSE, 0, 0, 0, 18, 0], &mut sense, false);
                status = self.storage_command(slot, lun, &command, chunk, write);
            }
            match status {
                Ok(Status::Passed) => {}
                Ok(_) => return Err("the USB disk reported an error"),
                Err(error) => {
                    warn!("usb: disk I/O failed: {error}");
                    return Err("USB transfer failed");
                }
            }
        }
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

    /// Removes every device on a root port (with hubs, there can be many).
    fn detach(&mut self, port: u8) {
        let slots: Vec<u8> = self.devices.iter().filter(|(_, d)| d.port == port).map(|(&slot, _)| slot).collect();
        self.remove_devices(port, slots);
    }

    /// Removes a device and everything plugged into it, if it's a hub.
    fn detach_subtree(&mut self, slot: u8) {
        let mut slots = alloc::vec![slot];
        let mut index = 0;
        while index < slots.len() {
            let parent = slots[index];
            slots.extend(
                self.devices
                    .iter()
                    .filter(|(_, d)| d.location.parent.is_some_and(|(hub, _)| hub == parent))
                    .map(|(&s, _)| s),
            );
            index += 1;
        }
        let port = self.devices.get(&slot).map_or(0, |device| device.port);
        self.remove_devices(port, slots);
    }

    fn remove_devices(&mut self, port: u8, slots: Vec<u8>) {
        for slot in slots {
            if self.devices[&slot].keyboard.is_some() {
                KEYBOARDS.fetch_sub(1, Ordering::Relaxed);
            }
            let _ = self.command(0, TRB_DISABLE_SLOT << 10 | u32::from(slot) << 24);
            self.forget(slot);
            info!("usb: {}: port {port}: device removed", self.name);
        }
    }

    /// Drops what we know about a device and frees its memory. Its disks
    /// are unregistered when the controller's lock is next released.
    fn forget(&mut self, slot: u8) {
        self.device_contexts.write64(usize::from(slot) * 8, 0);
        if let Some(device) = self.devices.remove(&slot) {
            let mut pages = alloc::vec![device.output.phys, device.input.phys, device.control.memory.phys];
            if let Some(keyboard) = device.keyboard {
                pages.extend([keyboard.ring.memory.phys, keyboard.buffer.phys]);
            }
            if let Some(hub) = device.hub {
                pages.extend([hub.ring.memory.phys, hub.buffer.phys]);
            }
            if let Some(storage) = device.storage {
                pages.extend([storage.input_ring.memory.phys, storage.output_ring.memory.phys, storage.wrappers.phys]);
                pages.extend((0..STORAGE_BUFFER_PAGES as u64).map(|page| storage.buffer.phys + page * PAGE_SIZE));
                self.removed.push(device.id);
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
