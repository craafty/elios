//! Per-CPU data.
//!
//! Each CPU has a [`Cpu`] block, and its GS base register points at it while
//! in the kernel (`swapgs` trades it for the user's GS value on the way in
//! and out of user mode). The block's first fields are at fixed offsets the
//! system call entry code relies on; one of them holds the block's own
//! address, so Rust code can find it with a single GS-relative load.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::wrmsr;

/// The most CPUs eliOS will use; any more are left idle.
pub const MAX_CPUS: usize = 64;

const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

#[repr(C)]
pub struct Cpu {
    /// Top of the running thread's kernel stack (offset 0, used by
    /// `syscall_entry`).
    pub kernel_stack: AtomicU64,
    /// The user stack pointer during system call entry (offset 8).
    pub user_stack: AtomicU64,
    /// This block's own address (offset 16).
    this: AtomicU64,
    /// 0 for the boot CPU, then 1, 2, ... in the order they started.
    pub index: AtomicUsize,
    pub apic_id: AtomicU32,
    pub online: AtomicBool,
    /// Set when the running thread should be switched out at the next
    /// chance (see `task::preempt_point`).
    pub need_reschedule: AtomicBool,
    /// Set by another CPU that changed page tables and needs this CPU's
    /// TLB flushed (see `smp::flush_tlb`).
    pub shootdown_pending: AtomicBool,
    /// This CPU's Task State Segment.
    pub tss: AtomicU64,
    /// The running thread's id, and its user thread record if it has one
    /// (the scheduler keeps both up to date).
    pub current_thread: AtomicU64,
    pub current_user: AtomicPtr<()>,
    /// The page tables the scheduler last loaded here (CR3), so that
    /// changes to a program's mappings only interrupt the CPUs running it
    /// (see `smp::flush_user_tlb`). `u64::MAX` until the first switch:
    /// unknown, so always flushed.
    pub page_table: AtomicU64,
}

impl Cpu {
    const fn new() -> Self {
        Self {
            kernel_stack: AtomicU64::new(0),
            user_stack: AtomicU64::new(0),
            this: AtomicU64::new(0),
            index: AtomicUsize::new(0),
            apic_id: AtomicU32::new(0),
            online: AtomicBool::new(false),
            need_reschedule: AtomicBool::new(false),
            shootdown_pending: AtomicBool::new(false),
            tss: AtomicU64::new(0),
            current_thread: AtomicU64::new(0),
            current_user: AtomicPtr::new(core::ptr::null_mut()),
            page_table: AtomicU64::new(u64::MAX),
        }
    }

    pub fn index(&self) -> usize {
        self.index.load(Ordering::Relaxed)
    }
}

static CPUS: [Cpu; MAX_CPUS] = [const { Cpu::new() }; MAX_CPUS];
/// How many CPUs have been given a block (started or starting).
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Claims the next CPU block, for a CPU about to start.
pub fn allocate(apic_id: u32) -> Option<&'static Cpu> {
    let index = COUNT.fetch_add(1, Ordering::Relaxed);
    let Some(cpu) = CPUS.get(index) else {
        COUNT.fetch_sub(1, Ordering::Relaxed);
        return None;
    };
    cpu.index.store(index, Ordering::Relaxed);
    cpu.apic_id.store(apic_id, Ordering::Relaxed);
    cpu.this.store(core::ptr::from_ref(cpu) as u64, Ordering::Relaxed);
    Some(cpu)
}

/// Points this CPU's GS base at its block. Called first thing on each CPU.
pub fn install(cpu: &'static Cpu) {
    // SAFETY: GS is only used to reach per-CPU data; user GS starts at 0.
    unsafe {
        wrmsr(IA32_GS_BASE, core::ptr::from_ref(cpu) as u64);
        wrmsr(IA32_KERNEL_GS_BASE, 0);
    }
}

/// The CPU this code runs on. The answer is only stable while the thread
/// can't move to another CPU: with interrupts disabled, or for data every
/// CPU has its own copy of anyway.
pub fn current() -> &'static Cpu {
    let address: u64;
    // SAFETY: GS points at this CPU's block from `install` on, and offset
    // 16 holds the block's address.
    unsafe { asm!("mov {}, gs:[16]", out(reg) address, options(nostack, preserves_flags, readonly)) };
    // SAFETY: blocks are 'static.
    unsafe { &*(address as *const Cpu) }
}

/// The CPUs that have started.
pub fn online() -> impl Iterator<Item = &'static Cpu> {
    CPUS[..COUNT.load(Ordering::Relaxed).min(MAX_CPUS)].iter().filter(|cpu| cpu.online.load(Ordering::Acquire))
}

pub fn online_count() -> usize {
    online().count()
}

pub fn get(index: usize) -> Option<&'static Cpu> {
    CPUS.get(index).filter(|cpu| cpu.online.load(Ordering::Acquire))
}
