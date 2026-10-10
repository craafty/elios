//! The Global Descriptor Table and Task State Segment.
//!
//! Segmentation is mostly unused in 64-bit mode, but the CPU still needs code
//! and data descriptors for each privilege level, and a TSS to tell it which
//! stacks to switch to on interrupts.
//!
//! The descriptor order is fixed by `syscall`/`sysret`, which compute the
//! user selectors from a single base: user data must directly precede user
//! code.
//!
//! Every CPU needs its own TSS (it holds that CPU's stacks), so each has its
//! own GDT too. The boot CPU's live in static storage, since it sets them up
//! before there's a heap; the others' are allocated.

use alloc::boxed::Box;
use core::arch::asm;
use core::sync::atomic::Ordering;

use super::cpu::{self, Cpu};
use crate::memory;
use crate::sync::TakeOnce;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS: u16 = 0x28;

/// Interrupt Stack Table slots. These exceptions get a known-good stack of
/// their own, because they can happen when the current stack is unusable
/// (a double fault from a kernel stack overflow, for example).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;

const IST_STACK_SIZE: usize = 16 * 1024;
const IST_STACK_PAGES: u64 = IST_STACK_SIZE as u64 / memory::PAGE_SIZE;

#[repr(C, align(16))]
struct Stack([u8; IST_STACK_SIZE]);

static IST_STACKS: TakeOnce<[Stack; 3]> = TakeOnce::new([const { Stack([0; IST_STACK_SIZE]) }; 3]);

/// The 64-bit TSS. Its 8-byte fields sit at 4-byte offsets, hence `packed`.
#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    /// Stacks for entering rings 0-2 from a less privileged ring.
    privilege_stacks: [u64; 3],
    reserved1: u64,
    interrupt_stacks: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_bitmap_offset: u16,
}

impl TaskStateSegment {
    const EMPTY: Self = Self {
        reserved0: 0,
        privilege_stacks: [0; 3],
        reserved1: 0,
        interrupt_stacks: [0; 7],
        reserved2: 0,
        reserved3: 0,
        io_bitmap_offset: 0,
    };
}

static TSS_STORAGE: TakeOnce<TaskStateSegment> = TakeOnce::new(TaskStateSegment::EMPTY);

/// Null, kernel code/data, user data/code, and the TSS (which takes two
/// slots).
static GDT: TakeOnce<[u64; 7]> = TakeOnce::new([0; 7]);

// Flat segments: base 0, limit 4 GiB (ignored in 64-bit mode). The access
// byte (bits 40-47) carries present, privilege level and type; bit 53 (L)
// marks 64-bit code.
const KERNEL_CODE_DESCRIPTOR: u64 = 0x00AF_9A00_0000_FFFF;
const KERNEL_DATA_DESCRIPTOR: u64 = 0x00CF_9200_0000_FFFF;
const USER_DATA_DESCRIPTOR: u64 = 0x00CF_F200_0000_FFFF;
const USER_CODE_DESCRIPTOR: u64 = 0x00AF_FA00_0000_FFFF;

#[repr(C, packed)]
pub struct DescriptorTablePointer {
    pub limit: u16,
    pub base: u64,
}

/// Builds the boot CPU's GDT and TSS and loads them.
pub fn init(cpu: &'static Cpu) {
    let stacks = IST_STACKS.take().expect("GDT initialized twice");
    let tops = [0, 1, 2].map(|index| stacks[index].0.as_ptr_range().end as u64);
    let tss = TSS_STORAGE.take().expect("GDT initialized twice");
    let gdt = GDT.take().expect("GDT initialized twice");
    load(cpu, gdt, tss, tops);
}

/// Builds and loads a GDT and TSS for another CPU, with guarded stacks for
/// the exceptions that need their own.
pub fn init_other(cpu: &'static Cpu) {
    let tops = [0; 3].map(|_| memory::allocate_kernel_stack(IST_STACK_PAGES).top);
    load(cpu, Box::leak(Box::new([0; 7])), Box::leak(Box::new(TaskStateSegment::EMPTY)), tops);
}

/// Fills in a GDT and TSS (with `ist_tops` as the double fault, NMI and
/// machine check stacks) and loads them, reloading every segment register.
fn load(cpu: &Cpu, gdt: &'static mut [u64; 7], tss: &'static mut TaskStateSegment, ist_tops: [u64; 3]) {
    let mut interrupt_stacks = [0; 7];
    interrupt_stacks[usize::from(IST_DOUBLE_FAULT) - 1] = ist_tops[0];
    interrupt_stacks[usize::from(IST_NMI) - 1] = ist_tops[1];
    interrupt_stacks[usize::from(IST_MACHINE_CHECK) - 1] = ist_tops[2];

    *tss = TaskStateSegment {
        reserved0: 0,
        privilege_stacks: [0; 3],
        reserved1: 0,
        interrupt_stacks,
        reserved2: 0,
        reserved3: 0,
        // Past the end of the TSS: no I/O permission bitmap.
        io_bitmap_offset: size_of::<TaskStateSegment>() as u16,
    };

    cpu.tss.store(core::ptr::from_mut(tss) as u64, Ordering::Relaxed);

    let (tss_low, tss_high) = tss_descriptor(core::ptr::from_ref(tss) as u64);
    *gdt = [
        0,
        KERNEL_CODE_DESCRIPTOR,
        KERNEL_DATA_DESCRIPTOR,
        USER_DATA_DESCRIPTOR,
        USER_CODE_DESCRIPTOR,
        tss_low,
        tss_high,
    ];

    let pointer = DescriptorTablePointer { limit: (size_of_val(gdt) - 1) as u16, base: gdt.as_ptr() as u64 };
    // SAFETY: the GDT and TSS are 'static and the selectors match their
    // layout. The far return reloads CS, which can't be moved into directly.
    unsafe {
        asm!(
            "lgdt [{pointer}]",
            "push {code}",
            "lea {scratch}, [rip + 3f]",
            "push {scratch}",
            "retfq",
            "3:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            // Loading GS would clear the GS base, which holds this CPU's
            // data; FS is cleared for a clean start.
            "mov fs, {null:x}",
            "ltr {tss:x}",
            pointer = in(reg) &raw const pointer,
            code = in(reg) u64::from(KERNEL_CODE),
            data = in(reg) KERNEL_DATA,
            null = in(reg) 0u16,
            tss = in(reg) TSS,
            scratch = out(reg) _,
        );
    }
}

/// Sets the stack this CPU switches to when an interrupt arrives in user
/// mode. Called on every switch to a thread.
pub fn set_kernel_stack(top: u64) {
    let tss = cpu::current().tss.load(Ordering::Relaxed) as *mut TaskStateSegment;
    // SAFETY: the TSS is 'static; the field is misaligned (it's packed),
    // hence the unaligned write.
    unsafe { (&raw mut (*tss).privilege_stacks).cast::<u64>().write_unaligned(top) };
}

/// The two 8-byte halves of a 64-bit TSS descriptor.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<TaskStateSegment>() - 1) as u64;
    let low = (limit & 0xFFFF)
        | (base & 0xFF_FFFF) << 16
        | 0x89 << 40 // present, type "available 64-bit TSS"
        | (limit >> 16 & 0xF) << 48
        | (base >> 24 & 0xFF) << 56;
    (low, base >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selectors() -> (u16, u16, u16) {
        let (cs, ss, tr): (u16, u16, u16);
        // SAFETY: reads segment registers only.
        unsafe { asm!("mov {:x}, cs", "mov {:x}, ss", "str {:x}", out(reg) cs, out(reg) ss, out(reg) tr) };
        (cs, ss, tr)
    }

    crate::ktest! {
        fn our_segments_are_loaded() {
            assert_eq!(selectors(), (KERNEL_CODE, KERNEL_DATA, TSS));
        }

        fn tss_descriptor_encodes_its_base() {
            let (low, high) = tss_descriptor(0xFFFF_8000_1234_5678);
            assert_eq!(low & 0xFFFF, 103);
            assert_eq!(low >> 16 & 0xFF_FFFF, 0x34_5678);
            assert_eq!(low >> 56, 0x12);
            assert_eq!(high, 0xFFFF_8000);
        }
    }
}
