//! Interrupt and exception handling.
//!
//! Every vector enters through a small assembly stub that pushes its vector
//! number (and a zero error code where the CPU doesn't push one), then jumps
//! to a common routine that saves all general-purpose registers. The result
//! is an [`InterruptFrame`] on the stack, holding the interrupted code's
//! complete register state, which is handed to [`interrupt_dispatch`].
//!
//! The stubs are 16 bytes apart, so stub `n` is at `interrupt_stubs + 16 * n`
//! and no table of addresses is needed.

use core::arch::{asm, global_asm};
use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::{apic, gdt};
use crate::memory;
use crate::sync::TakeOnce;

// Vector assignments.
pub const DOUBLE_FAULT: u8 = 8;
pub const NMI: u8 = 2;
pub const BREAKPOINT: u8 = 3;
pub const PAGE_FAULT: u8 = 14;
pub const MACHINE_CHECK: u8 = 18;
/// The legacy PICs are remapped here and masked; only spurious interrupts
/// can arrive.
pub const PIC_BASE: u8 = 0x20;
/// Interrupts routed through the I/O APIC start here.
pub const IRQ_BASE: u8 = 0x30;
pub const TIMER: u8 = 0xF0;
pub const APIC_ERROR: u8 = 0xFE;
pub const SPURIOUS: u8 = 0xFF;

/// Register state of the interrupted code, in the order the entry code
/// pushes it (so the last push is the first field).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    /// Pushed by the CPU for some exceptions, zero otherwise.
    pub error_code: u64,
    // Pushed by the CPU on every interrupt.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// The entry code assumes this size: it keeps the stack 16-byte aligned for
// the call into Rust.
const _: () = assert!(size_of::<InterruptFrame>() == 22 * 8);

global_asm!(
    r#"
    .section .text.interrupts, "ax"

    .balign 16
    .global interrupt_stubs
interrupt_stubs:
    .set vector, 0
    .rept 256
        .balign 16
        // Exceptions that come with an error code: #DF, #TS, #NP, #SS, #GP,
        // #PF, #AC, #CP, #VC, #SX. Everything else gets a zero.
        .if vector == 8 || (vector >= 10 && vector <= 14) || vector == 17 || vector == 21 || vector == 29 || vector == 30
        .else
            pushq $0
        .endif
        pushq $vector
        jmp interrupt_common
        .set vector, vector + 1
    .endr

interrupt_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    // The ABI requires the direction flag clear on function entry.
    cld
    movq %rsp, %rdi
    call interrupt_dispatch
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    // Drop the vector number and error code.
    addq $16, %rsp
    iretq
    "#,
    options(att_syntax)
);

unsafe extern "C" {
    /// Start of the entry stubs; only the address matters.
    static interrupt_stubs: u8;
}

const STUB_SIZE: u64 = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    flags: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: Self =
        Self { offset_low: 0, selector: 0, ist: 0, flags: 0, offset_middle: 0, offset_high: 0, reserved: 0 };

    /// A present, ring-0 interrupt gate. Interrupt gates clear the interrupt
    /// flag on entry, so handlers run with interrupts disabled.
    fn new(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            flags: 0x8E,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    #[cfg(test)]
    fn handler(&self) -> u64 {
        u64::from(self.offset_low) | u64::from(self.offset_middle) << 16 | u64::from(self.offset_high) << 32
    }
}

static IDT: TakeOnce<[IdtEntry; 256]> = TakeOnce::new([IdtEntry::MISSING; 256]);

/// Builds and loads the IDT. Exceptions are handled from this point on.
pub fn init() {
    let idt = IDT.take().expect("IDT initialized twice");
    let stubs = &raw const interrupt_stubs as u64;
    for (vector, entry) in idt.iter_mut().enumerate() {
        let ist = match vector as u8 {
            DOUBLE_FAULT => gdt::IST_DOUBLE_FAULT,
            NMI => gdt::IST_NMI,
            MACHINE_CHECK => gdt::IST_MACHINE_CHECK,
            _ => 0,
        };
        *entry = IdtEntry::new(stubs + STUB_SIZE * vector as u64, ist);
    }

    let pointer = gdt::DescriptorTablePointer { limit: (size_of_val(idt) - 1) as u16, base: idt.as_ptr() as u64 };
    // SAFETY: the IDT is 'static and every entry points at a valid stub.
    unsafe { asm!("lidt [{}]", in(reg) &raw const pointer, options(nostack, preserves_flags)) };
}

pub type Handler = fn(&mut InterruptFrame);

/// Handlers for vectors 32 and up, stored as addresses (0 = none).
static HANDLERS: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];

/// Installs the handler for a hardware or software interrupt vector.
pub fn set_handler(vector: u8, handler: Handler) {
    assert!(vector >= 32, "vectors below 32 are CPU exceptions");
    HANDLERS[usize::from(vector)].store(handler as usize, Ordering::Release);
}

/// Called by the entry code for every interrupt and exception.
#[unsafe(no_mangle)]
extern "sysv64" fn interrupt_dispatch(frame: &mut InterruptFrame) {
    let vector = frame.vector as u8;
    if vector < 32 {
        handle_exception(frame);
        return;
    }

    let handler = HANDLERS[usize::from(vector)].load(Ordering::Acquire);
    if handler != 0 {
        // SAFETY: only `set_handler` stores into HANDLERS, and it stores
        // `Handler` function pointers.
        let handler = unsafe { core::mem::transmute::<usize, Handler>(handler) };
        handler(frame);
    } else if (PIC_BASE..IRQ_BASE).contains(&vector) || vector == SPURIOUS {
        // Spurious interrupts need no acknowledgment.
        return;
    } else {
        warn!("unexpected interrupt on vector {vector:#x}");
    }

    if vector >= IRQ_BASE {
        apic::end_of_interrupt();
    }
}

static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

/// How many `int3` breakpoints have been hit.
#[cfg(test)]
pub fn breakpoint_count() -> u64 {
    BREAKPOINTS.load(Ordering::Relaxed)
}

fn handle_exception(frame: &mut InterruptFrame) {
    if frame.vector == u64::from(BREAKPOINT) {
        BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
        debug!("breakpoint at {:#x}", frame.rip);
        return;
    }
    // Nothing can recover from the rest yet. Once there is userspace, faults
    // in user code will end the process instead.
    let fault_address = if frame.vector == u64::from(PAGE_FAULT) { Some(super::read_cr2()) } else { None };
    let hint = match fault_address {
        Some(address) => memory::describe_fault_address(address),
        // A double fault with the stack pointer at a guard page is almost
        // always a stack overflow: the page fault couldn't be delivered
        // because the CPU had no stack to push it on.
        None if frame.vector == u64::from(DOUBLE_FAULT) => memory::describe_fault_address(frame.rsp.wrapping_sub(64))
            .filter(|description| description.contains("guard page"))
            .map(|_| "the stack pointer is at a kernel stack's guard page: the stack overflowed"),
        None => None,
    };
    panic!("{}", ExceptionReport { frame, fault_address, hint });
}

const EXCEPTIONS: [(&str, &str); 32] = [
    ("#DE", "Divide Error"),
    ("#DB", "Debug"),
    ("NMI", "Non-Maskable Interrupt"),
    ("#BP", "Breakpoint"),
    ("#OF", "Overflow"),
    ("#BR", "Bound Range Exceeded"),
    ("#UD", "Invalid Opcode"),
    ("#NM", "Device Not Available"),
    ("#DF", "Double Fault"),
    ("--", "Coprocessor Segment Overrun"),
    ("#TS", "Invalid TSS"),
    ("#NP", "Segment Not Present"),
    ("#SS", "Stack-Segment Fault"),
    ("#GP", "General Protection Fault"),
    ("#PF", "Page Fault"),
    ("--", "Reserved"),
    ("#MF", "x87 Floating-Point Exception"),
    ("#AC", "Alignment Check"),
    ("#MC", "Machine Check"),
    ("#XM", "SIMD Floating-Point Exception"),
    ("#VE", "Virtualization Exception"),
    ("#CP", "Control Protection Exception"),
    ("--", "Reserved"),
    ("--", "Reserved"),
    ("--", "Reserved"),
    ("--", "Reserved"),
    ("--", "Reserved"),
    ("--", "Reserved"),
    ("#HV", "Hypervisor Injection Exception"),
    ("#VC", "VMM Communication Exception"),
    ("#SX", "Security Exception"),
    ("--", "Reserved"),
];

/// Whether the CPU pushes an error code for this exception.
fn has_error_code(vector: u64) -> bool {
    matches!(vector, 8 | 10..=14 | 17 | 21 | 29 | 30)
}

/// A human-readable account of a CPU exception.
struct ExceptionReport<'a> {
    frame: &'a InterruptFrame,
    fault_address: Option<u64>,
    /// What we can tell about the cause.
    hint: Option<&'static str>,
}

impl fmt::Display for ExceptionReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let frame = self.frame;
        let (mnemonic, name) = EXCEPTIONS[frame.vector as usize % 32];
        write!(f, "CPU exception: {name} ({mnemonic}, vector {})", frame.vector)?;
        if frame.cs & 3 == 3 {
            f.write_str(" in user mode")?;
        }
        if has_error_code(frame.vector) {
            write!(f, ", error code {:#x}", frame.error_code)?;
        }
        if let Some(address) = self.fault_address {
            let code = frame.error_code;
            let access = if code & (1 << 4) != 0 {
                "instruction fetch"
            } else if code & (1 << 1) != 0 {
                "write"
            } else {
                "read"
            };
            let cause = if code & (1 << 3) != 0 {
                "reserved bit set in page table"
            } else if code & 1 != 0 {
                "protection violation"
            } else {
                "page not present"
            };
            write!(f, "\n  {access} of {address:#x}: {cause}")?;
        }
        if let Some(hint) = self.hint {
            write!(f, "\n  ({hint})")?;
        }

        write!(f, "\n  rip {:#018x}  rsp {:#018x}  rflags {:#010x}", frame.rip, frame.rsp, frame.rflags)?;
        write!(f, "\n  cs  {:#06x}              ss  {:#06x}", frame.cs, frame.ss)?;
        let registers = [
            ("rax", frame.rax),
            ("rbx", frame.rbx),
            ("rcx", frame.rcx),
            ("rdx", frame.rdx),
            ("rsi", frame.rsi),
            ("rdi", frame.rdi),
            ("rbp", frame.rbp),
            ("r8 ", frame.r8),
            ("r9 ", frame.r9),
            ("r10", frame.r10),
            ("r11", frame.r11),
            ("r12", frame.r12),
            ("r13", frame.r13),
            ("r14", frame.r14),
            ("r15", frame.r15),
        ];
        for (index, (name, value)) in registers.iter().enumerate() {
            f.write_str(if index % 3 == 0 { "\n  " } else { "  " })?;
            write!(f, "{name} {value:#018x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::StrBuf;

    crate::ktest! {
        fn stubs_are_where_the_idt_says() {
            let mut pointer = gdt::DescriptorTablePointer { limit: 0, base: 0 };
            // SAFETY: sidt only writes the 10-byte pointer.
            unsafe { asm!("sidt [{}]", in(reg) &raw mut pointer, options(nostack, preserves_flags)) };
            let limit = pointer.limit;
            assert_eq!(usize::from(limit) + 1, 256 * size_of::<IdtEntry>());

            // SAFETY: the IDT register points at our 'static table.
            let idt = unsafe { &*(pointer.base as *const [IdtEntry; 256]) };
            let stubs = &raw const interrupt_stubs as u64;
            assert_eq!(idt[PAGE_FAULT as usize].handler(), stubs + 14 * STUB_SIZE);
            assert_eq!(idt[DOUBLE_FAULT as usize].ist, gdt::IST_DOUBLE_FAULT);
            assert_eq!(idt[255].handler(), stubs + 255 * STUB_SIZE);
        }

        fn breakpoints_are_handled_and_return() {
            let before = breakpoint_count();
            // SAFETY: int3 traps into our breakpoint handler, which returns.
            unsafe { asm!("int3") };
            assert_eq!(breakpoint_count(), before + 1);
        }

        fn registers_survive_an_interrupt() {
            let (a, b): (u64, u64);
            // SAFETY: int3 returns, and the entry code restores every register.
            unsafe {
                asm!(
                    "mov r12, 0x1234",
                    "mov r15, 0x5678",
                    "int3",
                    "mov {a}, r12",
                    "mov {b}, r15",
                    a = out(reg) a,
                    b = out(reg) b,
                    out("r12") _,
                    out("r15") _,
                )
            };
            assert_eq!((a, b), (0x1234, 0x5678));
        }

        fn page_fault_reports_explain_the_fault() {
            let frame = InterruptFrame { vector: 14, error_code: 0b10, rip: 0xFFFF_FFFF_8000_1000, cs: 8, ..Default::default() };
            let mut text = StrBuf::<1024>::new();
            let report = ExceptionReport { frame: &frame, fault_address: Some(0xdead), hint: Some("a hint") };
            core::fmt::write(&mut text, format_args!("{report}")).unwrap();
            let text = text.as_str();
            assert!(text.starts_with("CPU exception: Page Fault (#PF, vector 14), error code 0x2"), "{text}");
            assert!(text.contains("write of 0xdead: page not present\n  (a hint)"), "{text}");
            assert!(text.contains("rip 0xffffffff80001000"), "{text}");
        }
    }
}
