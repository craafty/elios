//! /proc: information about the system and its processes, as files.
//!
//! The layout follows Linux's (/proc/meminfo, /proc/<pid>/status and so on)
//! so programs that read /proc find what they expect. A file's contents are
//! generated on its first read and kept for that open file, so a program
//! reading in pieces sees one consistent snapshot.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{FileKind, Inode, Metadata, ramfs};
use crate::errno::{Errno, Result};
use crate::memory::{frames, heap};
use crate::process::credentials::{Gid, Uid};
use crate::process::{self, Pid};
use crate::sync::SpinLock;
use crate::{net, task, time};

type Generator = Box<dyn Fn() -> String + Send + Sync>;
type Writer = Box<dyn Fn(&[u8]) -> Result<()> + Send + Sync>;
/// A directory's entries.
pub(super) type Listing = Vec<(String, Arc<dyn Inode>)>;

/// A file whose contents are produced on demand. Every lookup makes a new
/// one, so each open file gets its own snapshot.
struct Generated {
    inode: u64,
    generate: Generator,
    snapshot: SpinLock<Option<String>>,
}

pub(super) fn generated(generate: impl Fn() -> String + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Generated {
        inode: ramfs::next_inode_number(),
        generate: Box::new(generate),
        snapshot: SpinLock::new(None),
    })
}

impl Inode for Generated {
    fn metadata(&self) -> Metadata {
        // Like Linux, report size 0: the contents aren't known until read.
        Metadata { modified: time::unix_time(), ..Metadata::virtual_file(FileKind::Regular, self.inode, 0o444) }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // Generate outside the lock: generators take other locks.
        if self.snapshot.lock().is_none() {
            let text = (self.generate)();
            self.snapshot.lock().get_or_insert(text);
        }
        let snapshot = self.snapshot.lock();
        let bytes = snapshot.as_deref().unwrap_or_default().as_bytes();
        let start = (offset as usize).min(bytes.len());
        let count = buf.len().min(bytes.len() - start);
        buf[..count].copy_from_slice(&bytes[start..start + count]);
        Ok(count)
    }
}

/// A generated file that can also be written, to control something: by
/// root, and by the members of `group` if it has one.
struct Control {
    file: Generated,
    write: Writer,
    group: Option<u32>,
}

/// The group whose members may control Wi-Fi and Bluetooth (`netdev`, as
/// on Alpine): everyone using the computer, so the desktop's settings
/// work without root.
const NETWORK_GROUP: u32 = 28;

fn control(
    generate: impl Fn() -> String + Send + Sync + 'static,
    write: impl Fn(&[u8]) -> Result<()> + Send + Sync + 'static,
) -> Arc<dyn Inode> {
    control_by(generate, write, None)
}

fn control_by(
    generate: impl Fn() -> String + Send + Sync + 'static,
    write: impl Fn(&[u8]) -> Result<()> + Send + Sync + 'static,
    group: Option<u32>,
) -> Arc<dyn Inode> {
    Arc::new(Control {
        group,
        file: Generated {
            inode: ramfs::next_inode_number(),
            generate: Box::new(generate),
            snapshot: SpinLock::new(None),
        },
        write: Box::new(write),
    })
}

impl Inode for Control {
    fn metadata(&self) -> Metadata {
        match self.group {
            Some(gid) => Metadata { permissions: 0o664, gid, ..self.file.metadata() },
            None => Metadata { permissions: 0o644, ..self.file.metadata() },
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.file.read_at(offset, buf)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        (self.write)(buf)?;
        Ok(buf.len())
    }

    /// `echo 1 > file` truncates first; as on Linux, that's ignored.
    fn truncate(&self, _len: u64) -> Result<()> {
        Ok(())
    }
}

/// A symbolic link whose target is computed when it's followed.
struct Link {
    inode: u64,
    target: Generator,
}

pub(super) fn link(target: impl Fn() -> String + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Link { inode: ramfs::next_inode_number(), target: Box::new(target) })
}

impl Inode for Link {
    fn metadata(&self) -> Metadata {
        Metadata { modified: time::unix_time(), ..Metadata::virtual_file(FileKind::Symlink, self.inode, 0o777) }
    }

    fn read_link(&self) -> Result<String> {
        Ok((self.target)())
    }
}

/// A directory whose entries are listed by a function.
struct Directory {
    inode: u64,
    list: Box<dyn Fn() -> Listing + Send + Sync>,
    /// The owner and group: a process's directory belongs to its user (which
    /// is how `ps` finds out who runs it).
    owner: (Uid, Gid),
}

pub(super) fn directory(list: impl Fn() -> Listing + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Directory { inode: ramfs::next_inode_number(), list: Box::new(list), owner: (0, 0) })
}

impl Inode for Directory {
    fn metadata(&self) -> Metadata {
        Metadata {
            uid: self.owner.0,
            gid: self.owner.1,
            links: 2,
            modified: time::unix_time(),
            ..Metadata::virtual_file(FileKind::Directory, self.inode, 0o555)
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        (self.list)().into_iter().find(|(entry, _)| entry == name).map(|(_, inode)| inode).ok_or(Errno::ENOENT)
    }

    fn entries(&self) -> Result<Listing> {
        Ok((self.list)())
    }
}

/// The /proc directory.
pub fn create() -> Arc<dyn Inode> {
    directory(|| {
        let mut entries: Listing = alloc::vec![
            ("self".into(), link(|| task::current_process().map_or("/proc".into(), |p| format!("/proc/{}", p.pid)))),
            ("meminfo".into(), generated(meminfo)),
            ("uptime".into(), generated(uptime)),
            ("version".into(), generated(version)),
            ("stat".into(), generated(system_stat)),
            ("cpuinfo".into(), generated(cpuinfo)),
            ("loadavg".into(), generated(|| format!("0.00 0.00 0.00 1/{} {}\n", process::count(), process::count()))),
            ("filesystems".into(), generated(filesystems)),
            ("mounts".into(), generated(mounts)),
            ("partitions".into(), generated(partitions)),
            ("cmdline".into(), generated(|| format!("{}\n", crate::cmdline::line()))),
            ("net".into(), net_directory()),
            ("sys".into(), sys_directory()),
            ("asound".into(), directory(|| alloc::vec![("cards".into(), generated(crate::sound::cards_text))])),
            // What the PS/2 mouse (a laptop's touchpad) said and its packet counts.
            ("mouse".into(), generated(crate::mouse::status_text)),
            // Cameras: formats, sizes, rates, and whether they're streaming.
            ("video".into(), generated(crate::video::status_text)),
        ];
        if crate::drivers::intel_wifi::present() {
            // Status, and requests ("scan", "connect\nSSID\nPASSWORD",
            // "disconnect") for the `wifi` command.
            entries.push((
                "wifi".into(),
                control_by(
                    crate::drivers::intel_wifi::status_text,
                    crate::drivers::intel_wifi::control,
                    Some(NETWORK_GROUP),
                ),
            ));
        }
        if crate::bluetooth::present() {
            // Status, devices found, and requests ("scan") for the
            // `bluetooth` command.
            let file = control_by(crate::bluetooth::status_text, crate::bluetooth::control, Some(NETWORK_GROUP));
            entries.push(("bluetooth".into(), file));
        }
        for info in process::list() {
            entries.push((info.pid.to_string(), process_directory(info.pid)));
        }
        entries
    })
}

/// /proc/sys: the kernel settings programs read (read-only here).
fn sys_directory() -> Arc<dyn Inode> {
    fn value(text: &'static str) -> Arc<dyn Inode> {
        generated(move || format!("{text}\n"))
    }
    directory(|| {
        alloc::vec![
            (
                "kernel".into(),
                directory(|| {
                    alloc::vec![
                        ("hostname".into(), generated(|| format!("{}\n", crate::syscall::HOSTNAME.lock()))),
                        ("ostype".into(), value("Linux")),
                        ("osrelease".into(), value(crate::syscall::KERNEL_RELEASE)),
                        ("overflowuid".into(), value("65534")),
                        ("overflowgid".into(), value("65534")),
                        ("pid_max".into(), value("4194304")),
                        ("ngroups_max".into(), value("65536")),
                        (
                            "random".into(),
                            directory(|| {
                                alloc::vec![
                                    ("uuid".into(), generated(random_uuid)),
                                    ("boot_id".into(), generated(boot_id)),
                                ]
                            })
                        ),
                        ("yama".into(), directory(|| alloc::vec![("ptrace_scope".into(), value("0"))])),
                    ]
                }),
            ),
            (
                "vm".into(),
                directory(|| {
                    alloc::vec![
                        ("overcommit_memory".into(), value("0")),
                        ("max_map_count".into(), value("65530")),
                        ("mmap_min_addr".into(), value("65536")),
                    ]
                }),
            ),
            ("fs".into(), directory(|| alloc::vec![("file-max".into(), value("1048576"))])),
            // No user namespaces: sandboxes that look here know not to try.
            ("user".into(), directory(|| alloc::vec![("max_user_namespaces".into(), value("0"))])),
        ]
    })
}

/// A random UUID (version 4), as /proc/sys/kernel/random/uuid gives.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    crate::random::fill(&mut bytes);
    bytes[6] = bytes[6] & 0x0F | 0x40;
    bytes[8] = bytes[8] & 0x3F | 0x80;
    uuid_text(&bytes)
}

fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}\n", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// The same UUID for the whole boot.
fn boot_id() -> String {
    static BOOT_ID: crate::sync::Once<String> = crate::sync::Once::new();
    if let Some(id) = BOOT_ID.get() {
        return id.clone();
    }
    let _ = BOOT_ID.set(random_uuid());
    BOOT_ID.get().cloned().unwrap_or_default()
}

/// /proc/net, in Linux's formats, which `netstat`, `route` and `arp` read.
fn net_directory() -> Arc<dyn Inode> {
    directory(|| {
        alloc::vec![
            ("dev".into(), generated(net_devices)),
            ("route".into(), generated(net_routes)),
            ("arp".into(), generated(net_neighbors)),
            ("tcp".into(), generated(|| net_sockets(net::PROTOCOL_TCP))),
            ("udp".into(), generated(|| net_sockets(net::PROTOCOL_UDP))),
            // No IPv6, raw sockets in this list, or Unix sockets: just the
            // headers, for tools that read these too.
            ("tcp6".into(), generated(|| String::from(SOCKET_HEADER))),
            ("udp6".into(), generated(|| String::from(SOCKET_HEADER))),
            ("raw".into(), generated(|| String::from(SOCKET_HEADER))),
            ("raw6".into(), generated(|| String::from(SOCKET_HEADER))),
            ("unix".into(), generated(|| String::from("Num       RefCount Protocol Flags    Type St Inode Path\n"))),
        ]
    })
}

fn net_devices() -> String {
    let mut text = String::from(
        "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
    );
    for interface in net::interfaces() {
        let c = interface.counters;
        text.push_str(&format!(
            "{:>6}: {:>8} {:>7}    0    0    0     0          0         0 {:>8} {:>7}    0    0    0     0       0          0\n",
            interface.name, c.received_bytes, c.received_packets, c.sent_bytes, c.sent_packets
        ));
    }
    text
}

/// An address as /proc/net prints it: the 32-bit value in the machine's
/// (little-endian) byte order, in hex.
fn hex_address(address: net::Ipv4) -> String {
    format!("{:08X}", u32::from_le_bytes(address.0))
}

fn net_routes() -> String {
    let mut text =
        String::from("Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n");
    for interface in net::interfaces().iter().filter(|i| i.name != "lo") {
        let Some(config) = &interface.config else { continue };
        let any = net::Ipv4::ANY;
        if let Some(gateway) = config.gateway {
            // Flags: up (1) and gateway (2).
            text.push_str(&format!(
                "{}\t{}\t{}\t0003\t0\t0\t0\t{}\t0\t0\t0\n",
                interface.name,
                hex_address(any),
                hex_address(gateway),
                hex_address(any)
            ));
        }
        let network = net::Ipv4::from_u32(config.address.to_u32() & config.netmask().to_u32());
        text.push_str(&format!(
            "{}\t{}\t{}\t0001\t0\t0\t0\t{}\t0\t0\t0\n",
            interface.name,
            hex_address(network),
            hex_address(any),
            hex_address(config.netmask())
        ));
    }
    text
}

fn net_neighbors() -> String {
    let mut text = String::from("IP address       HW type     Flags       HW address            Mask     Device\n");
    let device = net::STACK.lock().interface_name();
    for (address, mac) in net::neighbors() {
        text.push_str(&format!(
            "{:<16} 0x1         0x2         {}     *        {device}\n",
            address.to_string(),
            net::format_mac(&mac)
        ));
    }
    text
}

const SOCKET_HEADER: &str =
    "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

fn net_sockets(protocol: u8) -> String {
    let mut text = String::from(SOCKET_HEADER);
    let sockets = net::socket::table();
    for (index, socket) in sockets.iter().filter(|s| s.protocol == protocol).enumerate() {
        // UDP sockets report "closed" (7), as on Linux.
        let state = socket.state.map_or(7, net::tcp_state_code);
        text.push_str(&format!(
            "{index:>4}: {}:{:04X} {}:{:04X} {state:02X} {:08X}:{:08X} 00:00000000 00000000     0        0 0\n",
            hex_address(socket.local.address),
            socket.local.port,
            hex_address(socket.remote.address),
            socket.remote.port,
            socket.queued.0,
            socket.queued.1,
        ));
    }
    text
}

/// What a thread is doing, as /proc/PID/syscall shows it: the system call
/// number and first arguments, or `running`.
fn current_syscall(process: &process::Process, tid: Option<Pid>) -> String {
    let threads = process.threads.lock();
    let thread = match tid {
        Some(tid) => threads.iter().find(|thread| thread.tid() == tid),
        None => threads.first(),
    };
    let Some(thread) = thread else { return String::from("running\n") };
    match thread.syscall.load(core::sync::atomic::Ordering::Relaxed) {
        0 => String::from("running\n"),
        number => {
            let a: Vec<u64> =
                thread.syscall_arguments.iter().map(|a| a.load(core::sync::atomic::Ordering::Relaxed)).collect();
            format!("{} {:#x} {:#x} {:#x}\n", number - 1, a[0], a[1], a[2])
        }
    }
}

/// eliOS's own /proc/PID/threads: each thread's id, name, state and the
/// system call it's in (for finding where a program is stuck).
fn threads(process: &process::Process) -> String {
    let names: Vec<task::ThreadInfo> = task::list();
    let mut text = String::new();
    for thread in process.threads.lock().iter() {
        let info = names.iter().find(|info| info.id == thread.id);
        let name = info.map_or("?", |info| info.name.as_str());
        let state = info.map_or('?', |info| match info.state {
            task::State::Running | task::State::Ready => 'R',
            _ => 'S',
        });
        let call = match thread.syscall.load(core::sync::atomic::Ordering::Relaxed) {
            0 => String::from("-"),
            number => {
                let a: Vec<u64> =
                    thread.syscall_arguments.iter().map(|a| a.load(core::sync::atomic::Ordering::Relaxed)).collect();
                format!("{}({:#x}, {:#x}, {:#x})", crate::syscall::call_name(number - 1), a[0], a[1], a[2])
            }
        };
        text.push_str(&format!("{:>6} {state} {name:<16} {call}\n", thread.tid()));
    }
    text
}

/// eliOS's own /proc/PID/stacks: for each thread in a system call, where
/// it was called from and the return addresses found on its stack (words
/// pointing into executable areas, as `file+offset`). A heuristic, since
/// programs rarely keep frame pointers, but enough to see what a stuck
/// program is waiting for.
fn stacks(process: &process::Process) -> String {
    use core::sync::atomic::Ordering::Relaxed;
    const SCAN_BYTES: u64 = 64 * 1024;
    const MOST_FRAMES: usize = 48;
    let space = process.space.lock();
    let Some(space) = space.as_ref() else { return String::new() };
    let place = |address: u64| match space.describe(address) {
        Some((name, offset)) => format!("{address:#x} {name}+{offset:#x}"),
        None => format!("{address:#x}"),
    };
    let mut text = String::new();
    for thread in process.threads.lock().iter() {
        let number = thread.syscall.load(Relaxed);
        if number == 0 {
            text.push_str(&format!("thread {}: running\n\n", thread.tid()));
            continue;
        }
        let stack = thread.syscall_stack.load(Relaxed);
        text.push_str(&format!(
            "thread {}: {}\n  {}\n",
            thread.tid(),
            crate::syscall::call_name(number - 1),
            place(thread.syscall_rip.load(Relaxed))
        ));
        let mut found = 0;
        for address in (stack & !7..stack.saturating_add(SCAN_BYTES)).step_by(8) {
            let Some(word) = space.peek(address) else { break };
            if space.executable(word) {
                text.push_str(&format!("  {}\n", place(word)));
                found += 1;
                if found == MOST_FRAMES {
                    break;
                }
            }
        }
        text.push('\n');
    }
    text
}

/// eliOS's own /proc/PID/profile: write `1` to start sampling where the
/// process is at every timer tick (1000 a second per CPU), `0` to stop;
/// reading gives the most frequent places as `count library+offset`, with
/// `[kernel: call]` for time in system calls and page faults.
fn profile(process: &process::Process) -> String {
    const MOST_PLACES: usize = 300;
    let Some(samples) = process.profile.lock().clone() else {
        return String::from("not profiling: write 1 to start\n");
    };
    let total: u32 = samples.values().sum();
    let mut sorted: Vec<(u64, u32)> = samples.into_iter().collect();
    sorted.sort_by_key(|&(_, count)| core::cmp::Reverse(count));
    let space = process.space.lock();
    let mut text = format!("{total} samples\n");
    for &(address, count) in sorted.iter().take(MOST_PLACES) {
        let place = match address.checked_sub(process::KERNEL_SAMPLE) {
            Some(0) => String::from("[kernel: faults and interrupts]"),
            Some(call) => format!("[kernel: {}]", crate::syscall::call_name(call - 1)),
            None => match space.as_ref().and_then(|space| space.describe(address)) {
                Some((name, offset)) => format!("{name}+{offset:#x}"),
                None => format!("{address:#x}"),
            },
        };
        text.push_str(&format!("{count:>7} {place}\n"));
    }
    drop(space);
    // Where in the kernel, for `addr2line -f -e target/.../elikernel`.
    if let Some(kernel) = process.kernel_profile.lock().clone() {
        let mut sorted: Vec<(u64, u32)> = kernel.into_iter().collect();
        sorted.sort_by_key(|&(_, count)| core::cmp::Reverse(count));
        text.push_str("in the kernel:\n");
        for (address, count) in sorted.into_iter().take(MOST_PLACES) {
            text.push_str(&format!("{count:>7} kernel {address:#x}\n"));
        }
    }
    text
}

fn process_directory(pid: Pid) -> Arc<dyn Inode> {
    let owner = process::find(pid).map_or((0, 0), |process| {
        let credentials = process.credentials.lock();
        (credentials.euid, credentials.egid)
    });
    let list = Box::new(move || {
        let Some(process) = process::find(pid) else { return Vec::new() };
        let mut entries: Listing = Vec::new();
        let target = process.clone();
        entries.push(("exe".into(), link(move || target.executable.lock().clone())));
        let target = process.clone();
        entries.push(("cwd".into(), link(move || target.cwd.lock().clone())));
        let target = process.clone();
        entries.push((
            "cmdline".into(),
            generated(move || target.arguments.lock().iter().map(|a| format!("{a}\0")).collect()),
        ));
        entries.push(("status".into(), generated(move || status(pid))));
        entries.push((
            "comm".into(),
            generated(move || info(pid).map(|info| format!("{}\n", info.name)).unwrap_or_default()),
        ));
        entries.push(("stat".into(), generated(move || stat(pid))));
        let target = process.clone();
        entries.push(("syscall".into(), generated(move || current_syscall(&target, None))));
        let target = process.clone();
        entries.push(("threads".into(), generated(move || threads(&target))));
        let target = process.clone();
        entries.push(("maps".into(), generated(move || maps(&target))));
        let target = process.clone();
        entries.push(("stacks".into(), generated(move || stacks(&target))));
        let (target, controlled) = (process.clone(), process.clone());
        entries.push((
            "profile".into(),
            control(
                move || profile(&target),
                move |bytes| {
                    let on = match bytes.first() {
                        Some(b'1') => true,
                        Some(b'0') => false,
                        _ => return Err(Errno::EINVAL),
                    };
                    process::set_profiling(&controlled, on);
                    Ok(())
                },
            ),
        ));
        let target = process;
        entries.push((
            "fd".into(),
            directory(move || {
                let mut descriptors: Listing = Vec::new();
                let files = target.files.lock();
                for fd in 0..super::MAX_FILES {
                    if let Ok(file) = files.get(fd) {
                        let path = file.path.clone();
                        descriptors.push((fd.to_string(), link(move || path.clone())));
                    }
                }
                descriptors
            }),
        ));
        entries
    });
    Arc::new(Directory { inode: ramfs::next_inode_number(), list, owner })
}

fn info(pid: Pid) -> Option<process::ProcessInfo> {
    process::list().into_iter().find(|info| info.pid == pid)
}

fn status(pid: Pid) -> String {
    let Some(info) = info(pid) else { return String::new() };
    let state = match info.state {
        'R' => "R (running)",
        'Z' => "Z (zombie)",
        _ => "S (sleeping)",
    };
    let ids = &info.credentials;
    let groups: Vec<String> = ids.groups.iter().map(ToString::to_string).collect();
    let process = process::find(pid);
    format!(
        "Name:\t{}\nState:\t{state}\nTgid:\t{pid}\nPid:\t{pid}\nPPid:\t{}\n\
         Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nGroups:\t{}\nThreads:\t{}\nVmRSS:\t{} kB\nNoNewPrivs:\t{}\nSeccomp:\t{}\n",
        info.name,
        info.parent,
        ids.uid,
        ids.euid,
        ids.suid,
        ids.euid,
        ids.gid,
        ids.egid,
        ids.sgid,
        ids.egid,
        groups.join(" "),
        info.threads,
        info.memory / 1024,
        u8::from(process.as_ref().is_some_and(|p| p.no_new_privileges.load(core::sync::atomic::Ordering::Relaxed))),
        process
            .as_ref()
            .and_then(|p| p.threads.lock().first().map(|thread| thread.seccomp.lock().number()))
            .unwrap_or(0),
    )
}

fn stat(pid: Pid) -> String {
    let Some(info) = info(pid) else { return String::new() };
    let group = process::find(pid).map_or(pid, |p| p.group());
    // Times are in clock ticks of 1/100 s. All 52 of Linux's fields are
    // there, mostly 0: BusyBox's `ps`, `pidof` and `killall` read them
    // without checking for the end of the line. After `rss` come the
    // limit (none), code, stack and signal addresses, the exit signal
    // (SIGCHLD), and the CPU, scheduling, data and argument fields.
    format!(
        "{pid} ({}) {} {} {group} {group} 0 -1 0 0 0 0 0 {} 0 0 0 20 0 1 0 0 {} {} 18446744073709551615 \
         0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        info.name,
        info.state,
        info.parent,
        info.cpu_ms / 10,
        info.memory,
        info.memory / 4096
    )
}

fn maps(process: &process::Process) -> String {
    let space = process.space.lock();
    let Some(space) = space.as_ref() else { return String::new() };
    let mut text = String::new();
    for area in space.areas() {
        let flag = |bit, c| if area.protection & bit != 0 { c } else { '-' };
        let line = format!(
            "{:012x}-{:012x} {}{}{}{} {:08x} 00:00 0",
            area.start,
            area.end,
            flag(process::PROT_READ, 'r'),
            flag(process::PROT_WRITE, 'w'),
            flag(process::PROT_EXEC, 'x'),
            if area.shared { 's' } else { 'p' },
            area.offset,
        );
        match &area.name {
            // Linux pads the name to a column.
            Some(name) => text.push_str(&format!("{line:<73}{name}\n")),
            None => text.push_str(&format!("{line}\n")),
        }
    }
    text
}

/// CPU time totals, overall and per CPU, in 1/100 s.
fn system_stat() -> String {
    let times = crate::task::cpu_times();
    let line = |name: &str, (busy, idle): (u64, u64)| format!("{name} {} 0 0 {} 0 0 0 0 0 0\n", busy / 10, idle / 10);
    let total = times.iter().fold((0, 0), |(busy, idle), &(b, i)| (busy + b, idle + i));
    let mut text = line("cpu ", total);
    for (index, &cpu) in times.iter().enumerate() {
        text.push_str(&line(&format!("cpu{index}"), cpu));
    }
    text.push_str(&format!(
        "btime {}\nprocesses {}\nprocs_running 1\nprocs_blocked 0\n",
        time::unix_time() - time::uptime_nanos() / 1_000_000_000,
        process::count()
    ));
    text
}

/// The file system types `mount` accepts. Those marked `nodev` need no
/// device; `mount` tries the others in order when no type is given.
fn filesystems() -> String {
    ["nodev\tsysfs", "nodev\ttmpfs", "nodev\tramfs", "nodev\tproc", "nodev\tdevtmpfs", "\text2", "\tvfat"]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect()
}

fn mounts() -> String {
    super::mount::table()
}

/// /proc/partitions: every disk and partition, with its size in KiB.
fn partitions() -> String {
    use crate::drivers::block::{self, Partition};
    let mut text = String::from("major minor  #blocks  name\n\n");
    for disk in block::disks() {
        let whole = Partition::whole(&disk);
        for partition in core::iter::once(whole).chain(block::partitions(&disk)) {
            let (major, minor) = block::device_numbers(&partition);
            text.push_str(&format!("{major:>4} {minor:>7} {:>10} {}\n", partition.sectors / 2, partition.name()));
        }
    }
    text
}

fn meminfo() -> String {
    let memory = frames::stats();
    let kib = |bytes: u64| bytes / 1024;
    format!(
        "MemTotal:       {:>8} kB\nMemFree:        {:>8} kB\nMemAvailable:   {:>8} kB\nBuffers:               0 kB\nCached:                0 kB\nSlab:           {:>8} kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n",
        kib(memory.managed_bytes),
        kib(memory.free_bytes),
        kib(memory.free_bytes),
        kib(heap::stats().bytes_mapped)
    )
}

fn uptime() -> String {
    let uptime = time::uptime_nanos();
    format!("{}.{:02} 0.00\n", uptime / 1_000_000_000, uptime % 1_000_000_000 / 10_000_000)
}

fn version() -> String {
    format!("eliOS version {} (Linux-compatible system call ABI)\n", env!("CARGO_PKG_VERSION"))
}

fn cpuinfo() -> String {
    let vendor = {
        let [_, ebx, ecx, edx] = crate::arch::cpuid(0, 0);
        let mut bytes = Vec::new();
        for register in [ebx, edx, ecx] {
            bytes.extend_from_slice(&register.to_le_bytes());
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut model = Vec::new();
    for leaf in 0x8000_0002..=0x8000_0004 {
        for register in crate::arch::cpuid(leaf, 0) {
            model.extend_from_slice(&register.to_le_bytes());
        }
    }
    let model = String::from_utf8_lossy(&model).trim_matches(char::from(0)).trim().to_string();
    let mhz = time::tsc_hz() / 1_000_000;
    let count = crate::arch::cpu::online_count();
    let mut text = String::new();
    for cpu in crate::arch::cpu::online() {
        text.push_str(&format!(
            "processor\t: {index}\nvendor_id\t: {vendor}\nmodel name\t: {model}\ncpu MHz\t\t: {mhz}.000\nphysical id\t: 0\nsiblings\t: {count}\ncore id\t\t: {index}\ncpu cores\t: {count}\napicid\t\t: {apic}\nflags\t\t: fpu sse sse2 syscall nx lm\n\n",
            index = cpu.index(),
            apic = cpu.apic_id.load(core::sync::atomic::Ordering::Relaxed),
        ));
    }
    text
}
