# eliOS

An operating system for x86_64, written from scratch in Rust.

**Everything is custom.** eliOS has its own UEFI bootloader, kernel, drivers,
file systems, shell and core commands, and uses no third-party crates: only
Rust's `core` and `alloc`. The firmware interface, ELF loader, page tables,
scheduler, FAT32 driver and console font are all hand-written.

**And it runs existing software.** The kernel's internals are eliOS's own
design, but at the boundary to programs it implements the Linux system call
ABI. Unmodified Linux programs run on it: C programs built with glibc (static
or dynamically linked), and BusyBox with its ~200 commands including `vi`.

## Try it

You need [rustup](https://rustup.rs), QEMU (`qemu-system-x86_64`) and OVMF,
QEMU's UEFI firmware. On Debian or Ubuntu:

```sh
sudo apt install qemu-system-x86 ovmf
# Optional extras that end up inside eliOS:
sudo apt install gcc busybox-static
```

Then, from this directory:

```sh
cargo xtask run
```

The first build installs the nightly Rust toolchain eliOS uses and takes a
few minutes. A QEMU window opens and eliOS boots to a shell. You can type in the
window, or in the terminal you ran the command from (the serial port is a
second console). QEMU uses hardware virtualization (KVM) when it's
available.

Things to try:

```sh
help                          # every command
fetch                         # system information
cat ~/README.txt              # a short tour
edit notes.txt                # write something (Ctrl+S saves, Ctrl+Q quits)
ls -l /bin /usr/bin           # eliOS's commands, and Linux programs
ps; free; uptime; date; df -h
seq 100 | grep 7 | wc -l      # pipes
ctest                         # glibc's view of eliOS: a C library self-test
threadtest                    # pthreads: mutexes, condition variables, signals
hello-dynamic                 # a dynamically linked Linux program
strace hello-c                # watch a program's system calls
vi poem.txt                   # BusyBox vi (if busybox-static was installed)
cp notes.txt /disk/           # /disk is the boot disk: files there persist
poweroff                      # shut down (QEMU exits)
```

Other ways to run it:

```sh
cargo xtask run --headless    # no window: the terminal is the console
cargo xtask run --fresh       # start with a fresh disk image
cargo xtask run --release     # optimized build
cargo xtask run --gdb         # wait for a debugger on localhost:1234
cargo xtask run --trace       # log interrupts and CPU resets to target/qemu.log
cargo xtask run --cpus 8      # more (or fewer) CPUs; the default is 4
cargo xtask test              # boot a test kernel and run its 75 tests
cargo xtask image             # a fresh bootable disk image, target/eliOS.img
```

### On real hardware

The disk image boots on UEFI PCs with Secure Boot turned off. Write it to a
USB stick, which erases the stick:

```sh
cargo xtask image --release
sudo dd if=target/eliOS.img of=/dev/sdX bs=4M conv=fsync   # sdX = the stick
```

Real machines vary far more than QEMU. Keyboards work over PS/2 or USB
(plugged into the computer itself; not yet through a hub). It reads and writes
SATA (AHCI) and NVMe disks; without a supported disk it still runs from
memory. `poweroff` and `reboot` use ACPI. Other virtual machines with EFI
enabled may work too, but QEMU is the one eliOS is tested on.

## How it works

### Booting

1. UEFI firmware loads eliboot (`\EFI\BOOT\BOOTX64.EFI`) from the disk's EFI
   System Partition.
2. eliboot loads the kernel and the initrd from `\EFI\eliOS\`, maps each
   kernel segment with exactly the permissions it needs, builds a direct map
   of RAM, collects the memory map, framebuffer and ACPI tables, leaves UEFI
   behind and jumps to the kernel. The handoff is specified in
   `bootinfo/src/lib.rs`.
3. The kernel sets up its own descriptor tables, interrupt controllers,
   timers, page tables, heap and scheduler, unpacks the initrd into an
   in-memory file system, mounts the boot disk at `/disk`, and starts
   `/sbin/init`, which starts the shell.

### The kernel

- **Memory:** a bitmap frame allocator; kernel page tables with a W^X kernel
  image, guard pages under every kernel stack, and no identity map; a heap
  with size classes. User address spaces are demand-paged.
- **Processes:** preemptive round-robin scheduling on every CPU; user
  processes with threads, `fork`, `exec` (ELF, `#!` scripts, dynamic
  linkers), `wait`, process groups, and Linux-style signals with handlers.
- **Files:** a VFS over an in-memory file system, a FAT32 driver for disks,
  `/dev`, `/proc`, pipes, and a terminal with line editing and job control.
- **Devices:** framebuffer console (with ANSI escape codes and its own 8x16
  font), PS/2 and USB keyboards (an xHCI driver), serial port, Local and
  I/O APICs, PIT/TSC/APIC timers, CMOS clock, PCI with MSI/MSI-X, AHCI and
  NVMe, ACPI power management.
- **The Linux ABI:** about 100 system calls, with Linux's numbers,
  structures and error codes, plus a few of eliOS's own (process list, kernel
  log, tracing).

### User programs

- `user/eli` is **libeli**, the runtime for eliOS's own Rust programs:
  startup, a memory allocator, and a small standard library. It only uses
  Linux system calls, so these programs are ordinary Linux executables.
- `user/utils` is **eliutils**: the shell, `init`, a text editor and about
  40 commands in one binary, linked under each command's name in `/bin`.
- `user/c` holds C programs compiled by the host's C compiler and glibc. Ones
  named `*-dynamic.c` are dynamically linked, and the libraries they need are
  copied from the host.
- If the host has a static BusyBox, it's included, with links in `/usr/bin`
  for the commands eliutils doesn't have.

The shell has pipes, redirection (`<`, `>`, `>>`, `2>&1`), `;` `&&` `||`,
background jobs, quoting, `$VARIABLES`, `~`, `*` wildcards, scripts, history
(arrow keys) and Tab completion. Each pipeline runs in its own process group,
so Ctrl+C interrupts the program and not the shell.

### Tests

`cargo xtask test` boots a test build of the kernel, which runs every test
and reports through QEMU's exit code. Tests live next to the code they test:

```rust
crate::ktest! {
    fn spinlock_guards_its_value() {
        let lock = SpinLock::new(5);
        *lock.lock() += 1;
        assert_eq!(*lock.lock(), 6);
    }
}
```

They range from data structures to running real programs: a three-process
pipeline, the C library self-test, and reading and writing the disk. The FAT
crate also has host unit tests (`cargo test -p fat`).

### The font

The console font is drawn as ASCII art in `kernel/font/eli-8x16.txt`; edit a
glyph and rebuild.

## Layout

| Path            | What                                                         |
|-----------------|--------------------------------------------------------------|
| `boot/`         | **eliboot**, the UEFI bootloader                             |
| `bootinfo/`     | The boot protocol shared by eliboot and the kernel           |
| `kernel/`       | The kernel                                                   |
| `kernel/font/`  | The console font                                             |
| `fat/`          | FAT32, shared by the kernel and the disk image builder       |
| `user/eli/`     | libeli, the runtime for eliOS's Rust programs                |
| `user/utils/`   | eliutils: shell, init, editor and commands                   |
| `user/c/`       | C test programs built with the host's toolchain              |
| `rootfs/`       | Files for the root file system (`/etc`, `/home`)             |
| `xtask/`        | The build tool: builds everything and the disk image, runs QEMU |

## Roadmap

- [x] **Boot:** UEFI bootloader, ELF loading, paging, the kernel handoff
- [x] **Output:** framebuffer console with a custom font, logging, tests
- [x] **CPU and interrupts:** GDT/TSS, IDT, exception reports, ACPI, APICs,
      timers, keyboard
- [x] **Memory:** frame allocator, kernel page tables, guard pages, heap
- [x] **Multitasking:** threads, context switching, preemptive scheduler
- [x] **Userspace:** processes, the Linux system call ABI, VFS, initrd,
      pipes, terminal, shell and commands
- [x] **Linux compatibility:** glibc programs (static and dynamic), signal
      handlers, poll/select, /proc, strace, BusyBox
- [x] **Storage:** PCI, AHCI, NVMe, partitions, FAT32, bootable disk images
- [x] **Hardware support:** ACPI power off and reset, KVM
- [x] **Threads:** `clone`, futexes, per-thread signals; glibc's pthreads
- [x] **Multiple CPUs:** startup through a real-mode trampoline, per-CPU
      data, a scheduler shared by all CPUs, TLB shootdowns
- [x] **USB:** an xHCI host controller driver, HID keyboards, plugging and
      unplugging
- [ ] **Next:** networking with an own TCP/IP stack, USB hubs and storage,
      and a graphical desktop
