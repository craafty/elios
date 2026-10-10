# eliOS

An operating system for x86_64, written from scratch in Rust.

**Everything is custom.** eliOS has its own UEFI bootloader, kernel, drivers
and file systems, and uses no third-party crates: only Rust's `core` and
`alloc`. The firmware interface, ELF loader and page tables are all
hand-written.

**But it should run existing software.** The kernel's internals are eliOS's own
design. At the boundary to programs, eliOS will implement the Linux system call
ABI, so unmodified Linux binaries can run on it. That way originality goes into
how the OS works, and compatibility comes from what programs see.

## Quick start

Requirements: [rustup](https://rustup.rs) (the toolchain in
`rust-toolchain.toml` installs automatically), `qemu-system-x86_64`, and OVMF
UEFI firmware (`ovmf` package on Debian/Ubuntu).

```sh
cargo xtask run               # build and boot in a QEMU window
cargo xtask run --headless    # serial output only
cargo xtask run --gdb         # wait for gdb on :1234
cargo xtask run --trace       # log interrupts/resets to target/qemu.log
cargo xtask test              # run the kernel's tests inside QEMU
cargo xtask build --release   # just build
cargo xtask image             # a fresh bootable disk image
```

Everything is built into a disk image, `target/eliOS.img`: a GPT disk with
one EFI System Partition (FAT32) holding eliboot, the kernel and the initrd.
`run` updates the boot files on the existing image and keeps everything else,
so files you save under `/disk` are still there next time (`--fresh` starts
over).

### Real hardware

The image boots on UEFI PCs (with Secure Boot off). Write it to a USB stick,
replacing everything on it:

```sh
cargo xtask image --release
sudo dd if=target/eliOS.img of=/dev/sdX bs=4M conv=fsync   # sdX = the stick
```

Real machines vary far more than QEMU. eliOS needs a PS/2 keyboard (or
firmware that emulates one for USB keyboards, which many do), and reads and
writes disks through AHCI (SATA) or NVMe controllers; without a supported
disk it still boots and runs from memory. `poweroff` and `reboot` use ACPI.

eliOS boots to a shell. Type in the QEMU window, or in the terminal you ran
`cargo xtask run` from: the serial port is a second console. Try `help`,
`fetch`, `ls -l /bin`, `ps`, or `cat ~/README.txt` for a tour.

Building user programs needs the `rust-src` component (installed by
`rust-toolchain.toml`): their target, `x86_64-unknown-linux-none`, builds the
standard library from source.

## Userspace

eliOS implements the **Linux x86_64 system call ABI**: the same call numbers,
registers, structures and error codes. Its own programs are written against
that ABI, so they are ordinary static Linux executables (they run on Linux
too).

- `user/eli` is **libeli**, eliOS's runtime for Rust programs: startup, a
  memory allocator, and a small standard library (I/O, files, processes,
  terminals, time).
- `user/utils` is **eliutils**, one binary holding the shell, `init` and the
  core commands. `/bin` has a link per command; the name it's run as picks
  the command.
- `rootfs/` is copied into the root file system. `cargo xtask build` packs it
  with the programs into a cpio initrd, which eliboot loads and the kernel
  unpacks into an in-memory file system.

### Running Linux programs

`user/c/` holds C programs built by the host's ordinary C compiler and C
library (glibc): `hello-c`, `ctest` (a self-test of the C library: memory,
files, directories, time, processes, pipes) and `sigtest` (signal handlers).
They're plain Linux executables, unmodified, and run on eliOS. So does
`hello-dynamic`, which is dynamically linked: eliOS loads glibc's own dynamic
linker, which loads `libc.so.6` (copied from the host into the initrd). Run
`strace <program>` to watch any program's system calls.

If the build machine has a static BusyBox (Debian/Ubuntu: `busybox-static`),
it's included unmodified, with links in `/usr/bin` for each of its commands
that eliutils doesn't have: `vi`, `less`, `awk`, `sed`, `find`, `tar`,
`gzip`, `diff`, `top`, `df` and about 200 more.

The shell (`sh`) has pipes, redirection (`<`, `>`, `>>`, `2>&1`), `;` `&&`
`||`, background jobs, quoting, `$VARIABLES`, `~`, `*` wildcards, scripts,
history (arrow keys) and Tab completion. Ctrl+C interrupts the foreground
program: each pipeline runs in its own process group.

## Tests

Kernel tests live next to the code they test, inside `#[cfg(test)]` modules:

```rust
crate::ktest! {
    fn spinlock_guards_its_value() {
        let lock = SpinLock::new(5);
        *lock.lock() += 1;
        assert_eq!(*lock.lock(), 6);
    }
}
```

`cargo xtask test` builds a test kernel, boots it with eliboot in QEMU, runs
every registered test and reports the result through QEMU's exit code. A
failing assertion prints the panic and stops the run.

## The font

The console font is the eliOS 8x16 font, drawn as ASCII art in
`kernel/font/eli-8x16.txt`. Edit a glyph there and rebuild; `kernel/build.rs`
turns the file into the kernel's glyph tables.

## Layout

| Path        | What                                                             |
|-------------|------------------------------------------------------------------|
| `boot/`     | **eliboot**, the UEFI bootloader (`x86_64-unknown-uefi`)         |
| `kernel/`   | The kernel (`x86_64-unknown-none`), linked at `0xFFFFFFFF80000000` |
| `kernel/font/` | The console font, as editable ASCII art                      |
| `bootinfo/` | The boot protocol: the contract between eliboot and the kernel   |
| `fat/`      | FAT32 file systems, shared by the kernel and the image builder   |
| `user/eli/` | libeli, the runtime for user programs                            |
| `user/utils/` | eliutils: shell, init and commands                             |
| `rootfs/`   | Files for the root file system (`/etc`, `/home`)                 |
| `xtask/`    | Build tool: compiles everything, builds the initrd, runs QEMU    |

## How it boots

1. UEFI firmware loads `\EFI\BOOT\BOOTX64.EFI` (eliboot) from the boot volume.
2. eliboot reads `\EFI\eliOS\kernel.elf`, copies each ELF segment into memory,
   and builds page tables: the kernel at its higher-half address with per-segment
   permissions, plus a direct map of all RAM at `0xFFFF800000000000`.
3. It collects the memory map, framebuffer and ACPI RSDP into a `BootInfo`
   block, exits UEFI boot services, switches to the new page tables and stack,
   and jumps to the kernel.

The exact machine state at kernel entry is documented in `bootinfo/src/lib.rs`.

## Roadmap

- [x] **Phase 0: Boot.** Custom UEFI bootloader, ELF loading, paging and the
      handoff, serial output, framebuffer splash
- [x] **Phase 1: Output and safety nets.** Custom bitmap font and framebuffer
      text console with ANSI colors, logging, in-QEMU test harness
- [x] **Phase 2: CPU and interrupts.** GDT/TSS with IST stacks, IDT with
      custom entry stubs and exception reports, ACPI MADT, Local APIC + I/O
      APIC, TSC/APIC timer calibration, PS/2 keyboard
- [x] **Phase 3: Memory.** Bitmap frame allocator, kernel-owned page tables
      (W^X kernel image, RAM-only direct map, write-combining framebuffer,
      no identity map), guarded kernel stacks, bootloader memory reclaimed,
      kernel heap with size classes (`alloc`), early log replay
- [x] **Phase 4: Multitasking.** Kernel threads on guarded stacks,
      assembly context switch, preemptive round-robin scheduler, sleep,
      wait queues, join, stack reaping
- [x] **Phase 5: Userspace.** Ring 3 and `syscall`, per-process address
      spaces with demand paging, ELF loading with the Linux initial stack,
      fork/exec/wait/exit, signals (default actions), the Linux system call
      ABI (~90 calls), VFS with an in-memory file system, cpio initrd, pipes,
      a TTY with line editing and job control, libeli, and eliutils (shell,
      init and ~40 commands)
- [x] **Phase 6: Linux compatibility.** Unmodified glibc programs, static
      and dynamically linked (via glibc's own ld.so); signal handlers with
      Linux's signal frame, masks, SA_RESTART and alternate stacks; poll and
      select; /proc; strace
- [x] **Phase 7: Storage.** PCI, AHCI (SATA) and NVMe drivers, GPT/MBR partitions,
      a FAT32 file system with long names (shared with the image builder),
      the boot disk mounted at /disk, bootable GPT disk images
- [ ] **Next:** SMP, threads in user programs, networking (own TCP/IP
      stack), USB, a GUI and compositor
