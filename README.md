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

## What makes it different

- **One file describes your computer.** `/etc/system.conf` lists its name,
  its accounts and its programs; `eli apply` makes the system match.
- **It can't slowly break.** The system starts fresh from one image at
  every boot; your files, settings and programs live separately. Every
  change is a numbered generation: `eli rollback` (or a menu at boot)
  returns to any earlier one.
- **Tens of thousands of programs.** `eli add git vim python3` installs
  them from Alpine Linux's repositories, checked against Alpine's
  signatures by eliOS's own code; they run unmodified.

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
few minutes. A QEMU window opens and eliOS boots to a shell, logged in as
root (the live system has no password for root). You can type in the
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
lsblk; df                     # disks, partitions, and free space
ifconfig; ping -c 3 10.0.2.2  # the network (QEMU's virtual router)
wget -O - http://example.com/ # the internet, if the host has it
nettest                       # sockets through glibc: TCP, UDP, poll...
eli add vim git               # programs from Alpine Linux (needs the internet)
eli generations; eli rollback # every change is a generation you can go back to
adduser --admin alice         # an account (--admin: may use sudo)
su - alice                    # become alice; or `logout` and log in as her
sudo whoami                   # as alice: run a command as root
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
cargo xtask run --memory 3G   # more memory (the default is 1G)
cargo xtask run --forward 8080:80  # reach port 80 in eliOS at localhost:8080
cargo xtask run --second-disk # an empty 2 GiB disk to format, mount, install to
cargo xtask run --usb-stick   # start from a (virtual) USB stick, as on a real PC
cargo xtask test              # boot a test kernel and run its tests
cargo xtask image             # a fresh bootable disk image, target/eliOS.img
```

### The desktop

eliOS has a small graphical desktop of its own, `desktop`, which runs
Wayland programs. Start QEMU with some memory to spare (installed packages
live in memory on the live system), then in eliOS:

```sh
eli add foot font-dejavu      # a terminal, and a font for it
desktop foot                  # the desktop, with a terminal in it
```

The bar's buttons start a terminal or Firefox; Super+Enter opens a
terminal, Super+Tab switches windows, Super+Q closes one, and
Ctrl+Alt+Backspace goes back to the text console.

Firefox is Alpine's, unmodified, with its multi-process design (a fork
server, content processes, a socket process) working on eliOS's kernel.
It needs about 4 GiB of memory in QEMU (`cargo xtask run --memory 4G`):

```sh
eli add firefox font-dejavu glycin-image-rs glycin-svg
desktop firefox https://example.com
```

Firefox is a 100 MB download. `cargo xtask mirror` keeps Alpine's packages
on this computer, so testing doesn't fetch them again on every boot; point
eliOS at it with
`echo "repositories = http://10.0.2.2:8086/alpine/v3.24/main http://10.0.2.2:8086/alpine/v3.24/community" >> /etc/system.conf`.

### On real hardware

The disk image boots on UEFI PCs with Secure Boot turned off. Write it to a
USB stick, which erases the stick:

```sh
cargo xtask image --release
sudo dd if=target/eliOS.img of=/dev/sdX bs=4M conv=fsync   # sdX = the stick
```

### Installing it

`installer` (as root, which the live system is) copies eliOS onto another
disk: erasing it, or alongside another system in the disk's free space
(the disk needs an EFI System Partition, which any PC with a UEFI-installed
OS has). It asks for your account; the installed system boots to a login
prompt, and your account can use `sudo`. Programs added with `eli` on the
live system come along. On the installed system, the system itself boots
from the image every time, and your files, packages and settings live on
the data partition; press a key while it starts for a menu of earlier
generations. Try it in QEMU:

```sh
cargo xtask run --second-disk     # then, inside eliOS: installer
cargo xtask run --installed       # boot the installed disk on its own
```

**Logs from real hardware.** A machine without a serial port can't show
the kernel's debug output, so the live system saves its whole log to the
stick it started from: `logs/boot.log` on the stick's (FAT) partition,
with the previous boot's in `logs/previous-boot.log`. It's brought up to
date every two seconds, so it survives a hang; read it on any other
computer. (`savelog=0` in `\EFI\eliOS\cmdline.txt` turns it off, and
`savelog` turns it on for an installed system.)

Real machines vary far more than QEMU. Keyboards work over PS/2 or USB,
and eliOS reads and writes the USB stick it started from (directly or
through a hub), so files saved to `/disk` stay on the stick. It reads and writes
SATA (AHCI) and NVMe disks; without a supported disk it still runs from
memory. Networking needs an Intel e1000-family card (common in older PCs,
and what VirtualBox and VMware emulate); it configures itself with DHCP.
`poweroff` and `reboot` use ACPI. Other virtual machines with EFI enabled
may work too, but QEMU is the one eliOS is tested on.

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
   `/sbin/init`, which runs `login`, which starts your shell.

### The kernel

- **Memory:** a bitmap frame allocator; kernel page tables with a W^X kernel
  image, guard pages under every kernel stack, and no identity map; a heap
  with size classes. User address spaces are demand-paged.
- **Processes:** preemptive round-robin scheduling on every CPU; user
  processes with threads, `fork`, `exec` (ELF, `#!` scripts, dynamic
  linkers), `wait`, process groups, and Linux-style signals with handlers.
- **Files:** a VFS with mount points over an in-memory file system, FAT32
  and ext2 drivers for disks, `/dev` (with disk and partition device
  files), `/proc`, `/sys`, pipes, and a terminal with line editing and job
  control.
- **Users:** Unix users and groups with real, effective and saved ids,
  owners and permission bits on every file, the sticky bit, umask, and
  setuid programs, all with Linux's rules.
- **Devices:** framebuffer console (with ANSI escape codes and its own 8x16
  font), USB (an xHCI driver, with keyboards, mice, storage and hubs), PS/2
  keyboards and mice (laptop touchpads and TrackPoints), Linux-style input
  devices (`/dev/input/event*`), serial port, Local and
  I/O APICs, PIT/TSC/APIC timers, CMOS clock, PCI with MSI/MSI-X, AHCI and
  NVMe, Intel e1000 network cards (including the I217/I218 built into
  Haswell and Broadwell laptops), ACPI power management. Being brought up
  on real hardware: Intel's 7260 Wi-Fi card and its Bluetooth.
- **Networking:** eliOS's own TCP/IP stack (Ethernet, ARP, IPv4, ICMP, UDP,
  TCP), DHCP, loopback, and the Linux socket API, so programs like
  BusyBox's `wget` and `netstat` work unmodified.
- **The Linux ABI:** about 100 system calls, with Linux's numbers,
  structures and error codes, plus a few of eliOS's own (process list, kernel
  log, tracing).

### User programs

- `user/eli` is **libeli**, the runtime for eliOS's own Rust programs:
  startup, a memory allocator, and a small standard library. It only uses
  Linux system calls, so these programs are ordinary Linux executables.
- `user/utils` is **eliutils**: the shell, `init`, `login`, a text editor
  and about 60 commands in one binary, linked under each command's name in
  `/bin`. `su`, `sudo` and `passwd` are a second, small program, eliauth,
  installed setuid root.
- `user/c` holds C programs compiled by the host's C compiler and glibc. Ones
  named `*-dynamic.c` are dynamically linked, and the libraries they need are
  copied from the host.
- If the host has a static BusyBox, it's included, with links in `/usr/bin`
  for the commands eliutils doesn't have.

The shell has pipes, redirection (`<`, `>`, `>>`, `2>&1`), `;` `&&` `||`,
background jobs, quoting, `$VARIABLES` (and a script's `$1`, `$#`, `"$@"`),
`~`, `*` wildcards, scripts, `exec`, history
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
and ext2 crates also have host unit tests (`cargo test -p fat -p ext2`); the
ext2 tests check every volume they make with Linux's `e2fsck`.

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
| `ext2/`         | ext2, shared by the kernel, `mkfs` and the installer         |
| `apk/`          | Alpine's package format: gzip, tar, hashes, RSA, indexes      |
| `user/eli/`     | libeli, the runtime for eliOS's Rust programs                |
| `user/utils/`   | eliutils: shell, init, editor and commands                   |
| `user/c/`       | C test programs built with the host's toolchain              |
| `rootfs/`       | Files for the root file system (`/etc`; `/etc/skel` is a new home) |
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
- [x] **Storage:** PCI, AHCI, NVMe, partitions, FAT32, ext2, mount points,
      device files, bootable disk images
- [x] **Hardware support:** ACPI power off and reset, KVM
- [x] **Threads:** `clone`, futexes, per-thread signals; glibc's pthreads
- [x] **Multiple CPUs:** startup through a real-mode trampoline, per-CPU
      data, a scheduler shared by all CPUs, TLB shootdowns
- [x] **USB:** an xHCI host controller driver, HID keyboards, mass storage
      (USB sticks), hubs, plugging and unplugging
- [x] **Networking:** an e1000 driver, an own TCP/IP stack, DHCP, sockets,
      and `ifconfig`, `ping`, `host`, `wget` and `nc`
- [x] **Users:** accounts, permissions, setuid programs, `login`, `su`,
      `sudo`, `passwd`, `adduser`, with Linux's file formats and password
      hashes
- [x] **Installer:** partitions a disk (or uses its free space), a data
      partition, a UEFI boot entry through the firmware's runtime services
- [x] **System manager:** `system.conf`, packages from Alpine, generations,
      rollback, a boot menu of generations
- [ ] **Laptop (ThinkPad T440p):** the log saved to the USB stick, USB
      ports routed to xHCI on Intel 8-series chipsets, PS/2 and USB mice
      and `/dev/input`, the I217 Ethernet, and the first stage of the
      Intel 7260 Wi-Fi driver (firmware loaded, MAC address read, radio
      calibrated) and a Bluetooth probe; next, Wi-Fi networking (scanning,
      WPA2), Bluetooth
- [x] **Desktop groundwork:** copy-on-write `fork`, shared memory
      (`memfd`, `MAP_SHARED`, a page cache), `epoll`, `eventfd`,
      `timerfd`, Unix domain sockets with descriptor passing,
      pseudo-terminals, `/dev/fb0`
- [x] **Desktop:** a Wayland compositor of eliOS's own (`desktop`), running
      Alpine's Wayland programs
- [x] **Firefox:** Alpine's Firefox browses HTTPS sites in the desktop
      (package triggers, `mremap`, `statx`, `prctl` and `arch_prctl`
      additions, `/proc/sys`, namespaces refused so sandboxes fall back),
      tuned with a sampling profiler (`/proc/PID/profile`): per-file
      `poll` wakeups, a kernel heap cache, damage-only redraws
- [x] **Sound:** an Intel HD Audio driver (codec routes found like
      Linux's generic driver, so no per-laptop tables) behind Linux's
      ALSA interface, so `libasound` programs play and record
- [ ] **Next:** the camera (for video calls), faster
      clocks (a vDSO), Wi-Fi networking and Bluetooth on the T440p
