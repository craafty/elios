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
use crate::process::{self, Pid};
use crate::sync::SpinLock;
use crate::{task, time};

type Generator = Box<dyn Fn() -> String + Send + Sync>;
/// A directory's entries.
type Listing = Vec<(String, Arc<dyn Inode>)>;

/// A file whose contents are produced on demand. Every lookup makes a new
/// one, so each open file gets its own snapshot.
struct Generated {
    inode: u64,
    generate: Generator,
    snapshot: SpinLock<Option<String>>,
}

fn generated(generate: impl Fn() -> String + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Generated {
        inode: ramfs::next_inode_number(),
        generate: Box::new(generate),
        snapshot: SpinLock::new(None),
    })
}

impl Inode for Generated {
    fn metadata(&self) -> Metadata {
        // Like Linux, report size 0: the contents aren't known until read.
        Metadata {
            kind: FileKind::Regular,
            inode: self.inode,
            size: 0,
            permissions: 0o444,
            modified: time::unix_time(),
        }
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

fn link(target: impl Fn() -> String + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Link { inode: ramfs::next_inode_number(), target: Box::new(target) })
}

impl Inode for Link {
    fn metadata(&self) -> Metadata {
        Metadata {
            kind: FileKind::Symlink,
            inode: self.inode,
            size: 0,
            permissions: 0o777,
            modified: time::unix_time(),
        }
    }

    fn read_link(&self) -> Result<String> {
        Ok((self.target)())
    }
}

/// A directory whose entries are listed by a function.
struct Directory {
    inode: u64,
    list: Box<dyn Fn() -> Listing + Send + Sync>,
}

fn directory(list: impl Fn() -> Listing + Send + Sync + 'static) -> Arc<dyn Inode> {
    Arc::new(Directory { inode: ramfs::next_inode_number(), list: Box::new(list) })
}

impl Inode for Directory {
    fn metadata(&self) -> Metadata {
        Metadata {
            kind: FileKind::Directory,
            inode: self.inode,
            size: 0,
            permissions: 0o555,
            modified: time::unix_time(),
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
            ("cpuinfo".into(), generated(cpuinfo)),
            ("loadavg".into(), generated(|| format!("0.00 0.00 0.00 1/{} {}\n", process::count(), process::count()))),
            ("filesystems".into(), generated(|| "nodev\tramfs\nnodev\tproc\n".into())),
            ("mounts".into(), generated(|| "ramfs / ramfs rw 0 0\nproc /proc proc rw 0 0\n".into())),
        ];
        for info in process::list() {
            entries.push((info.pid.to_string(), process_directory(info.pid)));
        }
        entries
    })
}

fn process_directory(pid: Pid) -> Arc<dyn Inode> {
    directory(move || {
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
    })
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
    format!(
        "Name:\t{}\nState:\t{state}\nTgid:\t{pid}\nPid:\t{pid}\nPPid:\t{}\nThreads:\t1\nVmRSS:\t{} kB\n",
        info.name,
        info.parent,
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
    for (start, end, protection) in space.areas() {
        let flag = |bit, c| if protection & bit != 0 { c } else { '-' };
        text.push_str(&format!(
            "{start:012x}-{end:012x} {}{}{}p 00000000 00:00 0\n",
            flag(process::PROT_READ, 'r'),
            flag(process::PROT_WRITE, 'w'),
            flag(process::PROT_EXEC, 'x')
        ));
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
    format!(
        "processor\t: 0\nvendor_id\t: {vendor}\nmodel name\t: {model}\ncpu MHz\t\t: {mhz}.000\ncpu cores\t: 1\nflags\t\t: fpu sse sse2 syscall nx lm\n\n"
    )
}
