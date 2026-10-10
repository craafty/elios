//! Multiple processors: starting the other CPUs, and the requests CPUs send
//! each other.
//!
//! The firmware leaves every CPU but the boot CPU halted, waiting for the
//! INIT and STARTUP signals. A STARTUP signal starts a CPU in 16-bit real
//! mode at a page below 1 MiB, so a small "trampoline" is copied there. It
//! goes straight from real mode to 64-bit mode (setting the protection and
//! paging bits together), using temporary page tables that map the
//! trampoline at its own address and share the kernel half, then jumps into
//! the kernel at [`ap_main`].
//!
//! Once running, CPUs interrupt each other to request work: look for a
//! thread to run (`RESCHEDULE`), flush translations for page tables another
//! CPU changed (`TLB_SHOOTDOWN`), or stop after a panic (an NMI, which gets
//! through even with interrupts disabled).

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bootinfo::{BootInfo, MemoryKind};

use super::cpu::{self, Cpu};
use super::interrupts::{self, InterruptFrame};
use super::{apic, fpu, gdt, syscall};
use crate::memory::{self, KernelStack, PAGE_SIZE, phys_to_virt};
use crate::sync::SpinLock;
use crate::{acpi, task, time};

global_asm!(
    r#"
    .section .text.ap_trampoline, "ax"
    .code16
    .global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    // Real mode addresses are segment * 16 + offset; the startup signal set
    // CS to this page, so offsets from the start work through DS = CS.
    movw %cs, %ax
    movw %ax, %ds
    lgdt (ap_trampoline_gdtr - ap_trampoline_start)
    // Physical address extension and global pages, then the page tables.
    movl $0xA0, %eax
    movl %eax, %cr4
    movl (ap_trampoline_page_table - ap_trampoline_start), %eax
    movl %eax, %cr3
    // EFER: long mode and no-execute pages.
    movl $0xC0000080, %ecx
    rdmsr
    orl $0x900, %eax
    wrmsr
    // Protection and paging on together: straight into long mode.
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0
    // A far jump to the 64-bit code segment: jmp 0x08:target, with a 32-bit
    // target the kernel fills in.
    .byte 0x66, 0xEA
    .global ap_trampoline_far_target
ap_trampoline_far_target:
    .long 0
    .word 0x08

    .code64
    .global ap_trampoline_long_mode
ap_trampoline_long_mode:
    movl $0x10, %eax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movq ap_trampoline_stack(%rip), %rsp
    movq ap_trampoline_cpu(%rip), %rdi
    movq ap_trampoline_entry(%rip), %rax
    xorl %ebp, %ebp
    pushq $0
    jmpq *%rax

    .balign 8
    .global ap_trampoline_gdt
ap_trampoline_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .global ap_trampoline_gdtr
ap_trampoline_gdtr:
    .word 23
    .long 0
    .balign 8
    .global ap_trampoline_page_table
ap_trampoline_page_table:
    .quad 0
    .global ap_trampoline_stack
ap_trampoline_stack:
    .quad 0
    .global ap_trampoline_cpu
ap_trampoline_cpu:
    .quad 0
    .global ap_trampoline_entry
ap_trampoline_entry:
    .quad 0
    .global ap_trampoline_end
ap_trampoline_end:
    "#,
    options(att_syntax)
);

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_far_target: u8;
    static ap_trampoline_long_mode: u8;
    static ap_trampoline_gdt: u8;
    static ap_trampoline_gdtr: u8;
    static ap_trampoline_page_table: u8;
    static ap_trampoline_stack: u8;
    static ap_trampoline_cpu: u8;
    static ap_trampoline_entry: u8;
    static ap_trampoline_end: u8;
}

/// Set once a second CPU is running: from then on, page table changes must
/// be announced to the others.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set when a panicking CPU stops the others.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// The boot CPU's control registers, which the others copy.
static CR0: AtomicU64 = AtomicU64::new(0);
static CR4: AtomicU64 = AtomicU64::new(0);
/// The stack for the CPU being started; it becomes that CPU's idle thread.
static STARTING_STACK: SpinLock<Option<KernelStack>> = SpinLock::new(None);

/// Pages used below 1 MiB: the trampoline, then its PML4, PDPT and page
/// directory.
const LOW_PAGES: u64 = 4;
const PRESENT_WRITABLE: u64 = 0b11;
const HUGE: u64 = 1 << 7;

/// Starts every other enabled CPU in the ACPI tables. Returns how many
/// CPUs are running.
pub fn start_others(boot_info: &BootInfo, madt: &acpi::Madt) -> usize {
    interrupts::set_handler(interrupts::RESCHEDULE, |_| {});
    interrupts::set_handler(interrupts::TLB_SHOOTDOWN, on_shootdown);
    let me = apic::local_id();
    let others: alloc::vec::Vec<u32> = madt.processors.iter().copied().filter(|&id| id != u32::from(me)).collect();
    if others.is_empty() {
        return 1;
    }
    let Some(base) = low_memory(boot_info) else {
        warn!("smp: no free memory below 1 MiB to start other CPUs from");
        return 1;
    };
    // SAFETY: reading control registers has no side effects.
    unsafe {
        let (cr0, cr4): (u64, u64);
        asm!("mov {}, cr0", "mov {}, cr4", out(reg) cr0, out(reg) cr4, options(nomem, nostack, preserves_flags));
        CR0.store(cr0, Ordering::Relaxed);
        CR4.store(cr4, Ordering::Relaxed);
    }
    install_trampoline(base);

    for apic_id in others {
        // The xAPIC can only address IDs up to 254.
        if apic_id >= 0xFF {
            continue;
        }
        let Some(cpu) = cpu::allocate(apic_id) else {
            warn!("smp: more than {} CPUs; the rest stay idle", cpu::MAX_CPUS);
            break;
        };
        if !start(base, cpu) {
            warn!("smp: CPU with APIC ID {apic_id} didn't start");
        }
    }
    cpu::online_count()
}

/// Finds `LOW_PAGES` pages of free RAM below 1 MiB. The frame allocator
/// never hands out memory there, keeping it for this.
fn low_memory(boot_info: &BootInfo) -> Option<u64> {
    let span = LOW_PAGES * PAGE_SIZE;
    boot_info
        .memory_regions()
        .iter()
        .filter(|r| matches!(r.kind, MemoryKind::Usable | MemoryKind::BootloaderReclaimable))
        .map(|r| (r.start.max(0x1000).next_multiple_of(PAGE_SIZE), r.end().min(0x9F000)))
        .find(|&(start, end)| start + span <= end)
        .map(|(start, _)| start)
}

/// Copies the trampoline to `base` and builds its page tables after it.
fn install_trampoline(base: u64) {
    let start = &raw const ap_trampoline_start as u64;
    let offset = |symbol: *const u8| symbol as u64 - start;
    let size = offset(&raw const ap_trampoline_end);
    // SAFETY: the pages are free RAM below 1 MiB, in the direct map, used by
    // nothing else; the trampoline code is `size` bytes of the kernel image.
    unsafe {
        let page = phys_to_virt(base);
        core::ptr::copy_nonoverlapping(start as *const u8, page, size as usize);
        let patch32 = |symbol: *const u8, value: u64| {
            page.add(offset(symbol) as usize).cast::<u32>().write_unaligned(value as u32)
        };
        patch32(&raw const ap_trampoline_far_target, base + offset(&raw const ap_trampoline_long_mode));
        patch32((&raw const ap_trampoline_gdtr).add(2), base + offset(&raw const ap_trampoline_gdt));
        page.add(offset(&raw const ap_trampoline_page_table) as usize).cast::<u64>().write(base + PAGE_SIZE);

        // PML4 -> PDPT -> page directory, whose first entry maps the first
        // 2 MiB (with the trampoline) at their own addresses. The kernel half
        // comes from the kernel's own PML4.
        let table = |index: u64| phys_to_virt(base + index * PAGE_SIZE).cast::<u64>();
        for index in 1..LOW_PAGES {
            core::ptr::write_bytes(table(index), 0, 512);
        }
        table(1).write((base + 2 * PAGE_SIZE) | PRESENT_WRITABLE);
        table(2).write((base + 3 * PAGE_SIZE) | PRESENT_WRITABLE);
        table(3).write(HUGE | PRESENT_WRITABLE);
        let kernel = phys_to_virt(memory::kernel_page_table_root()).cast::<u64>();
        core::ptr::copy_nonoverlapping(kernel.add(256), table(1).add(256), 256);
    }
}

/// Starts one CPU with INIT, STARTUP, STARTUP (the sequence Intel
/// documents) and waits for it to come online.
fn start(base: u64, cpu: &'static Cpu) -> bool {
    let stack = memory::allocate_kernel_stack(task::STACK_PAGES);
    let page = phys_to_virt(base);
    let field = |symbol: *const u8| {
        // SAFETY: the field lies within the copied trampoline.
        unsafe { page.add((symbol as u64 - &raw const ap_trampoline_start as u64) as usize).cast::<u64>() }
    };
    // SAFETY: as above; the CPU reads these once it starts.
    unsafe {
        field(&raw const ap_trampoline_stack).write(stack.top);
        field(&raw const ap_trampoline_cpu).write(core::ptr::from_ref(cpu) as u64);
        field(&raw const ap_trampoline_entry).write(ap_main as *const () as u64);
    }
    *STARTING_STACK.lock() = Some(stack);

    let apic_id = cpu.apic_id.load(Ordering::Relaxed);
    apic::send_init(apic_id);
    delay_us(10_000);
    for _ in 0..2 {
        apic::send_startup(apic_id, (base / PAGE_SIZE) as u8);
        delay_us(200);
        if cpu.online.load(Ordering::Acquire) {
            return true;
        }
    }
    // Emulators can take a while to get going.
    let deadline = time::uptime_nanos() + 1_000_000_000;
    while time::uptime_nanos() < deadline {
        if cpu.online.load(Ordering::Acquire) {
            return true;
        }
        core::hint::spin_loop();
    }
    if let Some(stack) = STARTING_STACK.lock().take() {
        memory::free_kernel_stack(stack);
    }
    false
}

fn delay_us(us: u64) {
    let deadline = time::uptime_nanos() + us * 1000;
    while time::uptime_nanos() < deadline {
        core::hint::spin_loop();
    }
}

/// Where a started CPU enters the kernel, on its own stack, with the
/// trampoline's page tables.
extern "sysv64" fn ap_main(cpu: &'static Cpu) -> ! {
    // SAFETY: the kernel's tables map everything the kernel uses, including
    // this stack; the control register values are the boot CPU's, which
    // works here too.
    unsafe {
        super::set_page_table_root(memory::kernel_page_table_root());
        asm!(
            "mov cr0, {}",
            "mov cr4, {}",
            in(reg) CR0.load(Ordering::Relaxed),
            in(reg) CR4.load(Ordering::Relaxed),
            options(nostack, preserves_flags),
        );
    }
    cpu::install(cpu);
    super::init_page_attributes();
    gdt::init_other(cpu);
    interrupts::load();
    syscall::init();
    fpu::init_cpu();
    apic::init_local(acpi::madt().expect("ACPI tables were read at startup"));
    time::start_timer();
    let stack = STARTING_STACK.lock().take().expect("started CPU has no stack");
    ACTIVE.store(true, Ordering::Release);
    task::start_cpu(cpu, stack)
}

static SHOOTDOWN_LOCK: AtomicBool = AtomicBool::new(false);
static SHOOTDOWN_START: AtomicU64 = AtomicU64::new(0);
static SHOOTDOWN_END: AtomicU64 = AtomicU64::new(0);

/// Flushes the TLB entries for `start..end` on every CPU, and returns once
/// they're all done. Call after changing or removing mappings, before
/// freeing the memory they pointed at: other CPUs may still have the old
/// translations cached.
pub fn flush_tlb(start: u64, end: u64) {
    flush_local(start, end);
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let enabled = super::interrupts_enabled();
    super::disable_interrupts();
    let me = cpu::current().index();
    while SHOOTDOWN_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        serve_shootdown();
        core::hint::spin_loop();
    }
    SHOOTDOWN_START.store(start, Ordering::Relaxed);
    SHOOTDOWN_END.store(end, Ordering::Relaxed);
    let mut others = false;
    for cpu in cpu::online().filter(|cpu| cpu.index() != me) {
        cpu.shootdown_pending.store(true, Ordering::Release);
        others = true;
    }
    if others {
        apic::broadcast_ipi(interrupts::TLB_SHOOTDOWN);
        while cpu::online().any(|cpu| cpu.shootdown_pending.load(Ordering::Acquire)) {
            core::hint::spin_loop();
        }
    }
    SHOOTDOWN_LOCK.store(false, Ordering::Release);
    if enabled {
        super::enable_interrupts();
    }
}

/// Carries out a TLB shootdown requested of this CPU, if there is one.
/// Called from the interrupt, and by CPUs spinning with interrupts
/// disabled.
pub fn serve_shootdown() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let cpu = cpu::current();
    if cpu.shootdown_pending.load(Ordering::Acquire) {
        flush_local(SHOOTDOWN_START.load(Ordering::Relaxed), SHOOTDOWN_END.load(Ordering::Relaxed));
        cpu.shootdown_pending.store(false, Ordering::Release);
    }
}

fn on_shootdown(_frame: &mut InterruptFrame) {
    serve_shootdown();
}

/// Flushes this CPU's translations for `start..end`: page by page for small
/// ranges, everything (global pages included) for big ones.
fn flush_local(start: u64, end: u64) {
    if (end - start) / PAGE_SIZE <= 32 {
        for page in (start..end).step_by(PAGE_SIZE as usize) {
            super::flush_tlb_page(page);
        }
    } else {
        // SAFETY: toggling CR4.PGE flushes every translation, global ones
        // included; nothing else changes.
        unsafe {
            asm!(
                "mov {tmp}, cr4",
                "xor {tmp}, {pge}",
                "mov cr4, {tmp}",
                "xor {tmp}, {pge}",
                "mov cr4, {tmp}",
                tmp = out(reg) _,
                pge = const 1 << 7,
                options(nostack, preserves_flags),
            )
        };
    }
}

/// Asks the CPU running `cpu_index` to look for a thread to run.
pub fn send_reschedule(cpu_index: usize) {
    if let Some(cpu) = cpu::get(cpu_index) {
        apic::send_ipi(cpu.apic_id.load(Ordering::Relaxed), interrupts::RESCHEDULE);
    }
}

/// Halts every other CPU (for a panic). They stop wherever they are.
pub fn stop_others() {
    if ACTIVE.swap(false, Ordering::AcqRel) {
        STOPPING.store(true, Ordering::Release);
        apic::broadcast_nmi();
    }
}

/// Whether a panic is stopping the CPUs.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}
