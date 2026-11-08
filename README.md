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
few minutes. A QEMU window opens and eliOS boots to its login screen.
Development builds come with an account to try them with: user `test`,
password `1234` (an administrator: it can use `sudo`). Left and Right on
the *session* line choose between the desktop (the default) and the text
console, and the choice is remembered for each person;
Enter logs in, and logging out comes back to the login screen (`root`,
which has no password on the live system, works too). You can type in
the window, or in the terminal you ran the command from (the serial port
is a second console). QEMU uses hardware virtualization (KVM) when it's
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
sudo eli add vim git          # programs from Alpine Linux (needs the internet)
eli generations               # every change is a generation you can go back to
sudo adduser --admin alice    # an account (--admin: may use sudo)
su - alice                    # become alice; or `logout` and log in as her
sudo whoami                   # as alice: run a command as root
sudo poweroff                 # shut down (QEMU exits)
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
live in memory on the live system), then choose *Desktop* on the login
screen, or in a text session:

```sh
desktop term                  # the desktop, with a terminal in it
```

`term` is eliOS's own terminal (xterm-compatible: 256 colours and true
colour, mouse reporting, an alternate screen for `vi` and `top`, a 5000-line
scrollback on Shift+PgUp, Ctrl+Shift+C/V to copy and paste through the
desktop's clipboard, which Firefox and other programs share, Ctrl+= and
Ctrl+- for the font size). `files` is the file explorer: places (home,
the root, every disk and USB stick) in a sidebar, sortable columns, a path
bar you can type into, and rename, new folder, copy, cut, paste and delete
from the right-click menu or the keyboard (F2, Delete, Ctrl+C/X/V);
programs and text files open in a terminal; copying a file also puts its
path on the clipboard. The eliOS button (or the Super key on its own) opens
the app menu: every installed program with a `.desktop` file, so Firefox
appears there, and gets its own bar button, once it's added. The bar also
shows the network (the Wi-Fi network's name, Wired or Offline) and the
time. Super+Enter opens a terminal,
Super+E the file explorer, Super+Tab switches windows, Super+Q closes one, and
Ctrl+Alt+Backspace goes back to the text console.

Firefox is Alpine's, unmodified, with its multi-process design (a fork
server, content processes, a socket process) working on eliOS's kernel.
It needs about 4 GiB of memory in QEMU (`cargo xtask run --memory 4G`):

```sh
sudo eli add firefox font-dejavu glycin-image-rs glycin-svg
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

`installer` (as root: `sudo installer`) copies eliOS onto another
disk: erasing it, or alongside another system in the disk's free space
(the disk needs an EFI System Partition, which any PC with a UEFI-installed
OS has). It asks for your account (the live system's `test` account stays
behind); the installed system boots to the login screen, and your
account can use `sudo`. Programs added with `eli` on the
live system come along. On the installed system, the system itself boots
from the image every time, and your files, packages and settings live on
the data partition; press a key while it starts for a menu of earlier
generations. Try it in QEMU:

```sh
cargo xtask run --second-disk     # then, inside eliOS: sudo installer
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
   `/sbin/init`, which runs the login screen (`greeter`), which starts
   your shell or the desktop.

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
  keyboards and mice (laptop touchpads and TrackPoints; Synaptics touchpads
  in absolute mode, so a ClickPad clicks and two fingers scroll;
  `/proc/mouse` shows what the touchpad reported), Linux-style input
  devices (`/dev/input/event*`), serial port, Local and
  I/O APICs, PIT/TSC/APIC timers, CMOS clock, PCI with MSI/MSI-X, AHCI and
  NVMe, Intel e1000 network cards (including the I217/I218 built into
  Haswell and Broadwell laptops), Intel 7260/3160 Wi-Fi (scanning, open,
  WPA2-Personal and WPA2-Enterprise (PEAP with MS-CHAPv2, as eduroam and
  campus networks use) networks, with the `wifi` command; written for the
  T440p and still being tried there), the 7260's Bluetooth (Intel's
  firmware patch, scanning, Secure Simple Pairing with kept link keys,
  L2CAP, keyboards and mice, and headphones: A2DP audio with eliOS's own
  SBC encoder, with the `bluetooth` command), ACPI power management.
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
ext2 tests check every volume they make with Linux's `e2fsck`. The `tls`
crate's tests (`cargo test -p tls`) talk to `openssl s_server`, and, with
`RADIUS_SERVER=host:port` set, sign in to a FreeRADIUS server over PEAP.
The `truetype` crate's (`cargo test -p truetype`) check the bundled font
and the rasterizer against shapes whose coverage is known exactly, and the
`sbc` crate's (`cargo test -p sbc`) decode what the encoder makes with
ffmpeg and measure how close it comes back.

### The font

The console font is drawn as ASCII art in `kernel/font/eli-8x16.txt`; edit a
glyph and rebuild. The desktop uses a TrueType font instead, DejaVu Sans
Mono (`rootfs/usr/share/fonts/dejavu/`, with its licence), drawn by
eliOS's own rasterizer in `truetype/`.

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
| `tls/`          | TLS 1.2 client and PEAP/MS-CHAPv2, for enterprise Wi-Fi       |
| `truetype/`     | TrueType fonts: reading and anti-aliased rasterizing          |
| `sbc/`          | The SBC audio encoder, for Bluetooth headphones               |
| `user/ui/`      | The toolkit: Wayland windows, drawing, widgets (and `uidemo`) |
| `user/term/`    | `term`, the terminal                                         |
| `user/files/`   | `files`, the file explorer                                   |
| `user/desktop/` | The desktop: a Wayland compositor                            |
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
      rollback, a boot menu of generations, the time zone
- [ ] **Laptop (ThinkPad T440p):** the log saved to the USB stick, USB
      ports routed to xHCI on Intel 8-series chipsets, PS/2 and USB mice
      and `/dev/input`, the I217 Ethernet (works: gigabit, DHCP), sound
      (works: `speaker-test` and YouTube in Firefox play; headphones
      plugged in now mute the speakers), the Intel 7260 Wi-Fi driver (works:
      scanning, and joining Purdue's WPA2-Enterprise PAL3.0 with DHCP;
      open and WPA2 networks use the same path) and Bluetooth (the
      Intel firmware patch loads and scans find devices; pairing, L2CAP
      and HID keyboards and mice, and A2DP audio to headphones (SDP,
      AVDTP, our own SBC encoder; the sound card's output follows the
      headset), checked against a pretend controller in QEMU; next,
      trying them on the laptop, then the headset's microphone (HFP))
- [x] **Desktop groundwork:** copy-on-write `fork`, shared memory
      (`memfd`, `MAP_SHARED`, a page cache), `epoll`, `eventfd`,
      `timerfd`, Unix domain sockets with descriptor passing,
      pseudo-terminals, `/dev/fb0`
- [x] **Desktop:** a Wayland compositor of eliOS's own (`desktop`), running
      Alpine's Wayland programs
- [ ] **Our own desktop programs:** a login screen with a Text/Desktop
      choice (done), a TrueType rasterizer and bundled font (done), a
      widget library (`user/ui`, done), a terminal (`term`, done), a file
      explorer (`files`, done), a shared clipboard, an app menu (Firefox
      appears once it's added) and a status area (done), smoother touchpad
      and TrackPoint movement (resynchronizing packets, speed-based
      acceleration; to try on the laptop); next, the desktop preinstalled
- [x] **Firefox:** Alpine's Firefox browses HTTPS sites in the desktop
      (package triggers, `mremap`, `statx`, `prctl` and `arch_prctl`
      additions, `/proc/sys`, namespaces refused so sandboxes fall back),
      tuned with a sampling profiler (`/proc/PID/profile`): per-file
      `poll` wakeups, a kernel heap cache, damage-only redraws
- [x] **Sound:** an Intel HD Audio driver (codec routes found like
      Linux's generic driver, so no per-laptop tables) behind Linux's
      ALSA interface, so `libasound` programs play and record;
      PulseAudio (`eli add pulseaudio pulseaudio-alsa`) runs on it and the
      desktop starts it, so Firefox's `getUserMedia` gets the microphone
- [x] **Faster clocks:** a vDSO, so `clock_gettime`, `gettimeofday` and
      `time` read the TSC in the program without a system call (glibc and
      musl both use it)
- [x] **Camera:** a USB video class driver (descriptors, probe and
      commit, isochronous xHCI transfers, frame assembly) behind Linux's
      V4L2 interface (`/dev/video0`), tested against a pretend camera
      xtask attaches to QEMU over usbredir (`videotest`), and a Camera
      app on the desktop to see it; Firefox's `getUserMedia` uses it as
      is; to try on the T440p's webcam
- [x] **Video calls:** WebRTC in Firefox connects (route netlink for
      `getifaddrs` and `ip`, interface ioctls on every socket, `waitid`);
      a loopback call with the camera and microphone plays, in VP8 or
      H.264 (Cisco's OpenH264 plugin, which Firefox downloads), with
      Firefox's own seccomp-bpf sandboxes on (filters, traps to `SIGSYS`,
      thread sync); next, a real Teams meeting
- [ ] **Next:** Wi-Fi working on the T440p, Bluetooth pairing and devices
