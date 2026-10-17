# How eliOS works

This is the long version: how eliOS gets from the power button to a shell
prompt, how each part works, and why it was built that way. Where a choice
was made between alternatives, the alternatives and the reasons are given.

Contents:

1. [Principles](#1-principles)
2. [The build](#2-the-build)
3. [Booting: firmware to kernel](#3-booting-firmware-to-kernel)
4. [Kernel startup, step by step](#4-kernel-startup-step-by-step)
5. [Output: serial, console, font, log](#5-output-serial-console-font-log)
6. [The CPU: descriptor tables and interrupts](#6-the-cpu-descriptor-tables-and-interrupts)
7. [Time](#7-time)
8. [Memory](#8-memory)
9. [Locks and other synchronization](#9-locks-and-other-synchronization)
10. [Threads and the scheduler](#10-threads-and-the-scheduler)
11. [Entering and leaving user mode](#11-entering-and-leaving-user-mode)
12. [Processes](#12-processes)
13. [Signals](#13-signals)
14. [Users and permissions](#14-users-and-permissions)
15. [System calls and Linux compatibility](#15-system-calls-and-linux-compatibility)
16. [Files](#16-files)
17. [The terminal](#17-the-terminal)
18. [Devices and drivers](#18-devices-and-drivers)
19. [Networking](#19-networking)
20. [ACPI: tables and power](#20-acpi-tables-and-power)
21. [Userland](#21-userland)
22. [The system manager: packages, system.conf and generations](#22-the-system-manager-packages-systemconf-and-generations)
23. [Disk images and the build tool](#23-disk-images-and-the-build-tool)
24. [Testing](#24-testing)
25. [Limits and what's next](#25-limits-and-whats-next)
26. [Reference tables](#26-reference-tables)

---

## 1. Principles

Two goals pull in different directions and shape everything:

- **Everything is custom.** No third-party crates: the bootloader, kernel,
  user runtime and build tool use only Rust's `core` and `alloc` (and `std`
  on the host). Writing the UEFI bindings, ELF loader, page tables, FAT32
  driver and so on by hand is the point of the project.
- **It runs existing software.** A new OS with no programs is a curiosity.

The resolution: **original internals, standard interfaces.** Programs only
see the system call interface, so eliOS implements Linux's: the same call
numbers, register conventions, structure layouts and error codes. Everything
behind that line (how memory is managed, how scheduling works, how files are
stored) is eliOS's own design. The same thinking applies to formats other
systems touch: ELF executables, cpio archives, FAT32 and GPT disks are
standard so eliOS can exchange data with the rest of the world.

Smaller principles that come up repeatedly:

- **Simple first, then measured.** Polling disk drivers, a round-robin
  scheduler (and, until Firefox needed better, eager `fork` copies): each
  is the simplest thing that works correctly, with the faster design noted
  for later.
- **Fail loudly and clearly.** Every CPU fault produces a readable report;
  user faults are explained in the kernel log; the kernel panics rather than
  limping on in a corrupt state.
- **Test in the real environment.** Kernel tests run inside eliOS in QEMU,
  after a full boot, and many run real user programs.

## 2. The build

### Targets

eliOS is four kinds of binary, each needing a different compiler target:

| Binary | Target | Why |
|---|---|---|
| eliboot | `x86_64-unknown-uefi` | UEFI applications are PE files with the Microsoft calling convention; this target produces exactly that. |
| kernel | `x86_64-unknown-none` | Freestanding, no OS, soft-float (no SSE in the kernel, see section 11), no red zone (interrupts would clobber it). |
| user programs | `x86_64-unknown-linux-none` | "Linux system call ABI, no libc": precisely what eliOS programs are. Being Linux binaries, they also run on Linux. |
| xtask | the host | The build tool. |

The kernel adds three code generation flags (in `.cargo/config.toml`):
`relocation-model=static` and `code-model=kernel` because it's linked at one
fixed address in the top 2 GiB, and `force-frame-pointers=yes` so stack
traces are possible later.

User programs are built with `+crt-static` and `relocation-model=static`,
giving plain static executables at fixed addresses (`ET_EXEC`), the simplest
kind to load. `x86_64-unknown-linux-none` is a tier-3 target with no
prebuilt standard library, so xtask builds `core` and `alloc` from source
(`-Zbuild-std`), which needs the `rust-src` component.

### Why nightly

`rust-toolchain.toml` selects nightly. Two things need it: `-Zbuild-std` for
the user target, and the tier-3 target itself. The kernel itself uses no
unstable language features; it would build on stable.

### Why xtask

A cargo alias (`cargo xtask ...`) runs a small Rust program that drives the
build. The alternatives were a Makefile or shell scripts; xtask keeps
everything in Rust, works anywhere cargo does, and can do real work (writing
cpio archives, FAT32 file systems and GPT tables) without external tools.
The workspace's `default-members` is just xtask, so a bare `cargo build`
doesn't try to build the kernel for the host.

### Profiles

Both profiles use `panic = "abort"`: there's no unwinding in a kernel. The
release profile keeps debug info (useful with gdb). User programs are always
built optimized and without debug info, because they go into the initrd
where size matters: with debug info eliutils was 7 MB, without it 270 KB.

## 3. Booting: firmware to kernel

### Why UEFI, and why our own bootloader

PCs start in one of two ways: legacy BIOS (16-bit real mode, from 1981) or
UEFI (the modern standard; every PC from the last decade has it, and many
new ones have dropped BIOS support). UEFI starts us in 64-bit mode with
paging on, gives us a memory map, a framebuffer and the ACPI tables through
clean interfaces, and reads FAT file systems for us. There was no reason to
write 16-bit real-mode code in 2026.

Existing bootloaders (Limine, GRUB) would have worked, but writing our own
was a stated goal, and a UEFI loader is a manageable amount of code
(about 1,200 lines).

### UEFI bindings (`boot/src/uefi.rs`)

UEFI is a C interface: tables of function pointers. The bindings declare
the tables eliboot uses as `#[repr(C)]` structs with `extern "efiapi"`
function pointers. Functions eliboot never calls are declared as `usize`
placeholders: they only have to occupy the right slot so later fields land
at the right offsets. This keeps the bindings short while staying faithful
to the specification's layouts.

The system table pointer is kept in an `AtomicPtr` rather than a
`static mut`. After `ExitBootServices` it's set to null, so any later attempt
to print through the firmware becomes a no-op instead of a jump into memory
the firmware no longer owns.

Strings for the firmware are UCS-2 with CRLF line endings; the console
converts in fixed 128-unit chunks so no allocator is needed.

### What eliboot does (`boot/src/main.rs`)

1. **Disables the watchdog.** UEFI reboots the machine if a boot application
   runs for five minutes, which is easy to hit while debugging.
2. **Checks for 5-level paging.** If the firmware left it on, our 4-level
   tables would be meaningless; it refuses rather than crashing.
3. **Finds the framebuffer** through the Graphics Output Protocol. Only the
   RGB and BGR 32-bit formats are accepted: bitmask formats are vanishingly
   rare and "blit-only" modes have no framebuffer at all.
4. **Finds the ACPI RSDP** in the configuration tables, preferring the ACPI
   2.0 version (which has the 64-bit XSDT).
5. **Loads the kernel ELF** from `\EFI\eliOS\kernel.elf` on the same volume
   eliboot came from (found through the Loaded Image protocol's device
   handle).
6. **Loads the initrd** from `\EFI\eliOS\initrd.cpio`, if present. It's
   optional: without it the kernel still boots, which helps debugging.
7. **Builds the kernel's initial page tables** (below).
8. **Allocates the kernel stack** (64 KiB) and the `BootInfo` block.
9. **Exits boot services** and jumps to the kernel.

### Loading the kernel

eliboot has its own small ELF parser (validated with bounds checks
throughout; a malformed kernel produces an error, not a crash). For each
`PT_LOAD` segment it allocates fresh zeroed pages, copies the file contents
in (the zeroing takes care of `.bss`), and maps the pages at the segment's
virtual address with exactly its permissions: writable only if the segment
says so, executable only if the segment says so (W^X).

The kernel's linker script (`kernel/linker.ld`) produces three segments
(r-x code, r-- read-only data, rw- data) at page-aligned boundaries
specifically so they can be mapped with different permissions.

### Memory types

Everything eliboot allocates for the kernel uses UEFI memory types from the
range reserved for OS loaders (0x80000000 and up): kernel, kernel stack,
page tables, boot info, initrd. When eliboot translates the final memory map
for the kernel, those regions come out labeled (`MemoryKind::Kernel` and so
on), so the kernel knows exactly what each region holds and when it can be
freed. Firmware memory is labeled "bootloader reclaimable".

### The initial page tables

The kernel starts on page tables eliboot builds:

- **An identity map of all RAM** (virtual address = physical address).
  eliboot is running from identity-mapped memory when it loads CR3, so the
  instruction after the switch must still be mapped. The kernel removes this
  map early.
- **A direct map of all RAM at `0xFFFF_8000_0000_0000`** ("higher-half direct
  map"), non-executable. The kernel uses it to reach any physical address.
- **The kernel image at `0xFFFF_FFFF_8000_0000`**, the top 2 GiB, which the
  `kernel` code model requires.

The maps use 2 MiB pages. 1 GiB pages would be smaller still, but QEMU's
default CPU model doesn't support them and eliboot shouldn't depend on it.

How much to map: everything up to the end of the highest RAM-like region
(and the framebuffer), but not MMIO or "reserved" regions. That matters:
QEMU's memory map includes a reserved hole at 1 TiB, and mapping up to it
would have cost megabytes of page tables.

The NX bit is only set if the CPU supports it, and `EFER.NXE` is enabled
just before the switch (setting NX bits without NXE makes them "reserved
bits", which fault).

### Exiting boot services

The memory map buffer is allocated with room for 128 extra entries, because
allocations after sizing it (including the buffer itself) can split
regions. The map is fetched immediately before `ExitBootServices`, with
nothing in between that could change it. If the call still fails (firmware
events can allocate), the specification says to fetch the map again and
retry once; eliboot does.

After that, there's no firmware: no console, no allocator, no way to report
errors. eliboot disables interrupts, converts the memory map into eliOS's
format (sorted, adjacent regions of the same kind merged), fills in
`BootInfo`, and jumps. Only the firmware's **runtime services** survive
(see below); the memory they use is marked so the kernel never reuses it,
and device registers they need get their own kind
(`FirmwareRuntimeMmio`) so the kernel knows to map them for them.

### Runtime services, after boot (`kernel/src/efi.rs`)

A small part of UEFI keeps working after boot: the clock, reset, and
**variables**, a little non-volatile storage holding settings such as the
firmware's boot entries. eliOS uses the variables so the installer can add
a boot entry, as Linux's `efibootmgr` does.

Runtime code is still at its physical addresses: the OS may call
`SetVirtualAddressMap` once to relocate it, but eliboot deliberately
doesn't, because that's permanent and some firmware handles it badly. So
calls go through a separate set of page tables: the kernel's half (shared,
so the calling code, its stack and its buffers are all there) plus the
firmware's runtime memory and device registers mapped at their physical
addresses. A call:

1. takes a lock (firmware may only be entered by one CPU at a time; a
   spinlock, so interrupts are off throughout),
2. saves the user program's FPU registers (firmware may use SSE, and the
   kernel itself never does, so those registers belong to the program),
3. switches to the firmware's page tables,
4. switches to a private 64 KiB stack (firmware's stack needs aren't
   documented, and a kernel thread's stack is smaller), loads the arguments
   into `rcx`, `rdx`, `r8`, `r9` and the stack, above 32 bytes of "shadow
   space", as the Microsoft x64 convention UEFI uses requires, and calls,
5. switches everything back.

Every pointer passed must point into kernel memory, since only the kernel's
half is mapped during the call. The first version passed a program's
buffer straight through and the firmware faulted reading it; buffers are
now always copied into the kernel first.

### The jump

```
mov cr3, <page tables>
mov rsp, <kernel stack top>
xor ebp, ebp
push 0
jmp <kernel entry>
```

The `push 0` is a fake return address: it makes the stack look as if the
kernel's entry function was called normally (the System V ABI expects
`rsp + 8` to be 16-byte aligned at function entry), and the zero terminates
stack traces. `boot_info` is passed in `rdi`, the first argument register,
so the kernel's entry point is an ordinary `extern "sysv64" fn(&BootInfo)`.

### The boot protocol (`bootinfo/src/lib.rs`)

`BootInfo` is the contract between eliboot and the kernel: a `#[repr(C)]`
struct with a magic number and version (so a mismatched kernel and
bootloader fail clearly), the direct map offset, the page table root, the
memory map, the framebuffer, the RSDP, the initrd, the kernel command
line, the GUID of the partition eliboot was loaded from, and the address of
the firmware's runtime services. All pointers in it are direct-map
addresses, so they stay valid after the kernel drops the identity map.

The **command line** is one line of text from `\EFI\eliOS\cmdline.txt`, if
it exists: `data=PARTUUID=<guid>` there names an installed system's data
partition (section 22), and `generation=N` (which eliboot's menu adds)
which generation of the system to start. The **boot partition** comes from the
firmware's device path for eliboot's volume (its "hard drive" node carries
the GPT partition GUID), so the kernel mounts the partition it actually
booted from at `/disk`, not just the first FAT partition it sees (which,
on a PC with Windows, would be Windows's). The crate's documentation specifies the machine state at entry
precisely; it's the one place both sides agree on.

## 4. Kernel startup, step by step

`kernel/src/main.rs`. The order is dictated by dependencies:

1. **Record the boot timestamp** (TSC), so uptime counts from here.
2. **Serial port.** The first output channel, needing nothing else.
3. **Validate `BootInfo`.**
4. **GDT, TSS, IDT, `syscall` MSRs, FPU.** From here, CPU exceptions produce
   reports instead of triple faults (which silently reset the machine).
5. **ACPI tables.** Must happen while eliboot's identity map is still active:
   ACPI tables can be anywhere in the low 4 GiB, and the kernel's own direct
   map only covers RAM. This step can't allocate (there's no heap yet).
6. **Memory:** the frame allocator, then the kernel's own page tables, then
   the switch to them.
7. **Switch to a kernel stack with a guard page** (`switch_stack` into
   `kernel_main`). The bootloader's stack had no guard.
8. **Reclaim the bootloader's memory** (now unused: its page tables, its
   stack, firmware boot services memory).
9. **Start the scheduler**, turning the running code into the first thread.
10. **Console** on the framebuffer (needs the heap for its shadow buffer),
    then replay everything logged so far onto it.
11. **Interrupt controllers** (legacy PIC off, local APIC, I/O APIC), then
    **timers**, then the **keyboard**.
12. **Drivers:** start the network stack's thread; PCI scan; AHCI and NVMe
    disks (each registered under a Linux-style name), the network card, USB
    controllers (waiting briefly for devices present at boot).
13. **File systems:** mount the root: the ext2 partition the command line
    names (waiting up to ten seconds for it to appear), or else the initrd
    unpacked into memory; free the initrd's memory; mount `/dev`, `/proc`
    and `/sys` (and, on a disk root, fresh in-memory `/tmp` and `/run`);
    mount the boot partition at `/disk`; wait up to three seconds for DHCP.
    Drivers come first now because the root may be on a disk.
14. **Serial input and the keyboard thread**, then enable interrupts.
15. **Start `/sbin/init`** and let the boot thread exit.

In a test build, step 15 is replaced by running the tests.

## 5. Output: serial, console, font, log

### Serial (`serial.rs`)

The 16550 UART on COM1 is the first thing initialized and the last resort
for debugging: it works without memory management, without interrupts, and
in headless QEMU it's the terminal. It runs at 115200 baud, 8N1. Output
converts `\n` to `\r\n`. The panic handler builds a fresh, unlocked handle
to the port, because the panic may have happened while the normal lock was
held.

### The framebuffer console (`console.rs`)

Text is drawn by the kernel itself into the UEFI framebuffer, one 8x16 glyph
per character cell.

**The shadow buffer.** All drawing goes into a copy of the screen in RAM;
`flush` copies only the changed rows (tracked as a dirty range) to video
memory. The reason is scrolling: scrolling means moving every pixel up one
line, which requires *reading* the screen, and video memory is extremely
slow to read on real hardware (it's mapped uncached or write-combining).
With the shadow buffer, video memory is only ever written. The shadow buffer
is allocated on the heap at the exact screen size. (Before the heap existed
it was a 9 MB static capped at 1920x1200; moving it to the heap was one of
the first uses of the heap.)

**ANSI escape sequences.** The console interprets the standard terminal
escape codes: colors (16-color, 256-color mapped to the palette, and 24-bit
truecolor), cursor movement and positioning, erasing (`J`, `K`), and the
cursor show/hide sequence. This means kernel log colors work identically on
screen and over serial, and, more importantly, ordinary terminal programs
like `vi` and `top` work on the console. Unsupported sequences (including
escape sequences with intermediate bytes, like `ESC ( B`) are consumed and
ignored rather than printed; a test caught the case where they weren't.

**Line wrapping** is "deferred", as on real terminals: writing the last
column leaves the cursor past the end, and the wrap happens on the *next*
character. Otherwise a full-width line followed by a newline would produce a
blank line.

**The cursor** is a block (the cell with its colors swapped) drawn only
into video memory, never the shadow buffer, so it can't get scrolled or
overwritten; the shadow copy of a cell is what erases it. It's shown
unless a program hides it (`ESC [?25l`), and a kernel thread flashes it
every 530 ms (the usual terminal rate) so it's easy to find; output turns
it back on, so it stays solid while someone types.

### The font (`kernel/font/eli-8x16.txt`, `kernel/build.rs`)

The font is eliOS's own, drawn as ASCII art (`#` and `.`), 95 printable ASCII
characters plus a fallback box. The build script parses the file into glyph
tables at compile time, with line-numbered errors for malformed glyphs. ASCII
art was chosen so the font can be edited in any text editor and reviewed in
a diff. Metrics: capitals on rows 3-12, lowercase 6-12, descenders to 14,
column 7 always empty as letter spacing.

### Logging (`log.rs`)

`info!`, `warn!`, `error!` and `debug!` print timestamped, colored lines.
Debug messages go to serial only, to keep the screen readable (the boot
memory map, for instance, is debug output). Once `/sbin/init` starts, info
messages stop reaching the screen too (they still go to serial, `dmesg`
and the log file), like Linux's console log level: a driver logging while
the shell waits for input would otherwise push the prompt up and leave the
cursor on an empty line. Warnings and errors still show. Commands that
start slow work report its outcome themselves (`wifi connect` prints the
address DHCP gives).

Everything printed is also kept in a 64 KiB ring buffer. This solves an
ordering problem: the console can't exist until the heap does, but the
kernel logs things before that. When the console starts, it replays the
buffer, so the screen shows the whole boot. The same buffer backs `dmesg`
(through an eliOS system call).

**The log on disk (`logfile.rs`).** Laptops have no serial port, and
problems with drivers for real hardware (Wi-Fi, Ethernet, touchpads) only
show up on the machine that has it. So a second, 512 KiB ring keeps
*everything*, debug messages included, as plain text (color escapes
stripped as it's written), with bytes numbered from boot so a reader can
ask for what it hasn't seen. A kernel thread appends what's new to
`/disk/logs/boot.log` every two seconds, and once more before a restart or
power off; the previous boot's log is renamed to `previous-boot.log`. A
hang loses at most two seconds, and the stick can be read on any other
computer. It's on for the live system and off for installed ones (whose
`/disk` is the computer's EFI partition), and `savelog`/`savelog=0` on
the command line override that. Writing happens in the thread, never in
the logging call: logging can happen anywhere, including with spinlocks
held, and disk I/O sleeps.

## 6. The CPU: descriptor tables and interrupts

### GDT and TSS (`arch/gdt.rs`)

64-bit mode mostly ignores segmentation, but the CPU still requires a GDT
with code and data descriptors for each privilege level, and a TSS. The
layout is fixed by `syscall`/`sysret`, which compute the user selectors from
one base: user data must directly precede user code. So: null, kernel code
(0x08), kernel data (0x10), user data (0x1B), user code (0x23), TSS (0x28,
two slots).

The TSS holds the kernel stack pointer the CPU switches to when an interrupt
arrives in user mode (`rsp0`, updated on every thread switch), and three
Interrupt Stack Table entries: separate, known-good stacks for double faults,
NMIs and machine checks. Those exceptions can happen when the current stack
is unusable, most commonly a kernel stack overflow: the page fault from
hitting the guard page can't be delivered on the overflowed stack, so the
CPU raises a double fault, which runs on its own IST stack and produces a
readable report ("the stack overflowed") instead of a triple fault.

Each CPU has its own TSS (it holds that CPU's stacks) and therefore its own
GDT. The boot CPU's live in static storage, because it sets them up before
the heap exists; the others' are allocated, with guard-paged IST stacks.
The IDT is shared: every CPU handles every vector the same way.

### Per-CPU data (`arch/cpu.rs`)

Each CPU has a `Cpu` block: the running thread's kernel stack and a scratch
slot for the system call entry code (at fixed offsets the assembly uses),
the CPU's index and APIC ID, its TSS, the running thread, and flags other
CPUs set to ask it for something (switch threads, flush its TLB). The GS
base register points at the block while in the kernel, and the block
stores its own address at offset 16, so `cpu::current()` is a single
`mov rax, gs:[16]`.

The answer is only meaningful while the thread can't move to another CPU,
so per-CPU lookups that matter happen with interrupts disabled (preemption
only happens in interrupts).

### Starting the other CPUs (`arch/smp.rs`)

The firmware leaves every CPU but one halted, waiting for two signals sent
through the local APIC: INIT, then STARTUP with a page number. STARTUP
starts the CPU in 16-bit real mode at that page, which must be below 1 MiB.
(This is why the frame allocator never hands out low memory.)

eliOS copies a small trampoline there. In about 30 instructions it loads a
temporary GDT, enables PAE and global pages, loads page tables, turns on
long mode and no-execute in EFER, and then sets the protection *and*
paging bits in CR0 at once: real mode straight to long mode, skipping
32-bit protected mode entirely (it works, and it halves the trampoline). A
far jump then enters 64-bit code. The trampoline's page tables are built
next to it, also below 1 MiB (CR3 can only hold a 32-bit address at that
point): they map the first 2 MiB at their own addresses, so the code keeps
running after paging turns on, and copy the kernel's top-level entries, so
the kernel is mapped too. The trampoline then jumps to `ap_main` on a stack
the boot CPU allocated.

`ap_main` switches to the kernel's own page tables, copies the boot CPU's
CR0 and CR4, and sets up everything per-CPU: the GS base, the page
attribute table, the GDT and TSS, the IDT, the `syscall` MSRs, the FPU,
the local APIC and its timer. The code it's running becomes that CPU's idle
thread.

The boot CPU starts the others one at a time, following Intel's documented
sequence (INIT, 10 ms, STARTUP, 200 µs, STARTUP again if needed) and
waiting for each to report in. CPUs with APIC IDs above 254 are skipped;
they'd need x2APIC mode.

### Interrupt entry (`arch/interrupts.rs`)

All 256 vectors enter through assembly stubs, generated with an assembler
`.rept` loop. Each stub pushes a zero error code (for vectors where the CPU
doesn't push one), pushes its vector number, and jumps to common code. The
stubs are padded to exactly 16 bytes, so stub `n` is at
`interrupt_stubs + 16n` and no table of addresses is needed. (This was
checked by disassembling: stubs for vectors above 127 need a longer `push`
encoding and still fit.)

The common code saves all 15 general-purpose registers. The result on the
stack is an `InterruptFrame`: the complete register state of the interrupted
code, in a fixed layout. This one structure is central to the kernel:

- exception reports print it;
- system calls build the same frame (section 11), so system calls,
  interrupts and exceptions all describe user state the same way;
- `fork` copies it; `execve` rewrites it; signal handlers save and restore
  it;
- new user threads start by "returning" through a hand-built one.

The `rustc`-provided `extern "x86-interrupt"` ABI would have been an
alternative, but it gives each handler only the CPU-pushed part of the
state and requires nightly; the hand-written stubs give the full frame.

Before calling Rust, the entry code clears the direction flag (the ABI
requires it clear at function entry) and, if the interrupt came from user
mode (checked from the saved CS), runs `swapgs` (section 11). The frame is
22 words, which keeps the stack 16-byte aligned for the call.

### Exceptions

Breakpoints (`int3`) are counted and return. Every other exception in
kernel mode is fatal and produces a report: the exception's name and
mnemonic, the error code, all registers, and for page faults a decoded
explanation ("write of 0xdead: page not present") plus a hint from the
memory manager when the address is recognizable (the null page, a stack
guard page). Exceptions in user mode become signals (section 13).

### Interrupt controllers (`arch/pic.rs`, `arch/apic.rs`)

**The legacy 8259 PICs** are reprogrammed so their vectors don't collide
with CPU exceptions (by default they do), then fully masked. Spurious
interrupts can still arrive on their vectors, so those vectors are
recognized and ignored.

**The local APIC** (one per CPU) delivers interrupts and has a timer. It's
used in xAPIC mode (memory-mapped registers), which every x86_64 CPU and
every hypervisor supports; x2APIC (MSR-based) would be needed beyond 255
CPUs. Setup: LINT0 (the legacy PIC's "virtual wire") masked; LINT1 set as
the NMI input if the MADT says so; an error vector; the spurious vector
(0xFF) with the APIC enabled.

**The I/O APIC** routes device interrupt lines to vectors. All its inputs
start masked. Legacy ISA interrupts (keyboard IRQ1, serial IRQ4) aren't
always wired to the input with the same number: the MADT's interrupt source
overrides say where each one really goes and with what polarity and trigger
mode. `route_isa_irq` applies them.

### Vector assignments

| Vectors | Use |
|---|---|
| 0-31 | CPU exceptions |
| 0x20-0x2F | the masked legacy PICs (spurious only) |
| 0x30 + IRQ | I/O APIC interrupts (keyboard 0x31, serial 0x34) |
| 0xF0 | local APIC timer |
| 0xF1 | reschedule request from another CPU |
| 0xF2 | TLB shootdown request from another CPU |
| NMI | after a panic, stops the other CPUs |
| 0xFE | local APIC error |
| 0xFF | spurious |

### Dispatch

`interrupt_dispatch` sends exceptions to their handlers and other vectors
to registered handler functions (stored as addresses in atomics, so
registration needs no lock). After a device interrupt it sends the APIC its
end-of-interrupt, *then* gives the scheduler a chance to switch threads
(section 10), and if returning to user mode, acts on pending signals.

## 7. Time

### Calibration (`time.rs`)

The kernel needs two clocks: the TSC (a per-CPU counter that's cheap to
read, for timestamps and uptime) and the local APIC timer (for periodic
interrupts). Neither runs at a frequency known in advance. Both are measured
against the PIT, the ancient 8254 timer whose input clock is fixed at
1.193182 MHz.

PIT channel 2 can be used without interrupts: its gate and output are
visible in I/O port 0x61. The kernel programs it for about 50 ms (one-shot),
starts the APIC timer counting down from its maximum, reads the TSC, waits
for the PIT's output to go high, and reads both again. The differences give
both frequencies. 50 ms is long enough for good precision and short enough
not to slow the boot noticeably.

The APIC timer then fires every millisecond (1 kHz): the scheduler's tick.
Every CPU's timer runs at the rate measured on the boot CPU (they share the
bus clock), and each drives its own CPU's time slices; the boot CPU's
also wakes sleeping threads whose time has come. Uptime comes from the TSC,
with nanosecond resolution, and so do the kernel's deadlines: a "tick"
(`time::ticks()`) is a millisecond of TSC time, not a counted interrupt.
Counting interrupts made time run slow whenever they arrived late (in an
emulator under load, `alarm(1)` rang after two seconds); now a late
interrupt simply finds everything that came due meanwhile.

The TSC on older CPUs (and in QEMU's default CPU model) isn't "invariant":
it can change speed with power states. The kernel warns but uses it anyway;
in QEMU it's stable.

### Wall-clock time (`rtc.rs`)

The CMOS real-time clock is read once at boot (twice in a row until two
reads agree, so a read can't straddle an update; BCD and 12-hour formats are
handled). Wall-clock time is that plus uptime. The clock is assumed to hold
UTC, as on most non-Windows systems.

## 8. Memory

### Physical frames (`memory/frames.rs`)

One bit per 4 KiB frame of physical memory; the bitmap lives at the start of
the first free region big enough to hold it. The alternatives:

- a free list (each free frame stores a pointer to the next) is O(1) but
  can't find physically contiguous runs, which device drivers need for DMA;
- a buddy allocator is the classic answer to both, but is considerably more
  code.

The bitmap supports both single frames and aligned contiguous runs.
Single-frame allocation is "next fit": it continues from where the last one
succeeded, so it usually finds a free frame in the first 64-bit word
checked. Freeing checks the bit and panics on a double free, which turns a
whole class of memory corruption bugs into immediate, explained failures.

Memory below 1 MiB is never handed out: starting other CPUs needs a page
there (they start in 16-bit mode), and firmware leftovers like to live
there.

**Owner counts.** A frame can belong to several places at once: a page
shared by a parent and child after `fork`, or a file's page held by its
page cache and mapped into every program using the library. Next to the
bitmap is a 32-bit count per frame (16 MiB for 16 GiB of RAM). `allocate`
sets it to 1, `share` adds an owner, and `free` drops one and frees the
frame with the last, so code that frees what it allocated works as
before, and a frame lives exactly as long as someone uses it.

### Memory objects (`memory/object.rs`)

A memory object is a numbered set of pages that can be mapped into
address spaces and also read and written like a file. One object backs
several things at once: an in-memory file's data lives in one (so `read`,
`write` and every `MAP_SHARED` mapping see the same bytes); `memfd_create`
and `MAP_SHARED | MAP_ANONYMOUS` memory are objects without a file; and a
file on a disk gets one as its **page cache**, read from the file a page at
a time when first needed and shared by every program that maps it (a
browser's 100 MB library is in memory once, not once per process). Pages
are made on first use: zeroed, or read from the file. When a disk file is
written or truncated its page cache is forgotten, so later mappings see
the change (mappings that already exist keep the old pages, a
simplification compared with Linux).

Using objects for in-memory files also removed their old 64 MiB limit
(they were a growable `Vec` each); a write now fails with `ENOSPC` only
when free memory runs low.

### The kernel's page tables (`memory/mod.rs`, `memory/paging.rs`)

Early in boot the kernel builds its own page tables and switches to them.
Why not keep eliboot's? Because they contain the identity map, which makes
null pointers and other low addresses silently valid; and because the kernel
needs to know and control everything mapped.

The new tables have:

- **The kernel image**, section by section, with permissions from the linker
  script's boundaries: code read-only and executable; read-only data
  non-executable; data writable and non-executable. All marked *global*, so
  they survive address space switches in the TLB.
- **The direct map** at the same address eliboot used (so pointers made
  before the switch, like `BootInfo`, stay valid), but now covering only RAM
  (plus the first megabyte, where firmware tables can hide). Device memory is
  deliberately absent: it gets mapped individually with the right caching.
- **Nothing at all in the lower half.** Null pointer dereferences fault.

`map_range` uses the largest pages each piece's alignment allows (1 GiB if
the CPU supports it, else 2 MiB, else 4 KiB), which keeps the direct map to
a handful of page table pages.

**Pre-created top-level entries.** All 256 top-level (PML4) entries for the
kernel half are created at boot, even though most of the regions are empty.
Every user address space copies these 256 entries. If a later kernel mapping
needed a *new* top-level entry, existing address spaces wouldn't see it; with
all of them created up front, every kernel mapping made at any time is
visible in every address space automatically. Cost: 256 pages (1 MiB).

**Kernel address space layout:**

| Start | Region |
|---|---|
| `0xFFFF_8000_0000_0000` | direct map of RAM |
| `0xFFFF_C000_0000_0000` | kernel heap |
| `0xFFFF_D000_0000_0000` | device memory (framebuffer, registers) |
| `0xFFFF_E000_0000_0000` | kernel stacks, each with a guard page |
| `0xFFFF_FFFF_8000_0000` | the kernel image |

Each region is a whole 512 GiB top-level entry, and the heap, device and
stack regions are bump-allocated: simple, and with 512 GiB each, exhaustion
isn't a practical concern.

### Device memory and caching

`map_device(phys, len, cache)` maps registers and framebuffers into the
device region. Caching matters: registers must be uncached (every access
must reach the device), and framebuffers should be write-combining (writes
are batched into bursts, which on real GPUs makes drawing several times
faster). x86 selects a memory type through the Page Attribute Table; at boot
the kernel reprograms PAT entry 1 from write-through to write-combining, so
a page table entry with only the PWT bit set means write-combining.

### Kernel stacks and guard pages

Each kernel stack is allocated with one unmapped page below it. Overflowing
the stack touches the guard page and faults, instead of silently overwriting
whatever happened to be below. The boot stack (from eliboot) had no guard,
so the kernel switches to a guarded stack as soon as its page tables are up.

### TLB shootdowns

The CPU caches page translations in its TLB, and each CPU has its own.
When the kernel removes or restricts a mapping, its own CPU flushes the
entry, but another CPU may still hold the old translation: if the memory
were then reused, that CPU could keep reading or writing it. So every
unmapping (heap pages, kernel stacks, `munmap`, `mprotect`) calls
`smp::flush_tlb(range)`, which flushes locally, flags each other CPU,
sends them all an interrupt, and waits until every one has flushed. Only
then is the memory freed.

A subtle deadlock lurks here. A CPU spinning on a spinlock has interrupts
disabled, so it never sees the shootdown interrupt; if the lock's holder is
the CPU waiting for the shootdown, neither moves. The fix: the spinlock's
wait loop also serves pending shootdown requests. Small ranges are flushed
page by page with `invlpg`; large ones flush everything by toggling CR4's
global-pages bit (which also drops global, kernel, translations).

A related, gentler case: when a page gains permissions (an `mprotect`
that adds write access, say), other CPUs may still hold the old read-only
translation and fault on a write that's now allowed. The page fault handler
recognizes this (the page table entry permits the access) and just flushes
the stale entry.

### Reclaiming boot memory

Once the kernel runs on its own page tables and stack, eliboot's page
tables, eliboot's stack and the firmware's boot-time memory are unused. The
kernel hands those regions to the frame allocator: in QEMU about 48 MB, a
fifth of the 256 MB machine. The initrd's memory is likewise freed after
it's unpacked. ACPI regions are kept: the tables are needed later (power
off reads them).

### The heap (`memory/heap.rs`)

`alloc` (and so `Vec`, `Box`, `String`, `BTreeMap`) needs a global
allocator. The design:

- **Small allocations (up to 2 KiB)** come from power-of-two size classes:
  16, 32, ..., 2048 bytes. Each class has a free list threaded through the
  free blocks themselves; an empty class takes a fresh 4 KiB page and splits
  it. Blocks are aligned to their size, which covers any alignment up to
  that size.
- **Large allocations** get whole pages in the heap region, each backed by
  its own physical frame (so they needn't be physically contiguous), and are
  unmapped and returned to the frame allocator when freed. The virtual
  ranges are reused first-fit from a list of freed ranges, merged with
  their neighbors. Unmapping kernel memory means a TLB shootdown, which
  interrupts every CPU, and system calls allocate buffers of a few pages
  constantly (a `recvmsg`, a `read`), so freed runs of up to 16 pages are
  kept mapped, up to 64 pages' worth per length, and handed out again
  first. Profiling Firefox found this: heap frees were the kernel's top
  cost.

The trade-off: small-block pages are never returned to the system (blocks
are only reused), in exchange for a very simple, fast design. Large blocks,
where returning memory matters, are returned immediately.

## 9. Locks and other synchronization

`sync.rs`.

- **`SpinLock`** is an atomic flag other CPUs spin on, and it also disables
  interrupts while held (restoring the previous state when released).
  Without that, an interrupt handler that takes a lock the code it
  interrupted holds would spin forever on that CPU. Making *every* spinlock
  interrupt-safe removes that class of deadlock entirely. The rules that
  follow: never block while holding a spinlock, and never allocate while
  holding a lock the heap itself needs (the kernel page table lock: the heap
  takes it to grow, so allocating under it deadlocks, which a test caught).
  Spinning CPUs serve TLB shootdown requests (section 8).
- **`Mutex`** puts waiting threads to sleep on a wait queue, with interrupts
  left on. It's for slow operations, like disk I/O, where holding interrupts
  off for milliseconds would delay the timer.
- **`Once`** holds a value set once at runtime and read forever after (the
  parsed ACPI tables, for instance).
- **`TakeOnce`** hands out a `&'static mut` to a static exactly once, which
  gives safe access to static storage (the GDT, IDT, TSS) without
  `static mut`.
- **`ByteQueue`** is a lock-free single-producer single-consumer ring of
  bytes, for interrupt handlers handing data to threads (keyboard
  scancodes).

## 10. Threads and the scheduler

`task/mod.rs`, `task/wait.rs`.

### Context switching

Each thread has its own kernel stack. `switch_context(save, load)` (a few
lines of assembly) pushes the callee-saved registers (rbx, rbp, r12-r15),
stores the stack pointer, loads the other thread's, pops its registers and
returns. That's the whole switch: everything else a thread was doing,
including an interrupt it was in the middle of handling, is already on its
stack.

A new thread gets a hand-made stack that looks as if `switch_context` had
been called, with the "return address" pointing at a trampoline. The
trampoline takes the thread's closure (from a register) and calls
`thread_start`, which enables interrupts, runs the closure and exits the
thread. The return address slot is placed so the stack ends up 16-byte
aligned at the `call`, as the ABI requires.

Beyond the stack, a switch also swaps the per-thread state user programs
can see: FPU/SSE registers (`fxsave`/`fxrstor`), the FS base (user
thread-local storage), CR3 (the address space, only if it differs) and the
kernel stack the CPU uses on entry from user mode.

### Scheduling policy

Round-robin with 10 ms time slices: the simplest fair policy. There's one
ready queue, which every CPU takes threads from, so work naturally spreads
across CPUs. Each CPU's timer interrupt (1 kHz) decrements that CPU's
slice, and the boot CPU's also wakes sleeping threads whose time has come;
when a switch is due it only sets a per-CPU flag.

When a thread becomes ready and some CPU is idle, that CPU is "kicked":
its flag is set and it's sent a reschedule interrupt, which wakes it from
`hlt`. (Without that, an idle CPU would notice new work at its next timer
tick, up to a millisecond later.)

One queue under one lock is simple and fair; its cost is contention, which
per-CPU queues with work stealing would avoid. At eliOS's scale (a few
CPUs, short critical sections) the simple design measures fine.

**The lock is held across the switch.** The CPU switching away takes the
scheduler lock, decides, swaps per-thread state, and calls `switch_context`
*still holding it*; the thread it switches to releases it first thing. If
the lock were released before the switch, another CPU could see the old
thread as ready and start running it on its stack before its registers
were even saved. With the lock held, no CPU can observe a thread in a
half-switched state.

**Where switches happen.** The flag is checked at the end of interrupt
handling, after the end-of-interrupt has been sent to the APIC, and before
returning to user mode. Switching from inside a handler before the EOI
would leave the APIC thinking the interrupt was still in service, blocking
other interrupts of equal or lower priority until this thread ran again.

Because spinlocks disable interrupts, a thread can never be preempted while
holding one.

### The idle thread

Each CPU has an idle thread, run when nothing is ready; it executes
`sti; hlt` in a loop.
The two instructions execute as a unit (`sti` takes effect after the next
instruction), so an interrupt can't slip in between them and be missed. If
an interrupt makes a thread ready, the switch happens on its way out.

### Sleeping, blocking and wait queues

- `sleep_ms` blocks the thread with a wake-up tick; the timer wakes it.
- **Wait queues** are how threads wait for events (a key press, data in a
  pipe, a child exiting). The danger is the classic "lost wakeup": the
  event arrives between checking the condition and going to sleep, and the
  thread sleeps forever. On one CPU, disabling interrupts around the check
  is enough. With several CPUs it isn't: the event can come from another
  CPU at any moment. So `wait_until(condition)` adds the thread to the
  queue *first*, then checks the condition, then blocks. A wakeup that
  comes after the check finds the thread in the queue. If the thread hasn't
  actually blocked yet, the wakeup leaves a "wake pending" mark, and the
  attempt to block returns at once, so the thread rechecks. Whoever changes
  the condition calls `wake_all`; woken threads recheck it and go back to
  sleep if it still doesn't hold. Waits can also have a deadline (for
  `poll` timeouts).

A kernel test bounces a token between two threads 2,000 times through a
wait queue; a single lost wakeup would hang it.

### Exiting

A thread can't free its own stack while running on it, and with several
CPUs, can't be freed while any CPU is still on its stack. Exited threads go
on a list and are freed after a later switch, once no CPU's current thread
is one of them.

## 11. Entering and leaving user mode

`arch/syscall.rs`, `arch/fpu.rs`.

### System call entry

User programs enter the kernel with the `syscall` instruction. It jumps to
the address in an MSR, switches to ring 0, saves the return address and
flags in rcx and r11, and *does not switch stacks*. So the entry code must
find the kernel stack without a free register, using `swapgs`: GS normally
points at a per-CPU block in the kernel; `swapgs` exchanges it with the
user's GS value. The entry code then:

1. `swapgs`, save the user stack pointer in the per-CPU block, load the
   current thread's kernel stack from it;
2. push exactly what an interrupt from user mode would have pushed (user
   SS, RSP, RFLAGS, CS, RIP), a zero error code and a vector of -1 (meaning
   "system call");
3. save all registers into an `InterruptFrame`;
4. call `syscall_dispatch`, which enables interrupts (system calls are
   preemptible), runs the call and stores the result in the frame's rax;
5. leave through the same `interrupt_return` code interrupts use.

Building the same frame as interrupts means every user transition is
described one way, and returning through `iretq` (rather than `sysret`)
handles every case, including `execve` changing every register, and avoids
`sysret`'s well-known pitfalls with non-canonical return addresses. `sysret`
is faster and could be added as a fast path later.

`interrupt_return` checks the saved CS: if going back to user mode it runs
`swapgs` again just before `iretq`.

### Entering user mode the first time

A new user thread starts with a hand-built `InterruptFrame` (user code and
stack selectors, entry point, stack pointer, interrupts enabled) and jumps
into `interrupt_return` with the stack pointer at it.

### FPU and SSE

The kernel is compiled without floating point (soft-float), so FPU and SSE
registers only ever hold user values. That allows a simple scheme: save and
restore them (`fxsave`/`fxrstor`, 512 bytes) on every thread switch. The
"lazy" alternative (trap on first use) saves time but adds complexity, and
switches are infrequent enough not to matter.

Only the legacy FXSAVE state is managed; XSAVE (for AVX registers) isn't
enabled. That's deliberate: with `CR4.OSXSAVE` off, CPUID tells programs AVX
isn't usable, so glibc and others won't use AVX registers that eliOS
wouldn't save.

New programs start with a clean FPU state (captured at boot after `fninit`,
with the standard MXCSR value).

## 12. Processes

`process/`.

A process is: an address space, a file descriptor table, a working
directory, signal handlers, a process group, and one or more threads.

### Address spaces (`process/address_space.rs`)

An address space is a set of **areas** (ranges with access rights and a
*backing*, kept in a `BTreeMap`) plus the page tables. Pages are mapped on
first touch ("demand paging"), so reserving memory is free: an 8 MiB stack
area costs nothing until used, and programs can `mmap` large regions or
files they only partly touch. What a touched page gets depends on the
backing:

- **private memory** gets a fresh zeroed page;
- **a memory object** gets the object's page: as it is in a shared mapping
  (everyone sees the same bytes), or read-only and marked copy-on-write in
  a private one, so the first write makes the program its own copy;
- **device memory** (a framebuffer) gets the device's page, with its cache
  type; such pages have no owner count and are never freed.

Outside any area, or against its rights, a fault is a segmentation fault.
Copy-on-write and "device" are software bits in the page table entry (bits
9 and 11, which the CPU ignores). Getting an object's page can mean reading
a disk, which mustn't happen with the address space's spinlock held, so
such a fault says which page it needs; the page is fetched with the lock
released and interrupts on, then installed if the area still wants it.

The top 256 page table entries are copied from the kernel's, so the kernel
is mapped (supervisor-only) in every address space, and system calls don't
need to switch page tables.

`madvise(MADV_DONTNEED)` drops a range's pages but keeps the areas: the
next access gets zeros (private memory) or the file's contents again.
Memory allocators (scudo, jemalloc) hand freed memory back this way and
count on it reading as zero afterwards. When `madvise` was ignored,
Firefox's content process built a hash table in "zeroed" memory full of
old data and spun forever probing it. Areas also record what's mapped (the
file's path, `[stack]`, `[heap]`), for `/proc/PID/maps` and fault reports.

### The kernel touching user memory (`process/user.rs`)

System calls receive user pointers. Before touching user memory, the kernel
checks the whole range lies in areas with the needed rights and maps any
untouched pages, copying copy-on-write ones it will write
(`prepare_access`). Then it reads or writes through the user address
directly (the process's address space is the active one).

Another thread can change the memory between the check and the access:
if it calls `fork`, the pages become copy-on-write again, and the kernel's
write faults. So a page fault in kernel mode at a user address is resolved
the same way as a program's own (copying the page), and the kernel retries
the write; a test forks a hundred times while another thread `read`s into
a buffer.

The alternative, used by Linux, is to let the access fault and have an
"exception table" turn faults at known instructions into `EFAULT` errors.
That's faster (no up-front walk) but needs careful compiler cooperation;
the check-first approach is simple and safe for now.

### Loading programs (`process/elf.rs`)

The ELF loader accepts static executables (`ET_EXEC`), position-independent
ones (`ET_DYN`, loaded at a fixed base) and dynamically linked ones (those
with a `PT_INTERP` naming a dynamic linker). For each `PT_LOAD` segment it
creates an area with the segment's rights and copies the file data in.

For dynamically linked programs it also loads the interpreter (glibc's
`ld-linux-x86-64.so.2`) at a fixed address above where `mmap` places things,
and starts *it* instead of the program. The interpreter finds the program
through the auxiliary vector, loads the shared libraries with ordinary
`open`/`mmap`/`mprotect` system calls, applies relocations and jumps to the
program. eliOS needed nothing else to support dynamic linking: the
complicated part is entirely in glibc, which is how Linux does it too.

`#!` scripts run their interpreter with the script as an argument, like
Linux.

### The initial stack

Linux programs expect a precise stack layout at entry, specified by the
System V ABI: `argc`; the `argv` pointers and a null; the `envp` pointers and
a null; the auxiliary vector (key/value pairs); then the strings. The
auxiliary vector tells the C library where the program headers are (`AT_PHDR`,
which glibc needs to set up thread-local storage), the page size, the entry
point, where the interpreter was loaded (`AT_BASE`), 16 random bytes
(`AT_RANDOM`, for stack protectors and pointer mangling), CPU capabilities and
more. Getting this exactly right is what lets unmodified glibc programs
start.

### fork, exec, exit, wait

- **`fork`** shares the address space instead of copying it: every page is
  mapped in the child too (one more owner), and private writable pages
  become read-only and copy-on-write in both. Whichever process writes a
  page first gets a copy; if the other has meanwhile exited (the owner
  count is 1), the page is simply made writable again. Most pages are never
  written before an `exec`, so `fork` costs a page table walk, not a copy of
  the program's memory, which matters for a browser that starts many
  processes. Shared mappings stay shared. `fork` also duplicates the file
  table (sharing the open files, so the position is shared as on Linux),
  copies the signal state, and starts a thread whose frame is the parent's
  with rax = 0.
- **`execve`** builds the complete new address space first. Only when that
  succeeded does it discard the old one, so a failed `exec` leaves the
  process intact, as the API requires. It then closes close-on-exec files,
  resets handled signals to default, and rewrites the system call's frame so
  "returning" from `execve` starts the new program.
- **`exit`** closes files, frees the address space (after switching to the
  kernel's page tables: it can't free the tables it's running on), gives
  children to init, records the status and wakes waiting parents. The
  process stays as a "zombie" until its parent collects it.
- **`wait4`** finds a matching zombie child (by pid, any child, or process
  group, as Linux defines) and removes it, or sleeps until one appears.

Wait statuses use Linux's encoding: exit code in bits 8-15, or the signal
number in the low 7 bits.

### Threads (`process/mod.rs`, `process/futex.rs`)

A thread in a user program is a scheduler thread (section 10) plus a
`UserThread` record: the thread id programs see, its signal mask and
pending signals, its alternate signal stack, and the "clear child tid"
address described below. The process keeps a list of its threads; each
thread points back at its process. Address space, files, working directory
and signal handlers belong to the process, so threads share them by
construction. FPU registers and the FS base (thread-local storage) were
already per scheduler thread.

**Creating threads.** glibc's `pthread_create` calls `clone` (or `clone3`,
tried first) with `CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_SETTLS |
CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID` and a stack it allocated. The
new thread starts from a copy of the caller's system call frame, with rax =
0, the given stack pointer and the given FS base. Thread ids come from the
same counter as process ids, as on Linux, and the first thread's id is the
pid, so `getpid` and `gettid` agree in single-threaded programs.

`clone` without `CLONE_THREAD` makes a new process, and does so by copying
memory even if `CLONE_VM` asks for sharing. Sharing an address space
between *processes* would complicate exit and exec (whose memory is it?),
and the programs that ask for it (`vfork`, glibc's `posix_spawn`) only use
the shared memory in ways a copy also satisfies.

**Futexes.** Every glibc synchronization primitive (mutexes, condition
variables, semaphores, `pthread_join`, `pthread_once`) is an atomic variable
in user memory; the kernel is only asked to sleep (`FUTEX_WAIT`: "if the
variable still holds this value") or to wake sleepers (`FUTEX_WAKE`). The
check and the enqueue happen under one lock, so a wake that follows a
change to the variable can't be missed. Waiters live in a map keyed as on
Linux: a futex in private memory by (process, address), one in shared
memory (`MAP_SHARED`, memfd, `/dev/shm`) by (memory object, offset), so
processes that map it at different addresses still meet; that's what
process-shared mutexes and semaphores need. `FUTEX_PRIVATE_FLAG` skips
looking. Each waiter has its own wait queue, so waking *n* threads wakes
exactly those. The `_BITSET` variants (used by condition variables)
and `REQUEUE`/`CMP_REQUEUE` are supported, with relative or absolute
timeouts on either clock.

**Joining.** `CLONE_CHILD_CLEARTID` (and `set_tid_address`) give the kernel
an address in the thread's descriptor. When the thread exits, the kernel
stores 0 there and wakes one futex waiter at that address. That's all
`pthread_join` needs: it waits on the thread id field until it's 0.

**Ending threads.** `exit` (the system call) ends one thread;
`exit_group` (what C's `exit()` calls) ends them all, and so does a fatal
signal. A process-wide "ending" state records which: the status for
`exit_group`, or for `execve`, the thread that survives. Every other thread
is interrupted out of whatever it's blocked in (blocking calls treat this
like a pending signal) and ends itself on its way back to user mode. The
last thread to leave tears the process down, so nothing is freed while a
thread could still be using it. Before leaving the list, each thread
switches to the kernel's page tables, so the last thread can free the
address space without anyone running on it.

`execve` in a threaded process first loads the new program (so failures
leave the process intact), then ends the other threads and waits until
they're gone. The survivor takes the pid as its thread id.

### Process groups

Each pipeline the shell runs is a process group, and the terminal knows
which group is in the foreground. Ctrl+C sends SIGINT to that group: the
running program is interrupted, the shell (in its own group) isn't.

### init

Process 1 is special: orphans are given to it, and it must never exit (the
kernel panics if it does, because nothing could ever reap orphans again).

## 13. Signals

`process/signal.rs`.

Handlers (a `sigaction` per signal) belong to the process; each thread has
its own blocked mask, alternate stack, and set of signals sent to it in
particular. Signals sent to the process as a whole (`kill`, Ctrl+C) wait in
a process-wide pending set and are taken by whichever thread doesn't block
them; signals sent to one thread (`tgkill`, which `pthread_kill` and
`raise` use) and CPU faults wait for that thread. Signal sets use Linux's
numbering, where
signal *n* is bit *n - 1* (the obvious `1 << n` overflows for signal 64, a
bug that was found by `yes | head`'s SIGPIPE).

**Sending** marks the signal pending and, if it isn't ignored or blocked,
interrupts the target thread if it's sleeping or blocked. Blocking system
calls check for pending signals in their wait conditions and return `EINTR`.

**Delivery** happens just before returning to user mode, after any pending
thread switch (so a thread that waited for the CPU acts on what arrived
meanwhile). A signal with the default action either does nothing (SIGCHLD,
SIGWINCH, ...) or ends the whole process, every thread of it. A signal with a handler gets a **signal frame**:

1. The FPU state is saved on the user stack (64-byte aligned).
2. An `rt_sigframe` is built below it, laid out exactly as Linux's: the
   return address (the C library's "restorer"), a `ucontext` with the saved
   registers, the old signal mask and a pointer to the FPU state, and a
   `siginfo`.
3. The frame's registers are changed to enter the handler with the signal
   number, the siginfo and the ucontext as arguments.

The handler returns into the restorer, which calls `rt_sigreturn`; the
kernel reloads all registers, the mask and the FPU state from the frame.
Because the layout is Linux's, glibc's restorer, `siginfo` handling and
`siglongjmp` all work unmodified. Restored flags are sanitized (only bits a
program may change), and the restored MXCSR has its reserved bits cleared,
since a corrupt value would make `fxrstor` fault inside the kernel.

`SA_RESTART`: if a handler interrupted a system call that returned `EINTR`,
the frame is rewound to re-execute the `syscall` instruction (2 bytes) with
the original call number, so the call transparently restarts after the
handler. Calls Linux never restarts (pause, sigsuspend, sigtimedwait,
sleeps, poll, select, epoll) are left returning `EINTR`.

**CPU faults** in user code become signals: page faults outside any area
and protection faults become SIGSEGV, invalid opcodes SIGILL, division by
zero SIGFPE. If the program handles the signal, the handler runs (that's how
`sigtest` recovers from a null pointer write with `siglongjmp`); otherwise
the process ends and the kernel logs why.

## 14. Users and permissions

`process/credentials.rs`, `fs/mod.rs` (the checks), `user/eli/src/users.rs`,
`user/eli/src/crypt.rs`, `user/utils/src/accounts.rs`, `user/utils/src/auth.rs`.

eliOS follows the classic Unix model, the same one Linux uses, because
every program that cares about users (and every user who has used Linux)
expects it: numeric user and group ids, an owner, a group and nine
permission bits on every file, root (user 0) as the one all-powerful
account, and setuid programs for the few things ordinary users need
privileges for. Linux's finer-grained "capabilities" were left out: they
solve problems a single-user desktop doesn't have.

### Credentials

Each process has a **real**, **effective** and **saved** user id, the same
three group ids, and a list of supplementary groups. The effective ids are
what permission checks use; the real ids say who started the program; the
saved ids let a setuid program give up its privileges for a while and take
them back. The rules for changing them (`setuid`, `setresuid`, `setreuid`,
`setgroups` and the group versions) are Linux's: root may set anything,
everyone else only shuffles among their current three ids. These rules are
small pure functions on a `Credentials` value with their own kernel tests.

A process's credentials are an `Arc<Credentials>` that's replaced, never
changed in place. A permission check takes a snapshot (one reference count
increment) and then needs no lock, and a thread in the middle of a system
call sees one consistent set even if another thread changes them. Kernel
threads act as root.

Credentials are inherited by `fork` and kept by `exec`, except that a
**setuid** (or setgid) program makes the effective id the file's owner (or
group), and the saved id follows. Linux's details are kept: a script's own
setuid bit is ignored (its interpreter's counts), setgid needs the group
execute bit, and a traced program (`strace`) doesn't gain privileges. A
program that gained privileges gets `AT_SECURE` in its auxiliary vector, so
glibc's loader ignores variables like `LD_LIBRARY_PATH` that its caller
controls.

### Permission checks

`fs::check_access` is the one place the rules live: the owner's three bits
apply to the owner, the group's to anyone in the file's group (effective or
supplementary), the rest to everyone else; root may read and write
anything, and execute anything with at least one execute bit. The checks
are made where Linux makes them:

- **Path lookup** needs search (execute) permission on every directory it
  passes through. Root skips the check entirely, so the kernel's own
  lookups and root's cost nothing extra.
- **open** checks read and/or write against the access mode; creating a
  file needs write and search permission on the directory. A file the
  caller just created can be opened however it asked, even with mode 0444,
  as POSIX requires.
- **exec** needs execute permission.
- **Creating, deleting and renaming** need write and search permission on
  the directories involved. In a directory with the **sticky bit** (like
  `/tmp`, mode 1777), only an entry's owner, the directory's owner or root
  may delete or rename it.
- **chmod** is for the owner (or root); **chown** to another user is root's
  alone; an owner may change the group to another group they're in.
  Changing a regular file's owner clears its setuid and setgid bits, and so
  does opening it for writing as anyone but root, so nobody can turn
  someone else's privileged program into their own.
- **access** checks with the *real* ids (so a setuid program can ask
  whether its caller could do something), unless `AT_EACCESS` is given.
- **Signals** can only be sent to processes of the same user (comparing the
  sender's real or effective id with the target's real or saved one), or by
  root. `reboot` and `sethostname` are root's.

New files get the process's effective user and group, and the requested
permissions minus the **umask** (inherited, 022 by default). A directory
with the setgid bit hands its group to new entries, as on Linux.

Where ownership is stored: the in-memory file system keeps owner, group and
permissions per file; the initrd's cpio headers carry them, so xtask
decides them at build time (everything belongs to root; `eliauth` is 4755,
`/etc/shadow` 0600, `/root` 0700, `/tmp` 1777). FAT has nowhere to store
them, so a FAT volume reports everything as root's with mode 0755 and
refuses changes, as Linux's `vfat` does by default. `/proc/<pid>` belongs
to the process's user (which is how `ps` tools find out who runs what), and
`/proc/<pid>/status` has `Uid:` and `Gid:` lines.

The terminal is a file like any other: `login` gives `/dev/console` to the
user who logs in (mode 0620), and `/dev/tty` is a separate node, open to
everyone, that reaches the same terminal.

### Accounts

Accounts are kept in `/etc/passwd`, `/etc/group` and `/etc/shadow`, in
exactly Linux's formats, so the files (and tools like BusyBox's) are
interchangeable. libeli's `users` module reads and writes them; an update
writes a new file and renames it over the old, so a reader sees the old
file or the new one, never half of each.

**Passwords** are stored as SHA-512 crypt hashes (`$6$salt$hash`), the
scheme glibc uses: the password and a random 16-character salt go through
5000 rounds of SHA-512 in a fixed, deliberately convoluted pattern, so each
guess costs an attacker real work and identical passwords hash
differently. Both SHA-512 and the scheme are implemented in libeli
(`crypt.rs`) from their specifications, and the output was checked to be
identical to OpenSSL's (`openssl passwd -6`) and to accept the published
test vector. Comparison of hashes takes the same time whether they differ
early or late. An empty hash means no password; one starting with `!` or
`*` is locked.

### Logging in

init no longer starts a shell itself: it runs the login screen
(**greeter**, below) on the console, and again whenever the session ends.
Plain **login**, the classic prompt, is still there for serial lines and
for when the greeter can't run. login asks for a name and password (with
echo off, through the same termios flags as on Linux), pauses two seconds
after a failure whether the name or the password was wrong (so names can't
be probed), gives the user the terminal, sets their groups, group id and
user id (in that order, since after the last step the others can't be
changed), changes to their home and runs their shell as a login shell
(`-sh`) with a fresh environment (`HOME`, `USER`, `LOGNAME`, `SHELL`,
`PATH`, `TERM`). The shell's `logout` (or `exit`, or Ctrl+D) ends the
session and login asks again.

`autologin = NAME` in system.conf logs NAME straight in to a shell for
the first session (a kiosk, say), and so does `autologin=NAME` on the
kernel command line, which is how `cargo xtask session` starts at a root
shell. Every later session asks.

### The login screen (`greeter.rs`)

The greeter is a box in the middle of the console, in the style of the
`ly` display manager: three fields, *session*, *login* and *password*.
Up and Down (or Tab) move between them; Left and Right on *session*
choose Text or Desktop; Enter logs in; F1 powers off and F2 reboots. The
session is chosen at login, not at boot, so there's no boot menu or
countdown to wait through, and a broken desktop never locks you out: Text
still gets a shell.

It's drawn with ordinary escape sequences (cursor positioning, colors,
box-drawing characters, which the console font has), redrawn after every
key, so it works on the framebuffer console and a serial line alike and
needs nothing graphical. Function keys reach programs as the Linux
console's sequences (`ESC [ [ A` for F1, `ESC [ 1 7 ~` for F6...), and the
shell's key decoder knows them, so they don't type stray letters.

Checking the password is login's code: unknown names and wrong passwords
get the same answer after the same two-second pause. On success the
greeter restores the terminal and calls the same session start as login,
with the desktop (`/bin/desktop`) instead of the shell if that was the
choice; the desktop runs as the user, who has the `video` and `input`
groups for `/dev/fb0` and `/dev/input`. It remembers who logged in last
and each person's session, in `/data/etc/greeter` (in `/run` on the live
system), and fills them in next time, starting at the password.

Development images (everything xtask builds) come with an account, `test`
with password `1234`, an administrator: xtask adds it to the image's
system.conf and shadow file, so trying a build in QEMU or on the laptop
needs no account setup. The installer drops the live system's accounts
from the configuration it copies; the installed system has only the one
made while installing.

### su, sudo and passwd

These need root's privileges when an ordinary user runs them: `passwd`
writes `/etc/shadow`, and `su` and `sudo` switch users. They live in a
separate program, **eliauth**, installed setuid root, rather than in
eliutils (BusyBox's approach is one setuid binary that drops privileges for
every other applet; a separate program means only these few hundred lines
ever run with privileges someone else gave them). Its rules:

- `su [-] [USER]` asks for the *target's* password (root needn't give one).
- `sudo` lets members of the `wheel` group run commands as root after
  giving *their own* password, remembered for five minutes by a
  root-owned timestamp file in `/run/sudo`. Commands are found through a
  fixed search path, never the caller's `PATH`.
- `passwd` changes your own password after you give the current one; root
  can change anyone's, remove one (`-d`) or lock an account (`-l`).

`adduser` (root only) picks the first free id from 1000 up, creates a
group of the same name and number ("user private groups", as Debian and
Fedora do), copies `/etc/skel` into the new home directory (mode 0750),
and with `--admin` adds the user to `wheel`.

## 15. System calls and Linux compatibility

`syscall/`.

### The ABI

Linux x86_64's convention: call number in rax; arguments in rdi, rsi, rdx,
r10, r8, r9; result in rax, with errors as negated error numbers. eliOS uses
the same numbers for the same calls, and the same structure layouts (`stat`,
`termios`, `sigaction`, `dirent64`, `pollfd`, `utsname`, `sysinfo`, ...). About
100 calls are implemented. eliOS's own calls (process list, kernel log,
tracing) are numbered from 0x1000, far from Linux's range.

`uname` reports the system as "eliOS", but with release "6.8.0-eliOS":
programs that check for a minimum kernel version compare the release
number, and eliOS supports what they'd check for.

### Details that matter

These were all found by running real programs:

- **Integer arguments are 32-bit.** File descriptors, directory fds, pids and
  flags are C `int`s; the upper half of the register can be garbage. glibc
  passes `AT_FDCWD` (-100) as `0xFFFFFF9C`, which a 64-bit comparison
  missed. eliOS now truncates these as Linux does.
- **`isatty` uses `TCGETS2`**, the newer terminal query with baud rates, in
  current glibc. Without it glibc decides stdout isn't a terminal and fully
  buffers it, so output appears out of order.
- **`fstat` is often `newfstatat(fd, "", AT_EMPTY_PATH)`.**
- **Thread-local storage** needs `arch_prctl(ARCH_SET_FS)`, and the FS base
  must be saved and restored per thread and inherited by `fork`.
- BusyBox needed `prctl(PR_SET_NAME)`, `statfs` and `sendfile`.

### Running Firefox

Alpine's Firefox runs unmodified. What it needed, in the order it asked:

- `exec` in the shell, and `"$@"`: `/usr/bin/firefox` is a two-line
  script, `exec /usr/lib/firefox/firefox "$@"`.
- Package **triggers** (section 22): GTK's image loaders cache.
- `fallocate` (shared memory in `/dev/shm`), `mremap`, `statx`,
  `close_range`, `membarrier`, `preadv2`/`pwritev2`, `capget`, the
  scheduler queries, `readahead` and `fadvise64` (accepted as hints).
- `prctl`: `PR_SET_NO_NEW_PRIVS`, `PR_SET_PDEATHSIG` (a child gets a
  signal when its parent dies; GLib's image loaders use it), and
  `PR_CAPBSET_READ`. `PR_SET_NAME` names the calling thread, not the
  process, unless it's the main thread.
- `arch_prctl(ARCH_SET_GS)`: WebAssembly code compiled to native code
  addresses its memory through the GS segment. The GS base is saved per
  thread, like FS, in `IA32_KERNEL_GS_BASE` while in user mode.
- **Namespaces are refused** with `EPERM` (`unshare`, and `clone` with
  `CLONE_NEW*` flags). `bwrap` and Firefox's sandbox then fall back to
  running unsandboxed, as they do on Linux systems that disable
  unprivileged namespaces.
- `/proc/sys/kernel/overflowuid` and the other `/proc/sys` files.
- A non-blocking `pipe2` (see Pipes).

Firefox starts about a dozen processes through its fork server: the
parent, a socket process, extensions, a graphics decoder (RDD), and
content processes. It draws with software WebRender into `wl_shm` buffers.
TLS is Firefox's own (NSS), so HTTPS needs nothing from the kernel beyond
TCP.

### Tracing

`strace <program>` (in eliutils) turns on a per-process tracing flag (an
eliOS system call) and runs the program; the kernel prints every system call
with its first three arguments and result (with error names). The flag is
inherited by children and kept across `exec`. Unimplemented calls are also
logged by name at debug level. These two tools are how every compatibility
gap above was found.

A program that dies of a fault the kernel reports where:
`glxtest (pid 35): general protection fault at 0x6ffffbf5711c
(/usr/lib/libX11-xcb.so.1+0x111c), stack ..., error code 0x0`, an offset
`objdump` or `addr2line` can look up. For programs that hang, see
`/proc/PID/threads` and `/proc/PID/stacks` (section 16).

## 16. Files

`fs/`.

### The VFS

Everything that can be opened is an `Inode`: a trait with methods for
reading, writing, directory lookups, creating, linking, renaming, ioctl,
poll readiness and more. Every method has a default that refuses the
operation, so each kind of inode implements only what makes sense for it.
Implementations: the in-memory file system, FAT files and directories,
devices, pipes, the terminal, and `/proc`'s generated files. Every inode
reports an owner, a group and permission bits; who may do what with them
is section 14.

**Mounts** (`mount.rs`): a table lists which file system covers which
directory, identifying the directory by (file system, inode number) so it's
recognized however the path was spelled. Path lookup checks each directory
it reaches against the table and continues in the mounted root. The root is
itself a mount: an in-memory tree for the live system, an ext2 partition
for an installed one. A mounted disk file system provides a `Volume` (sync,
unmount, read-only switch); unmounting fails with `EBUSY` while any of its
files or directories are in use (each volume knows its live nodes). Before
power-off or reboot, every volume is written back and marked cleanly
unmounted. `..` is resolved lexically (by normalizing the path string),
which avoids needing parent pointers, and makes `..` at a mount point's
root step back into the parent file system for free.

**Path resolution** walks one component at a time and follows symbolic
links (relative to the link's directory, up to 8 deep, then `ELOOP`).

**Open files and descriptors** follow POSIX: an `OpenFile` is an inode plus
a position and flags; descriptors point to open files; `dup` and `fork`
share the open file (so the position is shared); `exec` closes descriptors
marked close-on-exec. The position lock is never held across a read, since
reads can block (a pipe, the terminal) and a spinlock must never be held
while sleeping.

**Renaming** is an inode method with a default (link under the new name,
unlink the old) and a FAT override. Each inode reports which file system it
belongs to, and renames across file systems fail with `EXDEV`, as on Linux;
`mv` then falls back to copy-and-delete.

### The in-memory file system and the initrd

The root file system is in memory (`ramfs.rs`): files are byte vectors and
directories are `BTreeMap`s. It's filled at boot from the initrd, a **cpio
archive** in the "newc" format, the same format Linux uses for its
initramfs. newc is simple (ASCII hex headers, 4-byte alignment) and standard
(any `cpio` tool can inspect it). xtask writes it with its own code.

eliutils is one binary; `/bin` holds a symbolic link per command, which is
why symlinks were implemented early.

### /dev and /proc

`/dev` has `null`, `zero`, `random`, `urandom`, `console` and `tty`, and a
**block device file** per disk and partition (`sda`, `sda1`, `nvme0n1p2`),
named as on Linux. Reading and writing one reads and writes sectors
directly (partial sectors are read, changed and written back), owned by
root and the `disk` group, mode 0660. They answer Linux's block ioctls:
`BLKGETSIZE64` (size), `BLKSSZGET` (sector size) and `BLKRRPART`, which
rereads the partition table after a tool changed it and replaces the
partition files (refused while one is mounted).

`/sys/block` describes each disk as Linux does: `size`, `removable`,
`device/model`, and per partition `partition`, `start`, `size` and its GPT
`uuid`. `lsblk` and the installer read it.

`/sys/firmware/efi/efivars` exists on UEFI systems (that's how programs
tell) and holds the firmware's variables in Linux's **efivarfs** format: a
file per variable, named `Name-<vendor GUID>`, containing the variable's
32-bit attributes followed by its value. Writing a file (in one write, same
layout) sets the variable; deleting it deletes the variable.

`/proc` follows Linux's layout because programs read it: `meminfo`,
`uptime`, `cpuinfo`, `stat`, `mounts`, `partitions`, `filesystems`,
`version`, `loadavg`, and per
process `exe`, `cwd`, `cmdline`, `status`, `stat`, `syscall`, `maps` and
`fd/`. `maps` names what's mapped (the file, `[stack]`, `[heap]`) with its
offset, as Linux does. `/proc/sys` has the settings programs read:
`kernel/hostname`, `osrelease`, `overflowuid`, `pid_max`,
`random/boot_id` and `uuid`, `vm/overcommit_memory`, and
`user/max_user_namespaces` = 0, which tells sandboxes not to try. Files are
generated when first read and the result is kept for that open file. A bug
showed why: `cat /proc/self/maps` reads in chunks, and `cat`'s own
allocations changed its memory map between reads, so regenerated content
shifted under the read position and lines appeared repeated. Like Linux,
`/proc` files report size 0.

`comm` is the process's name.

Three files are eliOS's own, for finding where a program is stuck or slow.
`profile` is a sampling profiler: write `1` to start, and every timer tick
(a thousand a second per CPU) that interrupts the process counts where it
was, the user instruction pointer, or the system call it was in; reading
lists the most frequent places as `library+offset` and `[kernel: call]`,
then the kernel's own addresses (for `addr2line` on `elikernel`). Time with
interrupts off shows up at the instruction after they're turned back on, so
kernel samples bunch at the ends of locked sections. It found most of the
performance problems in this section and the next. The other two:
`threads` lists each thread with its name and the system call it's in, and
`stacks` adds where each call came from and the return addresses found on
the thread's user stack, as `library+offset`. Programs rarely keep frame
pointers, so `stacks` scans the stack for words that point into executable
areas; that has false entries, but the real callers are there. With a
distribution's debug symbols (`firefox-dbg` from Alpine), `addr2line -f -C
-e libxul.so.debug OFFSET` on the host names them.

### Pipes

A 64 KiB buffer with reader and writer counts. Readers block while empty
(end of file once all writers are gone); writers block while full and get
`EPIPE` plus SIGPIPE when no readers remain, which is how `yes | head` ends.
Each end is one open file, so `dup` and `fork` share it rather than
counting twice. `pipe2` takes `O_NONBLOCK` and `O_CLOEXEC`: ignoring
`O_NONBLOCK` once froze Firefox, whose child-process watcher drains its
signal pipe until `EAGAIN`; the read blocked instead, on the thread that
carries all of Firefox's inter-process messages.

### poll and select

Each inode reports its readiness (for pipes it depends on which end, so the
open file's flags are passed in). Pipes, sockets, terminals, event files and
input devices also have a **poll source** (`fs::PollSource`): a wait queue
woken when their state may have changed, and a count of those changes.
`poll` waits on the sources of the files it watches, all at once
(`WaitQueue::wait_any`), then rechecks them. Files without one (regular
files are always ready) use a global queue that `fs::notify_poll` wakes.

It used to be only the global queue: any change anywhere woke every
waiting thread in the system to recheck its files. Firefox has dozens of
threads waiting at any time, and a profile showed idle processes spending
their time in `poll` and `epoll_wait`; with per-file sources they don't
run at all. Network sockets share one source, since the stack doesn't
track which socket a packet changed. `select` and `pselect6` are
translated into the same mechanism.

### Event files and epoll (`fs/events.rs`, `fs/epoll.rs`)

Event loops (GLib's, libwayland's, Firefox's) wait for more than sockets
and pipes. An **eventfd** is a 64-bit counter: writes add, a read takes
the total (or one, in semaphore mode) and waits while it's zero; threads
and processes use it to wake each other. A **timerfd** is a timer read
like a file: a read returns how many periods have passed since the last
one. Its expiry is computed from the clock when asked, so it needs no
timer of its own; instead files can tell `poll` when they'll become ready
by themselves (`Inode::poll_deadline`), and every wait includes that in
its deadline.

**epoll** keeps a set of watched files and waits on their poll sources,
like `poll`; its own source counts changes to the set, so a wait started
before another thread's `epoll_ctl` starts over. Entries hold the open
file weakly, so
closing a file's last descriptor drops it from every set, as on Linux. An
epoll set is itself readable when something in it is ready, so sets can
be nested. One-shot entries go quiet after one report until re-armed.
*Edge-triggered* entries are reported once per change: an entry remembers
its file's change count when reported and isn't reported again until the
count moves. Programs following the edge-triggered contract (read until
`EAGAIN`) see every event, and an always-writable socket doesn't make
`epoll_wait` spin.

## 17. The terminal

`tty.rs`.

`/dev/console` behaves like a Linux terminal, because programs depend on
its behavior:

- **Canonical mode** (the default): the kernel edits a line at a time. It
  echoes typed characters, handles backspace (a whole UTF-8 character at a
  time), Ctrl+U, and Ctrl+D (end of file on an empty line), and programs
  read whole lines. Escape sequences (arrow keys) are dropped in this mode.
- **Raw mode**, selected through the standard termios ioctls: bytes go
  straight to the program. The shell's line editor, `edit` and `vi` use it.
- **Ctrl+C** sends SIGINT to the foreground process group (when the termios
  `ISIG` flag is on).
- The **window size** ioctl reports the console's columns and rows.

Input comes from two places that both behave like a real terminal sending
bytes: the keyboard (decoded to the bytes a terminal would send, such as
`ESC [ A` for the up arrow) and the serial port (so a terminal on the host,
or a script, can drive eliOS). Output goes to both the screen and serial.

### Pseudo-terminals

A terminal emulator in the desktop (or `ssh`, or `script`) needs a terminal
that a *program* plays the keyboard and screen of. Opening `/dev/ptmx`
makes a pair: the program holds the **master** side, and `/dev/pts/N` is
the **slave**, an ordinary terminal for the shell to run on. It's the same
line discipline as the console's (it's the same code); only where output
goes differs. Writing to the master is typing at the terminal; what
programs write to the slave collects for the master to read, with `\n`
turned into `\r\n` as terminals expect (`ONLCR`). The window size belongs
to the terminal, set by the emulator (`TIOCSWINSZ` on the master), and a
change sends `SIGWINCH` to the foreground group. When the last slave
descriptor closes (the shell exited), reading the master fails with `EIO`,
which is how emulators notice; when the master closes, the terminal hangs
up: its programs get `SIGHUP` and end of file.

Each process has a **controlling terminal**, which `/dev/tty` opens:
inherited by children, dropped by `setsid`, and taken by opening a
terminal (or `TIOCSCTTY`) without one. Programs started in a terminal
emulator then reach their own terminal, not the console.

## 18. Devices and drivers

### The framebuffer device (`fs/fbdev.rs`)

`/dev/fb0` is Linux's framebuffer device over the screen the bootloader
found: its size and pixel layout come from the `FBIOGET_VSCREENINFO` and
`FBIOGET_FSCREENINFO` ioctls, and `mmap` maps the screen's memory itself
(as device memory, write-combined). While any program has it open for
writing, the text console stops drawing; it keeps its text and redraws it
when the last such program closes the device, so a desktop that crashes
hands the screen back. Members of the `video` group may open it (every
user of the computer is one, as on desktop Linux systems; `input` and
`audio` likewise).

### PS/2 keyboard (`keyboard.rs`)

The i8042 controller is configured to interrupt on key presses and to
translate scancodes to set 1, so the decoder needs only one table whatever
the keyboard speaks. The interrupt handler only queues raw scancodes (in a
lock-free queue) and wakes the keyboard thread, which decodes them (shift,
caps lock, ctrl, extended keys; US layout) and feeds the terminal. Keeping
the handler tiny keeps interrupts short. At start-up the keyboard is
told to repeat held keys after 250 ms, 30 times a second (Linux's
setting; the power-on default of 500 ms and about 11 a second makes
holding Backspace drag).

Both this and the USB keyboard driver turn keys into `Key` values and hand
them to `keyboard::send_key`, which sends the terminal the bytes a terminal
would (`ESC [ A` for the up arrow, and so on). They also report every
press and release to their input device (below).

### PS/2 mice (`mouse.rs`)

Laptops wire their touchpads and pointing sticks to the keyboard
controller's second ("auxiliary") port, speaking the PS/2 mouse protocol;
on the ThinkPad T440p the touchpad passes the TrackPoint's movements
through, so one device carries both. Bytes from either port arrive at the
same data register; bit 5 of the status register says which port a byte
came from, and each port has its own interrupt (1 and 12), so each handler
takes only its own bytes. Setup is done by polling, with the mouse's
interrupt off: reset (the self-test answers `AA 00`, after up to a second),
defaults, then the IntelliMouse "knock" (sample rates 200, 100, 80), after
which a wheel mouse reports ID 3 and sends 4-byte packets. Packets are
three bytes (buttons and sign bits, X, Y) plus the wheel; bit 3 of the
first byte is always set, which is how the decoder resynchronizes after a
lost byte. Y counts upward in PS/2 and downward in Linux, and the wheel
the other way round, so both are flipped.

Touchpads have richer protocols of their own (absolute positions, several
fingers, gestures); those are for later. In plain mouse mode the touchpad
moves the pointer and taps or clicks are the left button.

### Input devices (`input.rs`)

Graphical programs, and the libraries they use, read input the way Linux
presents it: `/dev/input/eventN` files ("evdev") that read as a stream of
24-byte `struct input_event`s (time, type, code, value), grouped by
`SYN_REPORT` events. Key codes are Linux's (`KEY_A` is 30); conveniently
they started out as scancode set 1, so PS/2 keys translate by identity
apart from the `E0`-prefixed ones, and USB keys through the HID usage
table. Mice report `REL_X`, `REL_Y`, `REL_WHEEL` and the `BTN_` codes.

Each open file needs its own queue (two readers each see every event), so
the VFS gained a hook: `Inode::open_instance` lets a device return a fresh
inode for each `open`, and the device keeps weak references to its
readers. The ioctls libraries use to identify devices are there: name,
ID, which event types and codes it can produce (`EVIOCGBIT`), properties
(`INPUT_PROP_POINTER`), keys held, and the clock for timestamps. A reader
can *grab* a device (`EVIOCGRAB`) to get its events alone; when a
keyboard is grabbed, `send_key` stops typing at the text console, which is
how a graphical session will take the keyboard over. A reader that falls
behind by 1024 events gets `SYN_DROPPED`, as on Linux.

### PCI (`drivers/pci.rs`)

Devices are found by probing every bus, device and function through the
configuration ports 0xCF8/0xCFC. These must be accessed as whole 32-bit
values: on many chipsets the neighboring port 0xCF9 is the *reset control
register*, so byte-wise access could reboot a real machine. (The first
version did exactly that; it was caught before it ran.)

**Interrupts from PCI devices** use MSI or MSI-X ("message signaled
interrupts"): the device signals an interrupt by writing a value to an
address that the local APIC recognizes, and the value is the vector. The
alternative, the legacy interrupt lines, are wired differently on every
board, and only ACPI's AML code (which eliOS doesn't interpret) says how.
MSI is configured in the device's PCI capability list; MSI-X puts the same
information in a table inside one of the device's memory regions, one
entry per interrupt. eliOS uses MSI if the device has it, else MSI-X.

### USB (`drivers/usb/`)

USB keyboards are the norm on PCs, and many recent machines no longer
emulate a PS/2 keyboard for them once an operating system takes over the
USB controller. So eliOS drives the controller itself.

**xHCI** (`xhci.rs`) is the controller interface on every PC from the last
decade; it handles USB 1, 2 and 3 devices. The driver and the controller
share rings of 16-byte entries ("TRBs") in memory: the driver writes
commands on the command ring and transfers on each endpoint's ring, then
rings a doorbell register; the controller writes completions and port
changes on the event ring. Instead of shared indexes, each ring has a
cycle bit that flips every time it wraps, which is how each side tells
new entries from stale ones. Setup is: take the controller over from the
firmware (the "USB legacy support" capability, which also stops the
firmware's PS/2 emulation), reset it, give it the device context array,
the command ring and the event ring, and start it.

**Finding devices.** For each connected port: reset it (USB 2 ports; USB 3
ports enable themselves), ask the controller for a device slot, and
"address" the device with an input context describing it (its speed, its
port, and endpoint 0). Then, through *control transfers* on endpoint 0
(a setup packet, optional data, a status acknowledgment), read the device
descriptor (its first 8 bytes first: they give endpoint 0's real packet
size, which can differ on full-speed devices) and the configuration
descriptor with its interfaces and endpoints. Ports that change later
(plug and unplug) arrive as events and are handled the same way.

**Intel's two controllers.** Intel's 7-, 8- and 9-series chipsets (Ivy
Bridge to Broadwell PCs, like the ThinkPad T440p) have an xHCI controller
*and* older EHCI ones, and every port starts out wired to EHCI so firmware
drivers work. Before resetting the xHCI controller, the driver moves the
ports over: USB 2 through the `XUSB2PR` configuration register and USB 3
through `USB3_PSSEN`, each to what the mask register beside it allows.
Without this, the built-in Bluetooth and camera, and USB 2 devices in
most sockets (the USB stick eliOS started from, say), would stay on a
controller eliOS doesn't drive.

**Keyboards** (`hid.rs`) are driven through the HID *boot protocol*, a
fixed 8-byte report every USB keyboard supports so that firmware can use
them without parsing HID report descriptors: a byte of modifier keys and
up to six keys held down. eliOS selects the boot protocol, configures the
keyboard's interrupt endpoint, and keeps one transfer queued on it; each
completed transfer is a report. A key press is a key that's in a report
but wasn't in the previous one. USB keyboards don't repeat held keys (PS/2
ones do it themselves), so the driver does: after 250 ms, every 33 ms (as PS/2 is set up). It
also sets the Caps Lock LED.

**Mice** use the boot protocol too: a byte of buttons and signed X, Y and
(on most) wheel movement. A device can have several HID interfaces (a
wireless receiver is often a keyboard and a mouse), so each device keeps a
list of HID endpoints, each with its own `/dev/input` device. Mice whose
interface isn't boot-capable need HID report descriptors parsed, which is
for later.

**Bluetooth** controllers are USB devices too (class `E0`), including the
one on Intel's 7260 Wi-Fi card. So far eliOS only identifies them: it
sends HCI commands (a class request on endpoint 0) and reads the
answering events from the interrupt endpoint, logging the controller's
address and version, and for Intel's, which firmware patch it needs. The
Bluetooth stack proper (patching, pairing, L2CAP, HID and audio profiles)
is to come.

**Storage** (`storage.rs`): USB sticks, card readers and external disks
nearly all speak the *Bulk-Only Transport*: each command is a 31-byte
Command Block Wrapper sent to the device's bulk OUT endpoint, carrying a
SCSI command; then the data on the bulk IN or OUT endpoint; then a 13-byte
Command Status Wrapper. The SCSI commands are the classic ones: INQUIRY
(what are you), TEST UNIT READY (retried for a few seconds, with REQUEST
SENSE, while a disk spins up or a card reader notices its card), READ
CAPACITY (the 16-byte form beyond 2 TiB), READ and WRITE (10- or 16-byte
forms), in pieces of up to 64 KiB through one bounce buffer per device.
Each logical unit with a disk becomes an `sd` disk like a SATA one;
devices with blocks bigger than 512 bytes have them split and merged.
Failures follow the specification's recovery: a stalled endpoint is reset
on the controller (Reset Endpoint, then Set TR Dequeue Pointer to skip what
was queued) and on the device (CLEAR_FEATURE(ENDPOINT_HALT)); a confused
device gets a Bulk-Only reset. A stall on endpoint 0 (a device refusing a
request, which is normal for GET MAX LUN on single-unit devices) is reset
the same way, or every later request would fail.

This is what makes a live USB stick work on a real PC: the firmware reads
the stick to start eliboot, but once eliOS takes over the USB controller,
only eliOS's own driver can read it, and the installer gets its files from
it.

**Hubs.** A device behind a hub is addressed through a *route string* (the
hub port numbers on the way, four bits per tier, up to five tiers) plus the
root port, in its slot context; a low- or full-speed device behind a
high-speed hub also names the hub and port whose *transaction translator*
converts its traffic. A hub's own slot context says it's a hub, how many
ports it has, and its translator's think time. The hub driver reads the hub
descriptor (USB 3 hubs have their own type, and are told their depth),
powers every port, and keeps a transfer queued on the hub's interrupt
endpoint, which completes with a bitmap of ports that changed. A changed
port has its change bits acknowledged; a new device is reset through the
hub (`SET_FEATURE(PORT_RESET)`), its speed read from the port status, and
enumerated like any other; a disconnected one is removed along with
everything behind it, if it was a hub too. Tested in QEMU with a USB 1.1
hub; transaction translators and USB 3 hubs follow the specifications but
QEMU can't emulate them.

**Threads and locking.** Each controller has a kernel thread that handles
its events, and the controller sits behind a sleeping lock, because disks
on it are read and written from whatever thread needs them. A disk request
takes the lock, queues its transfers, and waits for their completion
events; events for other devices that arrive meanwhile are kept, and the
controller's thread is woken to handle them. Waiting is on the
controller's interrupt, not a timer: a USB disk does thousands of small
transfers (mounting a FAT disk reads its whole FAT), and sleeping a timer
tick per transfer made mounting the boot stick take three seconds instead
of a tenth of one. Disks found while the lock is held are registered after
it's released, since registering reads their partition tables through the
same controller. The thread otherwise sleeps until the interrupt (MSI or
MSI-X), a key repeat, or a 100 ms check; without interrupts it polls every
10 ms. Boot waits (up to two seconds) for the devices present at startup,
so the keyboard works by the time the shell starts and a USB boot disk can
be mounted.

### AHCI (`drivers/ahci.rs`)

AHCI is the standard SATA controller interface, on real PCs and in QEMU's
q35 machine. Setup: take ownership from the firmware (BIOS/OS handoff), set
the AHCI enable bit, and for each port with a SATA disk, stop it, give it a
command list and a receive area, and start it. Commands are ATA commands
(IDENTIFY, READ DMA EXT, WRITE DMA EXT) in a command table with one memory
region. Transfers go through a 64 KiB physically contiguous bounce buffer
and completion is polled. Interrupt-driven completion would let other
threads run during I/O; polling is simpler and fast enough here.

### NVMe (`drivers/nvme.rs`)

NVMe is the interface to PCI Express SSDs, standard in modern laptops. The
host writes 64-byte commands into submission queues in memory and rings
doorbell registers; the controller writes 16-byte completions into
completion queues, flipping a phase bit each time it wraps so new entries
can be told from stale ones. Setup: reset, create the admin queue pair,
enable, identify the controller and namespace 1, create one I/O queue pair.
Transfers longer than two pages describe the buffer with a "PRP list" page.
Only 512-byte sectors are supported (most drives default to them).

### Partitions and the block layer (`drivers/block.rs`)

Disks implement a small `Disk` trait (read and write whole sectors, plus a
name and model) and are registered as they're found, which creates their
device files. They're named as on Linux: SATA (and USB) disks `sda`, `sdb`,
...; NVMe `nvme0n1`; partitions add a number (`sda1`), with a `p` when the
disk's name ends in a digit (`nvme0n1p1`). Device numbers (major 8 for
`sd`, 259 for NVMe) match Linux's too, since `ls -l` and /proc/partitions
show them.

Partition tables are read from GPT, falling back to MBR. Each partition
keeps its GPT unique GUID (how the boot partition and an installed root are
found) and its type: EFI System, eliOS root (eliOS's own type GUID,
`E1105A00-7E1D-4F0C-8C5B-30E1105A0002`, so an eliOS root is never mistaken
for a Linux one), or other.

### FAT32 (`fat/`)

FAT32 because UEFI requires it for the boot partition, and because every
other system reads it, which makes it the right format for moving files in
and out. The implementation is a separate `no_std` crate so the kernel
(reading and writing disks) and xtask (building images) share exactly the
same code.

The layout: a boot sector describing the geometry; the FAT, one 32-bit entry
per cluster giving the next cluster of the file ("cluster chains"); and the
clusters. Directories are files of 32-byte entries. Long names are stored
in extra entries preceding the classic 8.3 entry, with a checksum of the 8.3
name tying them together; eliOS generates `NAME~1.EXT` short names when a
name doesn't fit 8.3.

Implementation choices:

- Reads and writes find runs of physically consecutive clusters and
  transfer each run in one device request.
- One FAT sector is cached; the free cluster count is computed once and
  then kept up to date.
- Newly allocated clusters are zeroed, so gaps from writes past the end of a
  file read as zeros.
- Formatting chooses the cluster size so the volume has at least 65,525
  clusters, because that number *defines* FAT32: with fewer, every other
  implementation (including UEFI firmware) would read the volume as FAT16.
  That's why eliOS's 128 MiB boot partition uses 512-byte clusters.

The images are validated against independent implementations: Linux's
`fsck.fat` finds no errors, and UEFI firmware boots from them.

**In the VFS** (`fs/fatfs.rs`), each FAT file or directory is a node cached
by its entry's position on disk, so the same file is always the same node
however it's reached. Deleting a file marks any open node for it as deleted,
because its clusters may be reused. FAT has no permissions, symlinks or hard
links.

### ext2 (`ext2/`)

Installed systems keep their files on **ext2**, because a root file system
needs what FAT can't store: owners, permissions, symbolic and hard links.
ext2 was chosen over designing a new format because it's simple enough to
implement completely (no journal, no extents), it's Linux's classic file
system (so an eliOS disk can be read, and checked, on Linux), and Linux's
own tools make an exacting test oracle. Like FAT, it's a separate `no_std`
crate shared by the kernel and userland (`mkfs` and the installer).

The layout: the volume is split into **block groups**, each with a bitmap
of free blocks, a bitmap of free inodes and an inode table; some groups
keep backups of the **superblock** (the volume's geometry and counters)
and of the group descriptor table (where each group's bitmaps and table
are). An **inode** holds a file's type, permissions, owner, size, times,
link count and where its data is: 12 direct block numbers, then single,
double and triple **indirect** blocks (blocks of block numbers); 0 is a
hole that reads as zeros. A **directory** is a file of variable-length
records (inode, record length, name length, type, name); deleting merges a
record into the previous one, adding splits one with room to spare.

Implementation choices:

- **Supported features:** revision 1 with `filetype`, `sparse_super` and
  `large_file`, 1-4 KiB blocks, 128- or 256-byte inodes. Volumes from
  Linux's `mke2fs -t ext2` also have `resize_inode`, `dir_index` and
  `ext_attr`, which are safe to leave alone: hashed directories are read as
  plain ones and lose their index flag when changed (exactly what Linux's
  own ext2 driver does). Journals needing replay, extents and checksums
  are refused or mounted read-only.
- **Caching and consistency:** metadata blocks (bitmaps, inode tables,
  directories, indirect blocks) go through a cache of up to 2048 blocks;
  every operation writes its changes back before returning, so the disk is
  never more than one operation behind (no write-back cache to lose in a
  crash). File data bypasses the cache and moves in runs of up to 64
  contiguous blocks per device request.
- **Allocation:** a file's blocks are taken starting just after its
  previous block (so files stay contiguous); new directories go to the
  group with the most free inodes (spreading the tree out), new files to
  their directory's group.
- **Clean flag:** mounting writable marks the volume "not clean";
  unmounting (and shutting down) marks it clean, so a crash is noticed the
  way Linux notices it.
- **Formatting** follows mke2fs's choices: 4 KiB blocks from 512 MiB up,
  256-byte inodes, one inode per 16 KiB (64 KiB on big disks), a 16 KiB
  `lost+found`, inode tables zeroed.

It's tested on the host (`cargo test -p ext2`): every volume the tests
produce, after creating thousands of entries, indirect blocks, truncation,
hard links, renames across directories, symbolic links and filling the disk,
is checked by Linux's `e2fsck -fn`, and a test makes sure that check really
catches damage. Volumes made by Linux's mke2fs (with a hashed directory
index built by `e2fsck -D`) are mounted, changed, and checked again.

**In the VFS** (`fs/ext2fs.rs`), each inode is one node with a cached copy
of the inode, so `stat` and path lookups don't wait for the volume's lock;
anything that changes an inode refreshes its node. Unix lets a file be
deleted while open: its last name goes, but its data stays until the last
user closes it. The node's destructor notices an inode with no names and
queues it, and the next operation on the volume frees it (the destructor
can run where the volume can't be locked).

### Sound (`drivers/hda`, `sound/`)

Video meetings need a speaker and a microphone, and every PC since 2004
has the same sound hardware interface: Intel's **High Definition Audio**
(the T440p's chipset has it; QEMU emulates it as `ich9-intel-hda`). It
has two halves. The *controller* is a PCI device that moves samples
between memory and the HD Audio link by DMA. The *codecs* on that link
(a Realtek ALC292 in the T440p) turn them into sound and back.

**The controller** (`drivers/hda/mod.rs`) takes commands for the codecs
through a ring in memory (the CORB) and puts their answers in another
(the RIRB). One detail cost a debugging session: the controller counts
responses up to `RINTCNT` and stops taking commands when it gets there,
and the count only resets when the "response interrupt" flag is cleared.
That flag is only raised if it's enabled in `RIRBCTL`, so a driver that
leaves it off gets one answer and then silence. Linux enables it, and so
does eliOS now (the interrupt itself stays masked). Sound flows in
*streams*: a stream descriptor loops over a list of buffers, raising an
interrupt at the end of each (one per ALSA *period*), and tags the data
with a stream number the codec's converters listen for. eliOS uses one
output and one input stream, both 16-bit stereo at 44.1 or 48 kHz.

**The codec** (`drivers/hda/codec.rs`) is a graph of numbered *widgets*:
converters (DACs and ADCs), mixers and selectors, and *pin complexes*
(jacks, the built-in speaker and microphone). The firmware describes each
pin in its "configuration default": a speaker, a headphone jack, a
microphone, and whether anything is wired to it at all. Like Linux's
generic codec driver, eliOS searches the graph for a route from each
useful output pin back to a DAC and from a microphone to an ADC, then
powers, unmutes and selects along it. So basic sound needs no per-laptop
tables. Speakers and headphones are both driven, and a thread watches the
headphone jack's presence detection to mute the speaker while headphones
are plugged in. The route search is tested against a simulated ALC292
laid out like the T440p's.

**The interface** (`sound/`) is Linux's ALSA, so `libasound`, and with it
Firefox, `aplay` and `arecord`, works unchanged. Each card has
`/dev/snd/controlC0` (what the card is, its mixer controls: playback and
capture volume and switches) and `pcmC0D0p` and `pcmC0D0c` (playback and
capture), with Linux's ioctls and structures, and `/proc/asound/cards`.
A program narrows down the parameters it wants (`HW_REFINE`), sets them,
prepares the stream, then writes or reads frames. The buffer is a ring
the device loops over, with two counters running around it: the
program's position and the hardware's. If the device catches up (nothing
left to play, no room to record), that's an *xrun*: the stream stops
with `EPIPE` and the program prepares it again. `libasound` would like
to map the status records into memory. When that fails it falls back to
asking with `SYNC_PTR`, which is what eliOS offers. A device that stops
moving for 10 seconds fails reads and writes with `EIO`, as on Linux,
instead of hanging the program. The hardware does one format, so
`/etc/asound.conf` puts ALSA's `plug` layer in front of it, which
converts any rate, format or channel count in the program.

Two Linux details mattered. musl declares `ioctl`'s request as an `int`,
so ALSA's requests (most have the top bit set) arrive sign-extended to
64 bits. Linux truncates them to 32 bits, and eliOS must too. And sound
devices belong to the `audio` group (29), as on Alpine.

In QEMU, `cargo xtask run --audio-wav FILE` records what eliOS plays (the
sound card's output otherwise goes nowhere); with that backend QEMU has no
microphone, so recording only works without it.

## 19. Networking

`net/`, `drivers/e1000.rs`, `syscall/net.rs`.

eliOS has its own TCP/IP stack, written for it, and implements the Linux
socket API on top, so networking programs (BusyBox's `wget`, `ping`,
`netstat`, glibc programs using sockets) run unmodified.

### The network card (`drivers/e1000.rs`)

Intel's e1000 family: QEMU, VirtualBox and VMware all emulate one (QEMU's
q35 machine an 82574L, "e1000e"; the others an 82540EM or 82545EM), and
the design lives on in Intel's later chips. Packets move through two rings
of 16-byte descriptors, one to receive and one to transmit, each pointing
at a 2 KiB buffer. The driver gives the card receive descriptors by moving
the ring's tail register; the card fills them and sets their "done" bit.
To transmit, the driver fills the next descriptor and moves the transmit
tail. The MAC address is in the receive address registers, where the card
loads it from its EEPROM at reset (the EEPROM is read directly if not;
the 8254x and 82574 put the EEPROM read fields in different places, so
both layouts are tried).

A kernel thread waits for the card's interrupt (MSI, where the card has
it; the 82540EM doesn't, so then it polls every 2 ms) and hands each
received frame to the stack.

**The chipset versions** (I217 and I218, in Haswell and Broadwell
chipsets: the ThinkPad T440p has an I217-LM) use the same rings and
registers, but the PHY, the part that talks to the cable, is a separate
chip. It's connected to the MAC by a fast link for packets and a slow one
(SMBus) that Intel's management engine uses while the computer sleeps,
and firmware can leave it on the slow link or powered down, where it
ignores the MAC. Before the reset, the driver checks the PHY answers (by
reading its ID over MDIO, through the `MDIC` register); if not, it tries
forcing SMBus mode, then power-cycles the PHY with the `LANPHYPC` pin.
MAC and PHY are then reset together (unless the management engine forbids
resetting the PHY), the driver waits for the PHY to load its configuration
from flash, sets the bits Intel's documentation requires after a reset,
and restarts autonegotiation. PHY registers above 15 are paged, and every
PHY access takes a semaphore shared with the management engine. The steps
follow Intel's hardware as Linux's e1000e driver uses it. When the NVM
says software configures the PHY after a reset (`FEXTNVM` bit 27), the
driver copies the MAC's "gigabit disable" and "low-power link" settings
into the PHY's OEM bits, which a reset leaves undefined.

QEMU can't emulate these chips, so this part is tested on real hardware
(it works on the T440p: link at 1000 Mbit/s and DHCP within five seconds
of power-on), and the driver says what it sees for the log on the stick:
link changes with speed and duplex, and for two minutes after startup a
line every five seconds with the MAC's and PHY's view of the link, the
packet and error counters and the number of interrupts. The receive
thread also wakes every 100 ms when it relies on interrupts, so a machine
whose interrupts go missing still gets its packets (and a warning).

### Wi-Fi: Intel 7260 (`drivers/intel_wifi/`)

The ThinkPad T440p's Wi-Fi card is an Intel Wireless 7260 (its Bluetooth
half is a USB device; see USB above). Like all current Wi-Fi cards it
runs firmware of its own, which does the radio work and much of 802.11;
that firmware is closed, comes from Intel through the linux-firmware
collection, and is in the initrd at `/lib/firmware` with its licence. The
driver itself is eliOS's.

**The firmware file** (`firmware.rs`) is a header and a list of TLVs:
code sections for each image (each with the address in the card's memory
it loads at), the radio configuration, calibration triggers, and
capability bits.

**The transport** (`transport.rs`) is the card's PCIe interface: memory-
mapped registers, plus registers and memory deeper in the card reached
through address/data pairs while the driver holds the card awake. Bringing
it up: take ownership (the management engine may be using it), reset,
power up the basic functions with the workarounds Intel specifies for this
family, then set up the queues. Received data arrives in 4 KiB buffers
from a ring of 256 descriptors, with the card writing its progress to a
status block; commands go through transmit queue 0, a ring of 256
descriptors each pointing at up to 20 buffers, which the card's scheduler
serves. Firmware is copied in by the DMA engine's service channel, in
chunks of up to 128 KiB, then the card's processor is released and the
firmware says it's alive.

**The init image** (`mod.rs`) starts the scheduler for the command queue,
configures Bluetooth coexistence, reads the card's NVM with `NVM_ACCESS`
commands (its MAC address, and a word of flags per channel saying which
channels it may use where it was sold), and calibrates the radio, keeping
the `CALIBRATION_RESULT`s. If the hardware kill switch is off, the driver
waits for it. Everything is polled and every step is logged, because the
only way to test it is on the real card: the log saved to the USB stick
shows how far it got, the card's registers and the firmware's error log
if it failed. A firmware error restarts the card from scratch, up to
three times.

**The runtime image** (`runtime.rs`) is then loaded the same way. Its
start-up follows Linux's iwlwifi (`mvm`) closely, since the firmware
expects exactly that sequence: the calibration results go back as the
"PHY database" (in Linux's order), then the radio configuration,
Bluetooth coexistence, "dynamic queue allocation", an auxiliary station
with a queue that scan probes go out on, and power saving off. One kernel
thread owns the card from then on; it polls the receive ring, takes
requests written to `/proc/wifi`, and sends the frames the network stack
queues for `wlan0`.

**Commands** (`commands.rs`) build the firmware's structures byte by byte
(sizes checked by a test against Linux's): scan requests, PHY and MAC
contexts, bindings, time quotas, stations, queues, link quality (the rate
list), time events, smart FIFO. **802.11** (`ieee80211.rs`) parses beacons
(SSID, rates, RSN, HT, WMM, ERP) and builds management and data frames.

**Joining a network**: scan for it (the firmware hops channels and sends
probes; beacons and probe responses come back as received frames, each
preceded by a packet with its channel, signal and timestamps); describe
the channel, our station role and the access point to the firmware; take
the channel with a time event; send authentication and association
requests as ordinary frames on a management queue; then tell the firmware
the association id, the rates (802.11n MCS 15 down to 0 when both sides
have two streams, else the network's legacy rates) and, from the first
beacon, when DTIM beacons come.

**WPA2-Personal** (`wpa.rs`, `crypto.rs`) is done in software, with no
code from elsewhere: PBKDF2-HMAC-SHA1 turns the password into the PMK
(SHA-1 comes from the `apk` crate), the 4-way handshake derives the
pairwise keys, checks the access point's MIC and unwraps the group key
(AES key wrap), and every data frame is then AES-CCMP encrypted or
decrypted here, with replay checks per traffic class. Doing it in
software rather than handing keys to the firmware costs some processor
time, but keeps the firmware interface small. The tests check each piece
against the standards' test vectors (FIPS 197, RFC 3394, IEEE 802.11
annex M) and run a whole handshake against a simulated access point.

**WPA2-Enterprise** (the `tls` crate, `runtime.rs`) is what universities
and companies use: instead of a shared password, each person signs in
with their own user name and password, checked by a RADIUS server behind
the access point (802.1X). The access point relays EAP messages between
us (in EAPOL frames) and that server. eliOS speaks PEAPv0 with MS-CHAPv2,
the method nearly every such network offers (Windows' default): a TLS
session with the server, carried inside EAP messages in fragments of
about 1000 bytes, and inside it MS-CHAPv2, where each side proves it
knows the password. Both then derive the same 64-byte key from the TLS
session (the TLS exporter, label "client EAP encryption"); its first 32
bytes are the PMK, and the 4-way handshake proceeds as for WPA2-Personal.
Our RSN element names the 802.1X key management suite (`00-0F-AC:1`)
instead of PSK in both the association request and handshake message 2.
If no EAP request comes within a second of associating, we send an
EAPOL-Start; the whole sign-in may take 30 seconds.

The `tls` crate is eliOS's own TLS 1.2 client, written for this but
usable elsewhere (`eli`, `git`): ECDHE (X25519) or RSA key exchange,
AES-128-GCM, SHA-256, with RSA signatures checked using the `apk`
crate's RSA code; plus MD4 and DES, which MS-CHAPv2 needs. It does no
I/O, so the same code runs in the kernel and in host tests: against
`openssl s_server` (both key exchanges, and a server asking for a client
certificate, which we answer with none), and a whole PEAP sign-in through
a RADIUS client against FreeRADIUS, checking that our PMK equals the key
the server hands the access point (MS-MPPE-Recv-Key). **The server's
certificate chain isn't verified yet**: the log and `/proc/wifi` show its
subject, issuer and SHA-256 fingerprint to compare with what the
network's IT department publishes. Many campuses treat checking as
optional (Purdue's instructions for some systems do), but it means a
fake access point with the same name could learn the MS-CHAPv2 exchange,
which can be cracked offline for a weak password. Crypto binding (a PEAPv2
idea some servers send as a TLV) is ignored.

**Data**: outgoing Ethernet frames become 802.11 data frames (an LLC/SNAP
header carries the Ethernet type), with QoS headers when the network has
WMM; incoming ones are decrypted, A-MSDUs split, and handed to the stack
as Ethernet frames, so DHCP, TCP and the rest don't know it's Wi-Fi. A
simple rate control moves the firmware's starting rate down after
retries and back up after a run of clean first tries; the firmware falls
back down the list by itself for each frame.

**Leaving** (asked to, deauthenticated, or 20 beacons missed) sends a
deauthentication and restarts the runtime firmware, which clears all of
its state in about a second.

**Using it**: `wifi` shows the state and the last scan, `wifi scan`,
`wifi connect NETWORK [PASSWORD]` (it asks for the password if it isn't
given), `wifi connect NETWORK --user NAME` for enterprise networks (it
asks for the password), `wifi disconnect`. Network names may contain
spaces, quoted or not: `wifi connect Purdue Guest` takes the longest run
of words that names a network in the last scan, and anything after it is
the password. They write to `/proc/wifi` (root only; `connect` sends the
name, password and user name on separate lines) and wait for the state
to settle; `ifconfig` then shows `wlan0`'s address. Not yet: scanning
while connected, roaming, WPA3, EAP methods other than PEAP/MS-CHAPv2,
802.11n aggregation (so throughput is well below the card's best; the
access point's Block Ack requests are declined with status 37, as the
standard asks, so it stops asking and sends frames one at a time).

### The stack (`net/mod.rs`)

One kernel thread runs the stack: it processes received frames, delivers
loopback packets, and runs timers (TCP retransmissions, ARP retries,
DHCP). Programs' system calls work on the stack directly. All its state
is under one lock, held briefly. Programs that block (waiting for data,
a connection, a timeout) wait on one wait queue that's woken whenever any
socket changes; they recheck their own socket. That's simpler than a
queue per socket and fine at this scale.

The layers, bottom up:

- **Ethernet**: a 14-byte header (destination, source, type), then the
  packet. Frames are padded to the 60-byte minimum.
- **ARP** (`arp.rs`): to send to a machine on the local network, its
  Ethernet address is needed. ARP broadcasts "who has 10.0.2.2?", caches
  answers for five minutes, and holds packets for an address being looked
  up (up to 16, for four tries: 0.3 s, then a second apart, since the
  first request was often lost on Wi-Fi). Addresses in use are asked about
  again shortly before their entries expire. Requests for our own
  address are answered. The gateway is asked about as soon as DHCP
  finishes, so the first packet out doesn't wait for it (on Wi-Fi that
  wait made the first `ping` look lost; `ping` also counts late replies,
  as Linux's does).
- **IPv4**: headers are checked (version, lengths, checksum) and packets
  for other machines are dropped. Packets go to the local network directly
  or through the gateway, chosen by the netmask. Outgoing packets set
  "don't fragment" and fragments that arrive are dropped: reassembly is
  rarely needed, since TCP sizes segments to fit.
- **ICMP** (`icmp.rs`): echo requests are answered (so eliOS answers
  pings), and "port unreachable" goes back for UDP datagrams nobody is
  listening for, as on Linux.
- **UDP** (`udp.rs`): datagrams go to the socket bound to their port.
- **TCP** (`tcp.rs`): below.
- **Loopback**: packets to 127.0.0.0/8 or the machine's own address are
  handed back to the stack thread instead of to a card, so the whole stack
  (TCP included) works locally, without any network card.

Checksums (IP, ICMP, UDP and TCP all use the same ones' complement sum)
are computed in software.

### DHCP (`net/dhcp.rs`)

The interface configures itself at boot: broadcast DISCOVER, receive an
OFFER, REQUEST it, receive the ACK with the address, netmask, router and
DNS servers. Retries back off from one second to eight. The DNS servers
are written to `/etc/resolv.conf`, where C libraries look. Boot waits up
to three seconds for it, so programs started at boot find the network
ready.

### TCP (`net/tcp.rs`)

TCP turns unreliable packets into reliable, ordered byte streams. Every
byte has a 32-bit sequence number. The sender keeps what it sent until the
receiver acknowledges it, and sends it again if no acknowledgment comes in
time; the receiver delivers bytes in order and advertises how much more it
can take (its window), which the sender respects. Connections open with a
three-way handshake (SYN, SYN-ACK, ACK) and close with a FIN from each
side, through the standard states (SYN-SENT, ESTABLISHED, FIN-WAIT-1,
CLOSE-WAIT, TIME-WAIT, and so on).

The implementation follows RFC 9293's segment processing, with deliberate
simplifications:

- **Out-of-order segments are dropped.** The sender's retransmission fills
  the gap. Buffering them would cut retransmissions on lossy networks, but
  loss is rare on a LAN.
- **Retransmission is "go back N"**: on a timeout, everything
  unacknowledged is sent again. The timeout starts at one second and
  doubles on each retry (up to 30 s, and 8 tries before the connection is
  declared dead). There's no round-trip time measurement or congestion
  control; those matter on congested internet paths, much less locally.
- **Acknowledgments are immediate**, not delayed.
- **No window scaling**, so windows top out at 64 KiB, which limits
  throughput on long, fast links.
- **TIME-WAIT lasts 2 seconds** instead of the standard two minutes, so
  ports are reusable quickly.

A zero window is probed: if the other side's window is closed while data
waits, a single byte goes out when the timer fires, which forces a
window update. When a program reads from a nearly full receive buffer, a
window update goes out so the sender can continue.

Listening sockets create a connection for each SYN (up to the backlog);
once its handshake completes, it joins the listener's accept queue. A
connection whose program closes it becomes an "orphan" that finishes
closing by itself and is then removed.

### Sockets (`net/socket.rs`, `syscall/net.rs`)

A socket is an entry in the stack's socket table plus a file (a
`SocketFile` inode) referring to it, so `read`, `write`, `poll`, `select`
and `close` work on sockets as on any file, and `dup`/`fork` share them as
they should. The socket table holds TCP connections and datagram sockets:
UDP, ICMP "ping" sockets (the unprivileged kind Linux has, where the kernel
fills in the echo identifier) and raw ICMP sockets (which see every ICMP
packet, IP header included, as BusyBox's `ping` expects).

The system calls are Linux's: `socket`, `bind`, `listen`, `accept(4)`,
`connect`, `send(to/msg)`, `recv(from/msg)`, `shutdown`,
`get/setsockopt`, `getsockname`, `getpeername`. Linux's semantics matter in
the details programs rely on: nonblocking connect returns `EINPROGRESS`
and reports its outcome through `SO_ERROR`; `MSG_PEEK`, `MSG_WAITALL`,
`MSG_DONTWAIT` and `MSG_NOSIGNAL` work; `SO_RCVTIMEO`/`SO_SNDTIMEO` time
out with `EAGAIN`; writing to a closed connection raises SIGPIPE; and
`O_NONBLOCK` (now honored by pipes too) makes waits fail with `EAGAIN`.

Interface information comes the way Linux tools ask for it: the `SIOCGIF*`
ioctls on any socket (address, netmask, flags, MAC, MTU...), and
`/proc/net/dev`, `route`, `arp`, `tcp` and `udp` in Linux's formats. That's
all BusyBox's `ifconfig`, `route` and `netstat` need.

### Unix domain sockets (`net/unix.rs`)

Connections between programs on one machine: the Wayland protocol, D-Bus
and Firefox's processes all use them. A socket is named by a path (bound
to a socket file in an in-memory directory, through which `connect` finds
it), by an *abstract* name (a leading NUL byte; a global table, not the
file system), or not at all (`socketpair`). Streams, sequenced packets
and datagrams are supported.

Each socket has a queue of messages received. A connecting socket makes
the listener's end of the connection itself and puts it in the listener's
backlog for `accept`. A sender waits while the receiver's queue holds
256 KiB; a stream's send goes in pieces if it must. A stream reads as one
run of bytes across messages, except that it stops before a message that
carries files, so files arrive with the bytes they were sent with.

**Passing files** (`SCM_RIGHTS` control messages) is the reason Wayland
works: a client puts the descriptor of the shared memory it draws into in
a message, and the compositor receives a descriptor for the same open
file. The open files travel inside the message (holding a reference), and
`recvmsg` installs them in the receiver's table, close-on-exec if
`MSG_CMSG_CLOEXEC` asks. `SO_PEERCRED` says which process and user is on
the other end (taken when the connection was made), and `SO_PASSCRED`
adds the sender's to each message. Closing a socket gives its peer end of
file (and `EPIPE`/`SIGPIPE` to writers); connections waiting in a closed
listener's backlog are closed too.

### Name lookup

`libeli`'s resolver (`user/eli/src/net.rs`) tries a dotted address, then
`/etc/hosts`, then a DNS query (an A record, over UDP, with retries) to the
servers in `/etc/resolv.conf`. glibc programs use glibc's own resolver,
which reads the same files.

### Testing

`nettest` (a glibc program) checks TCP and UDP over loopback: 200 KiB
through an echo server in both directions at once, nonblocking connect,
`poll` and `select`, `MSG_PEEK`, `shutdown`, timeouts, refused
connections. A kernel test pings loopback and QEMU's virtual router. By
hand: downloads from a web server on the host (`wget` with an MD5 check)
and from the internet, and connections from the host into eliOS through a
forwarded port (`cargo xtask run --forward 8080:80`).

## 20. ACPI: tables and power

`acpi.rs`, `power.rs`.

ACPI tables are found from the RSDP the bootloader passes on, through the
XSDT (or the older RSDT), with every checksum verified.

- **MADT**: the processors, I/O APICs, interrupt overrides and NMI wiring.
- **FADT**: the power management ports and, on newer systems, the reset
  register.
- **DSDT**: to power off, ACPI says to write a "sleep type" for state S5 to
  the PM1 control ports. The values come from the `\_S5` object in the
  DSDT, which is AML bytecode. A full AML interpreter is a big project;
  instead, eliOS scans for the byte pattern of `Name(_S5_, Package(){...})`
  and decodes the two values (handling the encodings AML uses for small
  numbers). This is a well-known approach that works on nearly all
  hardware. If the firmware hasn't switched the chipset to ACPI mode, the
  kernel does that first through the SMI command port.

`reboot` uses the FADT's reset register when present, then the keyboard
controller's reset line, then a deliberate triple fault.

## 21. Userland

### libeli (`user/eli`)

The runtime for eliOS's own Rust programs, written against the Linux ABI.

- **Startup:** `_start` (a naked function) passes the initial stack pointer
  to Rust, which reads argc, argv and envp, calls the program's `main` and
  exits with its result.
- **Allocator:** the same size-class design as the kernel heap, on top of
  `mmap`. Programs are single-threaded, so its state needs no locking; the
  process-wide state uses `RefCell`, turning accidental overlapping use into
  a clean panic.
- **A small standard library:** I/O and print macros, files and directories,
  processes (fork, exec, wait, kill, process groups, signal dispositions),
  terminals (raw mode, window size, foreground group, password prompts),
  time, system information, users and groups (`users.rs`: ids and the
  account files), password hashing (`crypt.rs`), and the eliOS-specific
  calls.

### eliutils (`user/utils`)

One binary, linked under each command's name; the name it's run as picks
the command (`applets.rs` lists them, and xtask reads the same file to make
the links). One binary keeps the initrd small: the whole userland is about
300 KB.

**The shell** (`shell.rs`):

- Lines are split into words and operators with quoting respected, but
  expansion (`$VAR`, `$?`, `~`, wildcards) happens when each pipeline
  *runs*, not when the line is read. Expanding everything up front was the
  first design and was wrong: `false; echo $?` printed a stale status, and
  `touch x.txt; ls *.txt` didn't see the new file.
- Each pipeline gets its own process group, which becomes the terminal's
  foreground group while it runs, then the shell takes the terminal back.
  The shell ignores SIGINT; children reset it to default before `exec`.
- Redirections are applied in the child after `fork`, with `dup2`.
- Pipes are created close-on-exec, so only the ends a child explicitly
  moves into place survive into the next program.
- Background jobs (`&`) are reported when they finish, before the next
  prompt.

**The line editor** (`editor.rs`) uses raw mode to provide cursor movement,
history, Tab completion (commands for the first word, paths after) and the
usual Ctrl shortcuts. It redraws the line on each key, which the console
handles cheaply.

**init** (`init.rs`) runs `/etc/rc` if present, then keeps a login session
going on the console: it runs the login screen (`greeter`; `login` for an
automatic login or if the greeter is missing), and when the session ends,
runs it again (with a pause if sessions end immediately, to avoid a busy loop). It
also collects orphaned processes. `login` prints the welcome message.

**Accounts** (`accounts.rs`): `login`, `adduser`, `deluser`, `id`,
`whoami`, `groups`, `chmod` (octal or symbolic modes like `u+x,go-w`),
`chown` and `chgrp`. `su`, `sudo` and `passwd` are a separate setuid
program (`auth.rs`, installed as `/bin/eliauth`); see section 14.

**edit** (`edit.rs`) is a small nano-style editor: the screen is redrawn
from the buffer after each key.

**The commands** are deliberately simple implementations; the ones with a
subtle requirement are noted in the code (`head` streams its input so
`yes | head` terminates; `mv` falls back to copying across file systems).

**Network commands** (`network.rs`): `ifconfig` (through the same ioctls
as Linux's), `ping` (an ICMP ping socket, one request a second, with
round-trip times), `host` (a name lookup), `wget` (HTTP/1.0 downloads,
following redirects; HTTP/1.0 so the server closes the connection when
done and never uses chunked encoding) and `nc` (connects standard input and
output to a TCP connection, made or accepted; it waits on both with
`poll`).

### The installer (`installer.rs`, `gpt.rs`, `storage.rs`)

`installer` puts eliOS on a disk so the PC starts it without the USB stick
or live disk. It asks which disk (the one eliOS is running from isn't
offered), whether to erase it or install alongside what's there, and for
the first account; then it shows what will happen and wants the disk's
name typed before touching anything. Every answer can also be given as an
option (`--disk nvme0n1 --erase --user alice --password ... --yes`), which
is how it's tested.

An installed system is two partitions: an EFI System Partition (FAT32)
holding eliboot, the kernel, the initrd (the system image) and
`cmdline.txt` (which names the data partition by its GUID), and an ext2
*data* partition with eliOS's own type GUID. The system isn't copied
anywhere: it boots from the image, as the live system does (section 22).
The steps:

1. **Partitioning** (`gpt.rs`, eliOS's own GPT code: headers, entries,
   CRC32s, the backup copy at the end of the disk). *Erase*: the first and
   last MiB are cleared (old tables and file system signatures), and a new
   table gets a 128 or 256 MiB EFI partition and a data partition filling
   the rest, 1 MiB aligned. *Alongside*: the disk must already have a GPT
   and an EFI partition (any PC with a UEFI-installed OS does); the data
   partition goes in the largest free stretch, at least 512 MiB, and the
   existing MBR, partitions and their numbers are left alone. The primary
   header is written last, so until then the old table stands. Then
   `BLKRRPART` makes the kernel see the new partitions.
2. **Formatting** with the same `fat` and `ext2` crates the kernel uses
   (the EFI partition only when erasing).
3. **The account and settings**, written straight into the new ext2
   volume through the `ext2` crate (no mounting needed): `system.conf`
   (the live system's, with the computer's name and the account added),
   `shadow` with a SHA-512 crypt password (root's is locked: administration
   goes through sudo, as on Ubuntu), the account's home copied from
   `/etc/skel`, and generation 1.
4. **Packages added to the live system** are copied into the new store, so
   trying something live and then installing keeps it.
5. **The boot loader**: eliboot, the kernel and initrd go in
   `\EFI\eliOS\` with `cmdline.txt` and the boot menu's list; when
   erasing, eliboot also goes in the fallback path `\EFI\BOOT\BOOTX64.EFI`,
   so the disk starts even if the firmware never got a boot entry.
6. **A boot entry**: an `EFI_LOAD_OPTION` named "eliOS" whose device path
   is the EFI partition (number, start, size and GUID) and the file
   `\EFI\eliOS\eliboot.efi`, written through efivarfs and put first in
   `BootOrder`. Entries an earlier installation left, pointing at
   partitions that no longer exist, are deleted, and an identical entry is
   reused. Alongside another system this entry is the only way in (the
   other system's fallback loader stays), so failing to write it is an
   error there.

The first version took about three minutes, nearly all of it in the FAT
code: every read rebuilt the file's whole cluster chain, and every
allocated cluster rewrote a FAT sector in both FAT copies and was zeroed
before being overwritten. With the last chain cached, FAT sectors written
back once per operation, and clusters zeroed only when a gap needs it, an
installation takes about a second in QEMU.

It was checked end to end: Linux's `sgdisk -v` finds the partition tables
valid, `e2fsck` and `fsck.vfat` find the file systems clean, and the
installed disk boots on its own (through the fallback path after erasing,
and through the firmware boot entry alongside another system's
partitions), logs in, and keeps files across reboots. (The first version
installed a copy of the system on an ext2 root partition; the kernel still
accepts `root=PARTUUID=...` for that layout.)

### The desktop (`user/desktop`)

`desktop` is eliOS's graphical desktop: a Wayland compositor, written from
the protocol specification. Programs that speak Wayland (GTK and Qt
applications, Firefox, terminal emulators like `foot`) connect to its
socket, `$XDG_RUNTIME_DIR/wayland-0`, and draw their windows into shared
memory they hand it; it composes them on the screen and sends them input.
It's one event loop around `epoll`, with a 60 Hz timer for redrawing.

- **The wire protocol** (`user/ui/src/wire.rs`, shared with clients): messages are an object id, a size and
  opcode, and 32-bit-aligned arguments (integers, 24.8 fixed-point
  numbers, strings, arrays, object ids); file descriptors travel beside
  them as `SCM_RIGHTS`, in the order messages use them.
- **Objects** (`server.rs`): each client has a table of objects by id. The
  globals offered are `wl_compositor`, `wl_subcompositor`, `wl_shm`,
  `wl_seat`, `wl_output`, `xdg_wm_base` and a stub
  `wl_data_device_manager`. A client's shared memory pools are mapped
  (and remapped when they grow); buffers are views into them.
- **Surfaces** (`surface.rs`): changes are pending until a commit applies
  them all at once. A committed buffer is *copied* into the surface and
  released straight away, so clients can reuse it and the desktop never
  reads memory a client is changing. Only the damaged rows are copied
  (Wayland lets a compositor assume the rest of a same-sized buffer is
  unchanged), and only that part of the screen is redrawn: Firefox
  scrolling a page redraws a strip, not the window. Subsurfaces (Firefox draws its
  content into one) follow the parent's commits when synchronized.
- **The shell** (`shell.rs`): a bar along the top (buttons for a terminal
  and Firefox, a tab per window, a clock); every window maximized below
  it, so toolkits leave out shadows and rounded corners; dialogs centered;
  popup menus where their positioner says, slid to stay on screen; the
  focused window on top. Frame callbacks are answered after each redraw.
- **Input**: keyboards and mice are read from `/dev/input` (grabbed, so
  the text console stops seeing keys). Clients get a keymap, a US layout
  in XKB's text format (`keymap.rs`), which they compile with
  libxkbcommon, plus key codes and modifier states. Mice move a pointer
  with a little acceleration; the arrow is the desktop's own, unless the
  client under it sets one.
- **Drawing** (`render.rs`, with the toolkit's `Canvas`): into a back
  buffer, blending surfaces with alpha, and only the damaged part is copied to the screen (video memory is
  slow to read, so it's only written). Text is the system's TrueType
  font (below), each character rasterized once and kept, and blended onto
  the background by its coverage.

Shortcuts: Super+Enter a terminal, Super+F Firefox, Super+Tab the next
window, Super+Q closes the front one, Ctrl+Alt+Backspace quits back to the
text console. `desktop PROGRAM` starts a program once it's up.

### The toolkit (`user/ui`)

eliOS's own graphical programs are built on `ui`, a small library that the
desktop shares (the wire format, drawing, fonts). It has no retained widget
tree or layout engine: a program keeps its own state, draws everything
into a pixel buffer when something changed, and hands the window the
rectangle it touched. For a terminal, a file explorer and dialogs that's
simpler than a framework and fast enough: redrawing a window is a few
memory fills and cached glyphs.

- **Windows** (`window.rs`): a Wayland client. `Window::open` connects to
  `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`, binds `wl_compositor`, `wl_shm`,
  `xdg_wm_base` and `wl_seat`, and waits for the first `configure`.
  Drawing goes into a canvas in ordinary memory; `present(rect)` copies
  what changed into a shared-memory buffer (a `memfd`) and commits it.
  Each buffer remembers which parts are stale since it was last used, so
  switching buffers copies only those. At most one frame is in flight:
  the next commit waits for the frame callback, so a busy program can't
  flood the compositor. `wait` polls the connection (and any descriptors
  the program asks for, such as a terminal's pty) and returns plain
  events: resize, close, focus, keys, pointer moves, button presses with
  a click count (double clicks: within 400 ms and 4 px), scroll lines.
  Key repeat happens on the client side, at the rate the compositor gives.
- **Keys** (`keys.rs`): evdev key codes are turned into characters with
  a US layout table of eliOS's own, rather than compiling the XKB
  keymap the compositor sends; modifiers arrive as XKB's bit masks.
- **Drawing** (`draw.rs`): `Canvas` fills (blending translucent colours),
  borders, blits, scrolls a region (a terminal scrolling by a line moves
  pixels instead of redrawing text), and fills paths with the TrueType
  rasterizer, so rounded rectangles and icons are anti-aliased by the
  same code as letters. Everything clips to a rectangle. `Font` caches
  each rasterized glyph by character.
- **Widgets** (`widgets.rs`): functions and small structs that draw and
  handle input but own nothing else: buttons, a one-line text field
  (selection, word moves, Ctrl+A), a list with keyboard navigation and a
  draggable scroll bar, popup menus with separators and disabled items,
  and icons (folder, file, program, home, disk, up) drawn as paths on a
  16-unit grid so they scale. The colours are in `theme`, shared with the
  desktop's bar.

`uidemo` shows every widget in one window (`desktop uidemo`).

### Fonts (`truetype/`)

The desktop and its programs write with a TrueType font, read and
rasterized by eliOS's own code: one font file ships,
`/usr/share/fonts/dejavu/DejaVuSansMono.ttf` (DejaVu Sans Mono, with its
licence beside it). It's monospaced so the terminal can use it as well as
the bar and menus; the `truetype` crate reads any TrueType font, so adding
a proportional one later is just another file.

- **Reading** (`lib.rs`): the tables needed to draw are `head` (units per
  em, `loca`'s format), `maxp` (glyph count), `cmap` (character to glyph:
  format 12 for all of Unicode, else format 4's segments, both binary
  searched), `hhea`/`hmtx` (line height and advances), and `loca`/`glyf`
  (the outlines). A simple glyph's points are stored as flags plus
  delta-encoded coordinates; two control points in a row imply an
  on-curve point between them. Composite glyphs (most accented letters)
  are other glyphs with an offset and maybe a scale or 2x2 matrix, nested
  at most 8 deep. Every read is bounds-checked: a broken font draws
  nothing rather than crashing the desktop.
- **Rasterizing** (`raster.rs`): curves are flattened into lines (more
  pieces the more they bend), and each line adds the exact signed area it
  cuts off in every pixel it crosses to an accumulation buffer; a running
  sum then turns that into each pixel's coverage. That's exact
  anti-aliasing with one pass over the lines and one over the pixels, no
  sampling or edge sorting. The sum runs on across row ends because every
  row's areas cancel out.
- **No hinting**: fonts' bytecode programs that snap outlines to the pixel
  grid are skipped. Grayscale anti-aliasing reads well at the sizes and
  densities used (15 px on a 1080p laptop), and an interpreter would be
  larger than the rest put together.
- `core` has no `floor`, `ceil` or `sqrt` for floats (`std` gets them from
  the C library), so `raster.rs` has small versions of its own.

Tests (`cargo test -p truetype`) check the bundled font's metrics,
character map, closed outlines and composites, and the rasterizer against
shapes whose coverage is known exactly (a half-pixel-shifted square, a
diagonal); `TRUETYPE_SAMPLE=out.pgm` also writes a sample of text to look
at.

### Linux programs

- `user/c/` holds C programs compiled by the host's GCC and glibc: a hello
  world, `ctest` (a self-test of the C library on eliOS), `sigtest`
  (signal handling), `threadtest`, `nettest` and `permtest` (users,
  permissions and setuid programs, checked the way a Linux program sees
  them). `*-dynamic.c` programs are dynamically linked, and
  xtask copies the libraries `ldd` reports into the root file system.
- If the host has a static BusyBox, it's included with links for the
  commands eliutils doesn't have (about 200: `vi`, `awk`, `sed`, `find`,
  `tar`, `gzip`, `top`, ...). Links go in `/usr/bin`, which comes after
  `/bin` in `PATH`, so eliOS's own commands take precedence.

## 22. The system manager: packages, system.conf and generations

`user/utils/src/manager/`, `apk/`, `user/eli/src/http.rs`.

eliOS's answer to "how do I get git or vim" and "how do I keep a system
from slowly breaking" is one tool, `eli`, built on three ideas:

1. **The system itself is an image.** The kernel and the initrd are the
   whole base system; it's unpacked into memory fresh at every boot, live
   or installed. Nothing can drift: what boots is always exactly the image.
2. **One file says what's added to it.** `/etc/system.conf` names the
   computer, its accounts and its packages:

   ```text
   hostname = robinpc
   packages = git vim htop

   user robin
       name = Robin Example
       admin = yes
   ```
3. **Every change is a numbered generation.** `eli apply` makes the system
   match the file and records the result; `eli rollback` returns to any
   earlier one, and eliboot's menu can start one for a single boot.

This is NixOS's central idea (a declared system, with rollback), with the
complexity left out: a plain settings file instead of a programming
language, folders and links instead of a build system, and an existing,
very large package collection instead of a new one.

### Where things live

Everything that should last is under `/data`: the data partition on an
installed system (the kernel mounts it from `data=PARTUUID=...` on the
command line), or a directory in memory on the live system.

| Path | What |
|------|------|
| `/data/etc/system.conf` | the configuration (`/etc/system.conf` links to it) |
| `/data/etc/shadow` | passwords (`/etc/shadow` links to it, so `passwd` lasts) |
| `/data/etc/ids` | user ids, remembered so they never change |
| `/data/home` | homes, mounted (bind) on `/home` |
| `/data/pkg/NAME-VERSION` | the package store: one directory per package, never changed |
| `/data/generations/N` | each generation: `system.conf`, `packages`, `info`; `current` links to one |
| `/data/cache` | the repositories' verified package lists |

### Packages come from Alpine Linux

Writing and maintaining thousands of packages isn't something one project
can do, and eliOS doesn't need to: it runs Linux programs unmodified, so
it can use another distribution's builds. Alpine was chosen because its
packages are small, built against musl (a small C library whose dynamic
loader runs on eliOS as is), cover about 28,000 programs, and come in a
simple, well-documented format served over plain HTTP.

Plain HTTP is safe because nothing downloaded is trusted until checked
(`apk/src/lib.rs` has the details):

- A repository's index is two gzip streams: an RSA signature (made with
  one of Alpine's keys, which eliOS ships in `/etc/eli/keys`, fetched once
  over HTTPS and cross-checked against Alpine's Git repository), then the
  index it signs.
- The index gives each package's SHA-1 of its *control section*.
- The control section gives the SHA-256 of the package's files.

So one signature check plus two hashes per package cover every byte. All
of it is eliOS's own code, in the `apk` crate: DEFLATE decompression, tar
(with the PAX and GNU extensions for long names), SHA-1, SHA-256, base64,
RSA with 4096-bit keys (Montgomery multiplication), PEM keys, the index
format, Alpine's version ordering (`1.2_rc1 < 1.2 < 1.2_p1`, `-r2 > -r1`)
and dependency resolution (by name, version, shared library `so:...` or
command `cmd:...`). Its host tests check it against Python's zlib, tarfile
and hashlib and against OpenSSL, and against real Alpine files when
they're provided.

Package install scripts (`.post-install` and the like) aren't run; few
programs need them, and the ones skipped are recorded in the store.

### Applying a configuration

`eli apply` (and `eli add`, `eli remove`, `eli upgrade`, which change the
file and then apply):

1. resolves the packages, with dependencies, against the cached indexes
   (fetched first if missing), keeping the versions already in use unless
   upgrading;
2. downloads what isn't in the store, verifies it, and unpacks it into
   `/data/pkg/.unpacking-...`, renamed into place only when complete;
3. records a new generation (nothing changed means no new generation);
4. activates it, then points `current` at it (a new link renamed over the
   old one, so the switch is all or nothing), and writes the boot menu's
   list to the EFI partition (eliboot can read FAT, not ext2).

**Activation** makes the running system match a generation:

- `/etc/passwd` and `/etc/group` are written from the image's own
  accounts (ids below 1000) plus the configured users, each with a group
  of its own name; `admin = yes` puts a user in `wheel`. New users get a
  locked password entry until `passwd` sets one, and a home copied from
  `/etc/skel`.
- The hostname is set.
- Every file of every package is linked into place: `/usr/bin/git`
  becomes a symbolic link into `/data/pkg/git-2.54.0-r0/`. Directories
  that don't exist yet are created; ones that do are shared. What was
  created is recorded in `/run/eli/links`, so activating another
  generation first undoes it exactly. A package's file never replaces the
  base system's, except BusyBox's command links (asking for a real `less`
  should get it).

At boot, init runs `eli boot` before anything else: it makes sure `/data`
is set up (a fresh live system gets the image's default configuration and
a first generation), links `/etc/system.conf` and `/etc/shadow` into it,
mounts `/data/home` on `/home`, and activates the current generation (or
the one picked in eliboot's menu, from `generation=N` on the command line).

`adduser` and `deluser` change `system.conf` and apply it on such a system,
so accounts last; `passwd` writes through the `/etc/shadow` link. The shell
answers an unknown command with the package that has it ("vim isn't
installed; it's in the vim package: sudo eli add vim"), from the cached
indexes' `cmd:` entries.

### Package scripts and triggers

Alpine's packages can carry scripts (`.pre-install`, `.post-install`,
...) and a **trigger**: a script plus directories it watches, run after
any package changes them. eli keeps them in the store, in each package's
`.eli-scripts`. Install scripts mostly create users and directories that
eli manages itself, so they aren't run. Triggers rebuild caches from what
other packages put in shared directories (GTK's image loaders, GLib's
settings schemas, fonts, MIME types), so activation runs every trigger
whose directories exist, with BusyBox's `sh` (they're written for it).
What they print goes to `/run/eli/triggers.log`. Since the system is
assembled anew at every activation, they run every time.

`/etc/machine-id` (D-Bus and GLib want one) is made once, in
`/data/etc`, and linked in.

### What it took from the kernel

Alpine's vim needed POSIX timers: `timer_create`, `timer_settime` and
friends (and `setitimer` and `alarm` came with them). Every armed timer is
in one list; a kernel thread sleeps until the earliest is due, sends its
signal (to the process, or to one thread for `SIGEV_THREAD_ID`), and
re-arms repeating ones, counting missed periods as overruns. Signals now
carry their details (`si_code`, and a timer's id and value), and
`rt_sigtimedwait` waits for chosen signals, which is how musl runs a
timer's function in a thread. Testing this turned up an old bug: a handler
with `SA_RESTART` made `pause()` restart and wait forever; calls Linux
never restarts (waiting for signals, sleeping, `poll` and `select`) now
return `EINTR`. `timertest` checks it all through glibc.

## 23. Disk images and the build tool

### The disk image (`xtask/src/image.rs`)

`target/eliOS.img` is a GPT disk: a protective MBR (so MBR-only tools see
the disk as in use), the GPT header and partition table with their CRC32s,
the backup copies at the end of the disk, and one 128 MiB EFI System
Partition formatted FAT32 by the `fat` crate. eliboot goes at the path UEFI
boots by default (`\EFI\BOOT\BOOTX64.EFI`), so no firmware boot entries are
needed.

`cargo xtask run` *updates* the existing image (replacing only the boot
files) rather than rebuilding it, so files saved to `/disk` persist between
runs. `--fresh` and `cargo xtask image` start over.

`--second-disk` adds `target/second-disk.img`, an empty 2 GiB disk on NVMe
(kept between runs), to partition, format and install to; `--installed`
boots that disk alone, as a PC would after the USB stick is removed.
`--usb-stick` attaches the image as a USB stick instead of a SATA disk (the
firmware boots from USB, and eliOS must find its boot partition over USB,
as on a real PC), and `--usb-hub` puts the stick and keyboard behind a hub.

A raw disk image rather than an ISO: written to a USB stick it becomes an
ordinary, writable disk, and QEMU boots it directly.

### Running QEMU

- Machine `q35`: a modern chipset with AHCI and PCI Express, closer to real
  PCs than the default.
- Firmware: OVMF, the open-source UEFI implementation. Where the
  distribution ships it in two parts (code, and a template for its
  variables), the variables live in `target/ovmf-vars.fd` so boot entries
  persist between runs, as on a real PC; tests get a throwaway copy, and
  `--fresh` starts over.
- The live disk has QEMU's `bootindex=0`, so the firmware starts it even
  when the second disk has eliOS installed. `--installed` sets no boot
  index at all: given one, OVMF replaces its boot order with QEMU's list
  and drops entries like the one the installer adds, which a real PC keeps
  (this cost some head-scratching).
- KVM with the host's CPU model when `/dev/kvm` is usable: hardware
  virtualization, dozens of times faster than emulation. eliOS boots in
  about half a second under KVM.
- The serial port is connected to the terminal through a chardev with
  `signal=off`, so Ctrl+C reaches eliOS instead of stopping QEMU.
- `-no-reboot`: a triple fault stops QEMU instead of looping through
  reboots, which keeps crash output visible.

### Sessions (`xtask/src/session.rs`)

`cargo xtask session -- steps...` boots eliOS headless, types commands at
the shell (waiting for each prompt, `#` or `$`), can wait for text to
appear (`@wait Password:`), press keys through the QEMU monitor and take
screenshots, then prints a clean transcript. Sessions pass
`autologin=root` on the kernel command line (`\EFI\eliOS\cmdline.txt`),
so they start at a root shell; `--login-screen` leaves it out, to test the
greeter (log in with `@early` steps). Boot counts as done at a shell
prompt or a login prompt. It's how end-to-end behavior is checked
without a person typing. `@timeout SECONDS` changes how long the following
steps may take (installing Firefox takes minutes).

### The package mirror (`xtask/src/mirror.rs`)

`cargo xtask mirror` serves Alpine's repositories on port 8086 from
`target/alpine-cache/`, fetching each file from Alpine's CDN the first time
(package lists are refreshed daily). QEMU's guest reaches the host as
10.0.2.2, so a test session adds
`http://10.0.2.2:8086/alpine/v3.24/main` (and `community`) to
`system.conf` and installs Firefox in seconds instead of downloading
100 MB again. eli checks Alpine's signatures, so the mirror isn't
trusted.

## 24. Testing

- **Kernel tests** (`ktest!`) register themselves in a dedicated linker
  section; the linker script brackets the section with symbols, so the
  harness finds every test without a central list. `cargo xtask test`
  builds the kernel with `cfg(test)` (and `harness = false`, since Rust's
  normal test harness needs an OS), boots it, runs all tests, and exits QEMU
  through its `isa-debug-exit` device with a status xtask understands. A
  failing assertion panics, which prints the report and exits with the
  failure status.
- Tests run after the complete boot, with interrupts and the scheduler
  running, so they cover real behavior: thread preemption, timer accuracy,
  keyboard interrupts (the PS/2 controller is told to fake a key press),
  a three-process pipeline, the C library self-test, disk I/O through AHCI.
- **The FAT crate** has host unit tests (`cargo test -p fat`).
- **The tls crate** is tested against OpenSSL and, with
  `RADIUS_SERVER=host:port`, a FreeRADIUS server (secret `testing123`,
  user `testuser`, password `testpass`): `cargo test -p tls`.
- **End-to-end:** `cargo xtask session` and screenshots.

Every phase was also tested in release builds, since optimizations can
expose bugs that debug builds hide.

## 25. Limits and what's next

Current limits, roughly in order of how much they'd matter to a user:

- **Scheduling is simple**: one shared queue, no priorities, and no CPU
  affinity, so threads move between CPUs freely.
- **eliOS's own Rust programs are single-threaded**: libeli has no thread
  API yet (C programs have pthreads).
- **The kernel can fault on user memory another thread unmapped**: system
  calls check user memory before using it, so a thread that unmaps a buffer
  while another thread's system call is using it makes the kernel fault
  (and panic). Linux avoids this with "exception tables" that turn such
  faults into errors; eliOS will need the same.
- **USB:** keyboards, mice (boot protocol only), storage and hubs; no UAS,
  the faster USB 3 storage protocol (USB 3 disks also speak Bulk-Only,
  which eliOS uses), and Bluetooth controllers are only identified. USB 3
  hubs and transaction translators are untested outside the
  specifications.
- **Wi-Fi** (Intel 7260/3160 only) joins open, WPA2-Personal and
  WPA2-Enterprise (PEAP/MS-CHAPv2) networks; scanning works on the laptop,
  joining is still being tried. No WPA3, other EAP methods, certificate
  verification, roaming or 802.11n aggregation.
- **Networking is IPv4 only**, with one network card (Intel e1000
  family), no TLS (so `https` doesn't work), and TCP without congestion
  control or out-of-order buffering. Unix sockets live only in in-memory
  directories (not on ext2 or FAT).
- **Packages:** install scripts aren't run, and without TLS, programs
  that need HTTPS (git cloning from GitHub, for one) can't reach those
  servers. The system image itself is updated by replacing the files on
  the EFI partition; generations cover packages and configuration.
- **Disk I/O is polled** and holds the volume lock for whole operations.

## 26. Reference tables

### Physical memory at boot (QEMU, 256 MiB)

The memory map is logged at debug level on every boot (serial output).
Typical figures: about 197 MiB usable at entry, 48 MiB reclaimed from the
bootloader afterwards, the kernel image under 1 MiB plus its 123 KiB data
segment.

### User address space

| Address | Contents |
|---|---|
| `0x200000` (typically) | static executables (eliOS programs) |
| `0x400000` (typically) | static executables (GCC) |
| `0x5555_5560_0000` | position-independent executables |
| below `0x7000_0000_0000` | `mmap` regions, placed top-down |
| `0x7F00_0000_0000` | the dynamic linker, for dynamically linked programs |
| `0x7FFF_FF7F_F000`-`0x7FFF_FFFF_F000` | the stack (8 MiB, demand-paged) |

### Selectors

| Selector | Descriptor |
|---|---|
| 0x08 | kernel code |
| 0x10 | kernel data |
| 0x1B | user data (RPL 3) |
| 0x23 | user code (RPL 3) |
| 0x28 | TSS |
