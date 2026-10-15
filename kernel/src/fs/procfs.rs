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
        ];
        for info in process::list() {
            entries.push((info.pid.to_string(), process_directory(info.pid)));
        }
        entries
    })
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
    for (address, mac) in net::neighbors() {
        text.push_str(&format!(
            "{:<16} 0x1         0x2         {}     *        eth0\n",
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
        entries.push(("stat".into(), generated(move || stat(pid))));
        let target = process.clone();
        entries.push(("maps".into(), generated(move || maps(&target))));
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
    format!(
        "Name:\t{}\nState:\t{state}\nTgid:\t{pid}\nPid:\t{pid}\nPPid:\t{}\n\
         Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nGroups:\t{}\nThreads:\t{}\nVmRSS:\t{} kB\n",
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
        info.memory / 1024
    )
}

fn stat(pid: Pid) -> String {
    let Some(info) = info(pid) else { return String::new() };
    let group = process::find(pid).map_or(pid, |p| p.group());
    // Times are in clock ticks of 1/100 s.
    format!(
        "{pid} ({}) {} {} {group} {group} 0 -1 0 0 0 0 0 {} 0 0 0 20 0 1 0 0 {} {} 0\n",
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
    for (start, end, protection, shared) in space.areas() {
        let flag = |bit, c| if protection & bit != 0 { c } else { '-' };
        text.push_str(&format!(
            "{start:012x}-{end:012x} {}{}{}{} 00000000 00:00 0\n",
            flag(process::PROT_READ, 'r'),
            flag(process::PROT_WRITE, 'w'),
            flag(process::PROT_EXEC, 'x'),
            if shared { 's' } else { 'p' }
        ));
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
