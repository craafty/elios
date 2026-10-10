//! 16550 UART driver. COM1 is the kernel's log output until there is a screen
//! console, and stays the debug channel after that.

use core::fmt;

use crate::arch::interrupts::{self, IRQ_BASE, InterruptFrame};
use crate::arch::{apic, inb, outb};
use crate::sync::SpinLock;
use crate::tty;

const COM1: u16 = 0x3F8;

// Register offsets from the port base.
const DATA: u16 = 0;
const INTERRUPT_ENABLE: u16 = 1;
const FIFO_CONTROL: u16 = 2;
const LINE_CONTROL: u16 = 3;
const MODEM_CONTROL: u16 = 4;
const LINE_STATUS: u16 = 5;

const LINE_STATUS_TX_EMPTY: u8 = 1 << 5;

pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// 115200 baud, 8 data bits, no parity, one stop bit.
    fn init(&mut self) {
        // SAFETY: standard 16550 programming sequence on a port we own.
        unsafe {
            outb(self.base + INTERRUPT_ENABLE, 0x00);
            outb(self.base + LINE_CONTROL, 0x80); // expose the baud divisor
            outb(self.base + DATA, 0x01); // divisor 1 = 115200 baud
            outb(self.base + INTERRUPT_ENABLE, 0x00);
            outb(self.base + LINE_CONTROL, 0x03); // 8N1, hide the divisor
            outb(self.base + FIFO_CONTROL, 0xC7); // enable and clear FIFOs
            outb(self.base + MODEM_CONTROL, 0x0B); // DTR, RTS, OUT2
        }
    }

    fn write_byte(&mut self, byte: u8) {
        // SAFETY: polling the status register and writing data is how a
        // 16550 transmits.
        unsafe {
            while inb(self.base + LINE_STATUS) & LINE_STATUS_TX_EMPTY == 0 {
                core::hint::spin_loop();
            }
            outb(self.base + DATA, byte);
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

static PORT: SpinLock<SerialPort> = SpinLock::new(SerialPort::new(COM1));

pub fn init() {
    PORT.lock().init();
}

const SERIAL_IRQ: u8 = 4;
const LINE_STATUS_DATA_READY: u8 = 1 << 0;

/// Delivers bytes received on the serial port to the console terminal, so
/// it can be used from a serial terminal (or a script) as well as the
/// keyboard.
pub fn enable_input() -> Result<(), &'static str> {
    interrupts::set_handler(IRQ_BASE + SERIAL_IRQ, on_interrupt);
    // SAFETY: enables only the "data received" interrupt.
    unsafe { outb(COM1 + INTERRUPT_ENABLE, 0x01) };
    apic::route_isa_irq(SERIAL_IRQ, IRQ_BASE + SERIAL_IRQ)
}

fn on_interrupt(_frame: &mut InterruptFrame) {
    let mut bytes = [0; 16];
    let mut count = 0;
    // SAFETY: reading received bytes while the UART says it has some.
    while count < bytes.len() && unsafe { inb(COM1 + LINE_STATUS) } & LINE_STATUS_DATA_READY != 0 {
        bytes[count] = unsafe { inb(COM1 + DATA) };
        count += 1;
    }
    tty::input(&bytes[..count]);
}

pub fn write_fmt(args: fmt::Arguments) {
    use fmt::Write as _;
    let _ = PORT.lock().write_fmt(args);
}

/// A handle that bypasses the lock, for the panic handler: the panic may have
/// happened while the lock was held.
///
/// # Safety
/// Output may interleave with whoever holds the lock.
pub unsafe fn panic_port() -> SerialPort {
    SerialPort::new(COM1)
}
