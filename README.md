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
```

Kernel and bootloader logs appear on the serial port, which is your terminal.
The kernel also logs to the screen; `debug!` messages go to serial only.
Once booted, eliOS echoes what you type (Ctrl+L clears the screen).

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
| `xtask/`    | Build tool: compiles everything, lays out the boot volume, runs QEMU |

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
- [ ] **Phase 3: Memory.** Physical frame allocator, kernel-owned page tables
      (drop the identity map, reclaim bootloader memory), kernel heap (`alloc`)
- [ ] **Phase 4: Multitasking.** Threads, context switching, preemptive
      scheduler, synchronization
- [ ] **Phase 5: Userspace.** Ring 3, `syscall`/`sysret`, ELF process loading,
      per-process address spaces
- [ ] **Phase 6: Storage.** ACPI parsing, PCI, a disk driver (AHCI/NVMe/virtio),
      VFS, a native eliOS file system plus FAT32/ext2 for interop
- [ ] **Phase 7: Linux compatibility.** Linux syscall ABI: static binaries
      (e.g. BusyBox), then the dynamic linker and glibc/musl programs
- [ ] **Phase 8+:** SMP, networking (own TCP/IP stack), GUI and compositor,
      real-hardware support (custom GPT/FAT image builder for USB boot)
