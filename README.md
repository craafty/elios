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
cargo xtask build --release   # just build
```

Kernel and bootloader logs appear on the serial port, which is your terminal.

## Layout

| Path        | What                                                             |
|-------------|------------------------------------------------------------------|
| `boot/`     | **eliboot**, the UEFI bootloader (`x86_64-unknown-uefi`)         |
| `kernel/`   | The kernel (`x86_64-unknown-none`), linked at `0xFFFFFFFF80000000` |
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
- [ ] **Phase 1: Output and safety nets.** Custom bitmap font and framebuffer
      text console, logging, in-QEMU test harness
- [ ] **Phase 2: CPU and interrupts.** GDT/TSS, IDT and exception handlers,
      Local APIC + I/O APIC, timer, keyboard
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
