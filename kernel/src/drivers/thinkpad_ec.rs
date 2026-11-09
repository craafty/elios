//! The battery and charger of ThinkPads, read from the embedded
//! controller.
//!
//! On a laptop the battery is the embedded controller's (EC's) business:
//! a small microcontroller that also runs the fan, the keyboard lights
//! and the power button. Operating systems normally learn about the
//! battery by running the firmware's ACPI methods (`_BST`, `_BIF`), which
//! are AML bytecode that reads the EC's registers. eliOS has no AML
//! interpreter, so it reads those registers itself, at the places the
//! ThinkPad EC firmware (Lenovo's "H8", the same across a decade of
//! models, including the T440p) keeps them, as coreboot's ACPI code for
//! these machines documents:
//!
//! - 0x38: battery 0's state (bit 4 charging, bit 5 discharging, bit 7
//!   present); 0x46 bit 4: the charger is plugged in, bit 2: the lid is
//!   open.
//! - 0x81: a page selector for the 16 bytes at 0xA0 (battery number in
//!   the top four bits, page in the bottom). Page 0: remaining and full
//!   capacity, present rate (signed), voltage (mV); page 1, bit 15 of
//!   0xA0: capacities are in 10 mWh rather than mAh; page 2: design
//!   capacity.
//!
//! The EC is spoken to through two I/O ports with ACPI's standard
//! protocol (read command 0x80, write 0x81), waiting on the status port's
//! input-full and output-full bits. Only Lenovo firmware is trusted to
//! have this layout; elsewhere nothing is touched.
//!
//! A thread reads the battery every few seconds. When the battery runs
//! down to 3% on its own, it shuts down as `poweroff` does (disks
//! written and marked clean), as Linux desktops do. Only after watching it
//! fall there, though: a battery that already reads empty at boot may be
//! a layout this doesn't understand, and powering off then would make the
//! machine unusable. The lid is looked at twice a second, and the
//! backlight turned off while it's closed (but only once it has been seen
//! open, so a misread can't darken the screen of a machine in use).
//! Programs see the battery as
//! Linux's `/sys/class/power_supply/BAT0` and `AC` (see `fs/sysfs.rs`).

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::arch::{inb, outb};
use crate::sync::SpinLock;
use crate::{acpi, task, time};

const DEFAULT_COMMAND_PORT: u16 = 0x66;
const DEFAULT_DATA_PORT: u16 = 0x62;
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const READ: u8 = 0x80;
const WRITE: u8 = 0x81;

const BATTERY_STATE: u8 = 0x38;
const CHARGER: u8 = 0x46;
const PAGE: u8 = 0x81;
const PAGE_DATA: u8 = 0xA0;

const READ_EVERY_MS: u64 = 5000;
/// The lid is looked at more often, so the screen goes dark promptly.
const LID_EVERY_MS: u64 = 500;
/// 0x46 bit 2: the lid is open.
const LID_OPEN: u8 = 1 << 2;
/// At or below this percentage on battery, the machine is powered off
/// cleanly before it dies with the disks half written.
const CRITICAL_PERCENT: u32 = 3;
/// Readings in a row that must agree before that happens.
const CRITICAL_READINGS: u32 = 3;
/// How long the EC may take with one byte. It usually answers within
/// microseconds; the firmware's own code allows a millisecond or more.
const TIMEOUT_US: u64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Charging,
    Discharging,
    /// Plugged in and not charging (full, or held below a threshold).
    Idle,
}

#[derive(Debug, Clone, Copy)]
pub struct Battery {
    pub state: State,
    /// In mWh (or mAh when `in_mwh` is false; the percentage doesn't care).
    pub remaining: u32,
    pub full: u32,
    pub design: u32,
    pub in_mwh: bool,
    /// mW (or mA), always positive.
    pub rate: u32,
    pub millivolts: u32,
}

impl Battery {
    pub fn percent(&self) -> u32 {
        (self.remaining.min(self.full) * 100 + self.full / 2).checked_div(self.full).unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
pub struct Reading {
    /// `None` when no battery is in.
    pub battery: Option<Battery>,
    pub plugged_in: bool,
}

static READING: SpinLock<Option<Reading>> = SpinLock::new(None);
static TRANSACTION: SpinLock<()> = SpinLock::new(());
/// The lid: 0 not known, 1 open, 2 closed.
static LID: AtomicU8 = AtomicU8::new(0);

/// The last reading, if the machine has a ThinkPad EC.
pub fn reading() -> Option<Reading> {
    *READING.lock()
}

/// Whether the lid is open, if the machine has a ThinkPad EC.
pub fn lid_open() -> Option<bool> {
    match LID.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

pub fn probe() {
    let maker = acpi::maker();
    if maker != "LENOVO" {
        debug!("thinkpad-ec: firmware by {maker:?}, not Lenovo: no battery readings");
        return;
    }
    let (command, data) = acpi::embedded_controller().unwrap_or((DEFAULT_COMMAND_PORT, DEFAULT_DATA_PORT));
    let ec = Ec { command, data };
    // SAFETY: reading the EC's status port has no effect.
    let status = unsafe { inb(command) };
    info!("thinkpad-ec: ports {command:#x}/{data:#x}, status {status:#04x}");
    if status == 0xFF {
        warn!("thinkpad-ec: nothing answers at the EC's ports");
        return;
    }
    task::spawn("thinkpad-ec", move || ec.watch());
}

struct Ec {
    command: u16,
    data: u16,
}

impl Ec {
    fn wait(&self, ready: impl Fn(u8) -> bool) -> Result<(), u8> {
        let start = time::uptime_nanos();
        loop {
            // SAFETY: the EC's status port, read-only.
            let status = unsafe { inb(self.command) };
            if ready(status) {
                return Ok(());
            }
            if time::uptime_nanos() - start > TIMEOUT_US * 1000 {
                return Err(status);
            }
            core::hint::spin_loop();
        }
    }

    fn input_free(&self) -> Result<(), u8> {
        self.wait(|status| status & STATUS_INPUT_FULL == 0)
    }

    fn read(&self, address: u8) -> Result<u8, String> {
        let fail = |step: &str, status: u8| format!("reading {address:#04x}: {step} (status {status:#04x})");
        // With interrupts masked (the spinlock does that) nothing on this
        // CPU comes between the steps; the EC answers in microseconds.
        let _quiet = TRANSACTION.lock();
        self.input_free().map_err(|s| fail("busy", s))?;
        // A stale byte left by someone else would be taken for the answer.
        // SAFETY: the EC's ports, with ACPI's EC protocol.
        unsafe {
            if inb(self.command) & STATUS_OUTPUT_FULL != 0 {
                inb(self.data);
            }
            outb(self.command, READ);
        }
        self.input_free().map_err(|s| fail("command not taken", s))?;
        // SAFETY: as above.
        unsafe { outb(self.data, address) };
        self.wait(|status| status & STATUS_OUTPUT_FULL != 0).map_err(|s| fail("no answer", s))?;
        // SAFETY: as above.
        Ok(unsafe { inb(self.data) })
    }

    fn write(&self, address: u8, value: u8) -> Result<(), String> {
        let fail = |step: &str, status: u8| format!("writing {address:#04x}: {step} (status {status:#04x})");
        let _quiet = TRANSACTION.lock();
        self.input_free().map_err(|s| fail("busy", s))?;
        // SAFETY: the EC's ports, with ACPI's EC protocol.
        unsafe { outb(self.command, WRITE) };
        self.input_free().map_err(|s| fail("command not taken", s))?;
        // SAFETY: as above.
        unsafe { outb(self.data, address) };
        self.input_free().map_err(|s| fail("address not taken", s))?;
        // SAFETY: as above.
        unsafe { outb(self.data, value) };
        self.input_free().map_err(|s| fail("value not taken", s))
    }

    fn watch(self) {
        let mut first = true;
        let mut failures = 0u32;
        let mut logged: Option<(Option<State>, u32, bool)> = None;
        let mut logged_at = 0;
        // Whether the battery has been seen above critical, and critical
        // readings in a row since.
        let mut seen_above_critical = false;
        let mut critical = 0;
        // The lid is only acted on once it has been seen open: closed at
        // boot means this machine keeps it somewhere else (the owner is
        // looking at the screen), and darkening it would be a disaster.
        let mut seen_lid_open = false;
        let mut since_battery = READ_EVERY_MS;
        loop {
            match self.read(CHARGER) {
                Ok(value) => {
                    let open = value & LID_OPEN != 0;
                    let was = LID.swap(if open { 1 } else { 2 }, Ordering::Relaxed);
                    if was != if open { 1 } else { 2 } {
                        info!("thinkpad-ec: lid {} ({value:#04x})", if open { "open" } else { "closed" });
                    }
                    seen_lid_open |= open;
                    if seen_lid_open {
                        crate::drivers::intel_display::set_lid_closed(!open);
                    }
                }
                Err(error) => debug!("thinkpad-ec: lid: {error}"),
            }
            task::sleep_ms(LID_EVERY_MS);
            since_battery += LID_EVERY_MS;
            if since_battery < READ_EVERY_MS {
                continue;
            }
            since_battery = 0;
            match measure(&self, first) {
                Ok(reading) => {
                    failures = 0;
                    *READING.lock() = Some(reading);
                    // Logged when something changes, the percentage in
                    // steps of 5, and every ten minutes.
                    let now = time::uptime_nanos() / 1_000_000_000;
                    let key = (
                        reading.battery.map(|b| b.state),
                        reading.battery.map_or(0, |b| b.percent() / 5),
                        reading.plugged_in,
                    );
                    if logged != Some(key) || now - logged_at >= 600 {
                        logged = Some(key);
                        logged_at = now;
                        log(&reading);
                    }
                    match reading.battery {
                        Some(battery) if battery.percent() > CRITICAL_PERCENT || reading.plugged_in => {
                            seen_above_critical |= battery.percent() > CRITICAL_PERCENT;
                            critical = 0;
                        }
                        Some(battery) if battery.state == State::Discharging && seen_above_critical => {
                            critical += 1;
                            if critical >= CRITICAL_READINGS {
                                warn!("battery: {}% left: powering off", battery.percent());
                                crate::logfile::flush();
                                crate::fs::mount::shut_down();
                                crate::power::power_off();
                            }
                        }
                        _ => critical = 0,
                    }
                }
                Err(error) => {
                    failures += 1;
                    if failures <= 3 || failures.is_power_of_two() {
                        warn!("thinkpad-ec: {error} (failure {failures})");
                    }
                }
            }
            first = false;
        }
    }
}

/// The EC's registers (the real one, or a test's).
trait Registers {
    fn read(&self, address: u8) -> Result<u8, String>;
    fn write(&self, address: u8, value: u8) -> Result<(), String>;
}

impl Registers for Ec {
    fn read(&self, address: u8) -> Result<u8, String> {
        Ec::read(self, address)
    }

    fn write(&self, address: u8, value: u8) -> Result<(), String> {
        Ec::write(self, address, value)
    }
}

/// The 16 bytes of a battery information page, as little-endian words.
fn page(ec: &impl Registers, page: u8) -> Result<[u16; 8], String> {
    ec.write(PAGE, page)?;
    let mut words = [0; 8];
    for (index, word) in words.iter_mut().enumerate() {
        let address = PAGE_DATA + 2 * index as u8;
        *word = u16::from_le_bytes([ec.read(address)?, ec.read(address + 1)?]);
    }
    Ok(words)
}

fn measure(ec: &impl Registers, first: bool) -> Result<Reading, String> {
    let state = ec.read(BATTERY_STATE)?;
    let charger = ec.read(CHARGER)?;
    let plugged_in = charger & (1 << 4) != 0;
    if state & (1 << 7) == 0 {
        if first {
            info!("thinkpad-ec: state {state:#04x}, charger {charger:#04x}: no battery");
        }
        return Ok(Reading { battery: None, plugged_in });
    }
    let management = page(ec, 0x01)?;
    let design = page(ec, 0x02)?;
    let dynamic = page(ec, 0x00)?;
    if first {
        // Everything, in case the layout differs from what's expected.
        info!(
            "thinkpad-ec: state {state:#04x}, charger {charger:#04x}, page 0 {dynamic:04x?}, page 1 \
             {management:04x?}, page 2 {design:04x?}"
        );
    }
    let in_mwh = management[0] & 0x8000 != 0;
    let scale = if in_mwh { 10 } else { 1 };
    let charging = state & (1 << 4) != 0;
    let discharging = state & (1 << 5) != 0;
    // The rate is in mA either way: for mWh batteries it's turned into
    // mW with the voltage, as the firmware's own `_BST` does.
    let millivolts = u32::from(dynamic[5]);
    let milliamps = if charging || discharging { u32::from((dynamic[4] as i16).unsigned_abs()) } else { 0 };
    let rate = if in_mwh { milliamps * millivolts / 1000 } else { milliamps };
    let battery = Battery {
        state: if charging {
            State::Charging
        } else if discharging {
            State::Discharging
        } else {
            State::Idle
        },
        remaining: u32::from(dynamic[0]) * scale,
        full: u32::from(dynamic[1]) * scale,
        design: u32::from(design[0]) * scale,
        in_mwh,
        rate,
        millivolts,
    };
    if battery.full == 0 || battery.remaining > battery.full + battery.full / 4 {
        return Err(format!("implausible capacities {} of {}", battery.remaining, battery.full));
    }
    Ok(Reading { battery: Some(battery), plugged_in })
}

fn log(reading: &Reading) {
    let charger = if reading.plugged_in { "plugged in" } else { "unplugged" };
    match reading.battery {
        None => info!("battery: none, charger {charger}"),
        Some(battery) => {
            let unit = if battery.in_mwh { "mWh" } else { "mAh" };
            info!(
                "battery: {}%, {:?} at {} m{}, {} of {} {unit} (designed for {}), {} mV, charger {charger}",
                battery.percent(),
                battery.state,
                battery.rate,
                if battery.in_mwh { "W" } else { "A" },
                battery.remaining,
                battery.full,
                battery.design,
                battery.millivolts,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// Registers, with the 16 bytes at 0xA0 following the page selector.
    struct FakeEc {
        registers: SpinLock<[u8; 256]>,
        pages: SpinLock<BTreeMap<u8, [u8; 16]>>,
    }

    impl FakeEc {
        fn new(state: u8, charger: u8) -> Self {
            let mut registers = [0; 256];
            registers[usize::from(BATTERY_STATE)] = state;
            registers[usize::from(CHARGER)] = charger;
            Self { registers: SpinLock::new(registers), pages: SpinLock::new(BTreeMap::new()) }
        }

        fn set_page(&self, page: u8, words: &[u16; 8]) {
            let mut bytes = [0; 16];
            for (chunk, word) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(words) {
                *chunk = word.to_le_bytes();
            }
            self.pages.lock().insert(page, bytes);
        }
    }

    impl Registers for FakeEc {
        fn read(&self, address: u8) -> Result<u8, String> {
            let registers = self.registers.lock();
            if (PAGE_DATA..PAGE_DATA + 16).contains(&address) {
                let page = registers[usize::from(PAGE)];
                return Ok(self.pages.lock().get(&page).map_or(0, |bytes| bytes[usize::from(address - PAGE_DATA)]));
            }
            Ok(registers[usize::from(address)])
        }

        fn write(&self, address: u8, value: u8) -> Result<(), String> {
            self.registers.lock()[usize::from(address)] = value;
            Ok(())
        }
    }

    crate::ktest! {
        fn battery_percentage_rounds_and_caps() {
            let battery = Battery {
                state: State::Discharging,
                remaining: 2345,
                full: 4690,
                design: 5000,
                in_mwh: true,
                rate: 0,
                millivolts: 11000,
            };
            assert_eq!(battery.percent(), 50);
            assert_eq!(Battery { remaining: 4800, ..battery }.percent(), 100);
            assert_eq!(Battery { full: 0, ..battery }.percent(), 0);
        }

        fn battery_is_read_from_the_ec_pages() {
            // A T440p-like EC: discharging at 1.5 A and 11.1 V, 10 mWh
            // units, 2345 of 4690 (so 23450 of 46900 mWh).
            let ec = FakeEc::new(0b1010_0000, 0x00);
            ec.set_page(0x00, &[2345, 4690, 0, 0, (-1500i16) as u16, 11100, 0, 0]);
            ec.set_page(0x01, &[0x8000, 0, 0, 0, 0, 0, 0, 0]);
            ec.set_page(0x02, &[5000, 10800, 0, 0, 0, 0, 0, 0]);
            let reading = measure(&ec, false).unwrap();
            assert!(!reading.plugged_in);
            let battery = reading.battery.unwrap();
            assert_eq!(battery.state, State::Discharging);
            assert!(battery.in_mwh);
            assert_eq!((battery.remaining, battery.full, battery.design), (23450, 46900, 50000));
            assert_eq!((battery.rate, battery.millivolts), (16650, 11100));
            assert_eq!(battery.percent(), 50);
            // Plugged in and full: no rate, whatever the EC says.
            let ec = FakeEc::new(0b1000_0000, 1 << 4);
            ec.set_page(0x00, &[4690, 4690, 0, 0, 7, 12500, 0, 0]);
            ec.set_page(0x01, &[0, 0, 0, 0, 0, 0, 0, 0]);
            ec.set_page(0x02, &[5000, 10800, 0, 0, 0, 0, 0, 0]);
            let reading = measure(&ec, false).unwrap();
            assert!(reading.plugged_in);
            let battery = reading.battery.unwrap();
            assert_eq!((battery.state, battery.rate, battery.in_mwh, battery.full), (State::Idle, 0, false, 4690));
            // No battery.
            let ec = FakeEc::new(0, 1 << 4);
            assert!(measure(&ec, false).unwrap().battery.is_none());
            // Garbage is refused.
            let ec = FakeEc::new(0b1001_0000, 1 << 4);
            ec.set_page(0x00, &[0xFFFF, 100, 0, 0, 0, 0, 0, 0]);
            assert!(measure(&ec, false).is_err());
        }
    }
}
