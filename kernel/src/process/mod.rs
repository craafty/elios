//! User processes: creation (`fork`, `exec`), termination, waiting, and
//! signals.
//!
//! A process is an address space, open files, a working directory and one
//! thread. Signals are minimal for now: each is either ignored or ends the
//! process (handlers come later), which covers Ctrl+C and `kill`.

mod address_space;
pub mod elf;
pub mod user;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub use self::address_space::{AddressSpace, PROT_EXEC, PROT_READ, PROT_WRITE, USER_END};
use crate::arch::fpu::FpuState;
use crate::arch::interrupts::{self, InterruptFrame};
use crate::arch::syscall;
use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, FileTable, OpenFile};
use crate::memory;
use crate::sync::SpinLock;
use crate::task::{self, Spawn, ThreadId, WaitQueue};

pub type Pid = u32;

/// The first process; orphans are adopted by it.
pub const INIT: Pid = 1;

// Signal numbers (Linux values).
pub const SIGINT: u32 = 2;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;
pub const SIGPIPE: u32 = 13;
#[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGURG: u32 = 23;
pub const SIGWINCH: u32 = 28;
pub const SIGNAL_COUNT: usize = 65;

/// Signal disposition values (`sa_handler`).
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub struct Process {
    pub pid: Pid,
    parent: AtomicU32,
    group: AtomicU32,
    pub name: SpinLock<String>,
    pub space: SpinLock<Option<AddressSpace>>,
    pub files: SpinLock<FileTable>,
    pub cwd: SpinLock<String>,
    /// Set (as a wait status) once the process has exited.
    status: SpinLock<Option<i32>>,
    /// Signals sent but not yet acted on, one bit each.
    pending: AtomicU64,
    /// How each signal is handled (`SIG_DFL`, `SIG_IGN` or a handler).
    pub actions: SpinLock<[u64; SIGNAL_COUNT]>,
    thread: SpinLock<Option<ThreadId>>,
}

impl Process {
    pub fn parent(&self) -> Pid {
        self.parent.load(Ordering::Relaxed)
    }

    pub fn group(&self) -> Pid {
        self.group.load(Ordering::Relaxed)
    }

    pub fn set_group(&self, group: Pid) {
        self.group.store(group, Ordering::Relaxed);
    }
}

static PROCESSES: SpinLock<BTreeMap<Pid, Arc<Process>>> = SpinLock::new(BTreeMap::new());
static NEXT_PID: AtomicU32 = AtomicU32::new(INIT);
/// Woken when any process exits, for `wait`.
static CHILD_EVENTS: WaitQueue = WaitQueue::new();
/// Whether process 1 is init (it isn't when tests start processes).
static INIT_RUNNING: AtomicBool = AtomicBool::new(false);

fn create(parent: Pid, group: Option<Pid>, name: &str, cwd: String, files: FileTable) -> Arc<Process> {
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let process = Arc::new(Process {
        pid,
        parent: AtomicU32::new(parent),
        group: AtomicU32::new(group.unwrap_or(pid)),
        name: SpinLock::new(name.to_string()),
        space: SpinLock::new(None),
        files: SpinLock::new(files),
        cwd: SpinLock::new(cwd),
        status: SpinLock::new(None),
        pending: AtomicU64::new(0),
        actions: SpinLock::new([SIG_DFL; SIGNAL_COUNT]),
        thread: SpinLock::new(None),
    });
    PROCESSES.lock().insert(pid, process.clone());
    process
}

/// The process the running thread belongs to.
pub fn current() -> Arc<Process> {
    task::current_process().expect("not running in a user process")
}

pub fn find(pid: Pid) -> Option<Arc<Process>> {
    PROCESSES.lock().get(&pid).cloned()
}

/// Starts a thread that enters user mode with `frame`.
fn start_thread(process: &Arc<Process>, frame: InterruptFrame, fpu: FpuState, fs_base: u64) {
    let address_space = process.space.lock().as_ref().expect("process without memory").root();
    let options =
        Spawn { name: process.name.lock().clone(), process: Some(process.clone()), address_space, fpu, fs_base };
    let thread = task::spawn_with(options, move || {
        // SAFETY: the frame was built for this process's address space,
        // which the scheduler has made active.
        unsafe { syscall::enter_user(&frame) }
    });
    *process.thread.lock() = Some(thread);
}

/// What it takes to run a program file.
struct Image {
    space: AddressSpace,
    entry: u64,
    stack_pointer: u64,
    /// The arguments actually used (scripts gain their interpreter).
    path: String,
}

/// Loads the program at `path` (resolved from `cwd`) into a new address
/// space. Scripts starting with `#!` run their interpreter instead.
fn load_image(cwd: &str, path: &str, arguments: Vec<String>, environment: &[String]) -> Result<Image> {
    let inode = fs::lookup(cwd, path, true)?;
    if inode.metadata().kind != FileKind::Regular {
        return Err(Errno::EACCES);
    }
    let size = inode.metadata().size as usize;
    let mut data = vec![0; size];
    inode.read_at(0, &mut data)?;

    if let Some(script) = data.strip_prefix(b"#!") {
        let line = script.split(|&b| b == b'\n').next().unwrap_or_default();
        let line = core::str::from_utf8(line).map_err(|_| Errno::ENOEXEC)?.trim();
        let mut parts = line.splitn(2, char::is_whitespace);
        let interpreter = parts.next().filter(|i| !i.is_empty()).ok_or(Errno::ENOEXEC)?;
        let mut script_arguments = vec![interpreter.to_string()];
        if let Some(argument) = parts.next().map(str::trim).filter(|a| !a.is_empty()) {
            script_arguments.push(argument.to_string());
        }
        script_arguments.push(path.to_string());
        script_arguments.extend(arguments.into_iter().skip(1));
        if interpreter == path {
            return Err(Errno::ELOOP);
        }
        return load_image(cwd, interpreter, script_arguments, environment);
    }

    let mut space = AddressSpace::new()?;
    let program = elf::load(&mut space, &data)?;
    let stack_pointer = elf::build_stack(&mut space, &program, &arguments, environment, path)?;
    Ok(Image { space, entry: program.entry, stack_pointer, path: path.to_string() })
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Starts the first user program, with the console as its standard input,
/// output and error.
#[cfg_attr(test, allow(dead_code, reason = "tests start their own programs"))]
pub fn start_init(path: &str) -> Result<Pid> {
    let pid = start(path, vec![path.to_string()])?;
    INIT_RUNNING.store(pid == INIT, Ordering::Relaxed);
    Ok(pid)
}

/// Starts a program on the console, as a child of no one (the kernel).
pub fn start(path: &str, arguments: Vec<String>) -> Result<Pid> {
    let environment = default_environment();
    let image = load_image("/", path, arguments, &environment)?;
    let console = fs::lookup("/", "/dev/console", true)?;
    let mut files = FileTable::default();
    let terminal = OpenFile::new(console, "/dev/console".into(), fs::O_RDWR);
    for _ in 0..3 {
        files.insert(terminal.clone(), false, 0)?;
    }
    let process = create(0, None, base_name(path), "/".into(), files);
    *process.space.lock() = Some(image.space);
    start_thread(&process, syscall::user_frame(image.entry, image.stack_pointer), FpuState::initial(), 0);
    Ok(process.pid)
}

fn default_environment() -> Vec<String> {
    ["PATH=/bin", "HOME=/home", "TERM=linux", "SHELL=/bin/sh", "USER=root"].map(String::from).to_vec()
}

/// `fork`: a copy of the running process, continuing from the same system
/// call but seeing 0 as its result.
pub fn fork(frame: &InterruptFrame) -> Result<Pid> {
    let parent = current();
    let space = parent.space.lock().as_ref().ok_or(Errno::EFAULT)?.duplicate()?;
    let name = parent.name.lock().clone();
    let cwd = parent.cwd.lock().clone();
    let files = parent.files.lock().clone();
    let child = create(parent.pid, Some(parent.group()), &name, cwd, files);
    *child.actions.lock() = *parent.actions.lock();
    *child.space.lock() = Some(space);

    let mut child_frame = *frame;
    child_frame.rax = 0;
    let mut fpu = FpuState::initial();
    task::save_fpu(&mut fpu);
    start_thread(&child, child_frame, fpu, syscall::fs_base());
    Ok(child.pid)
}

/// `execve`: replaces the running program. On success the system call
/// "returns" into the new program, so `frame` is rewritten to start it.
pub fn exec(frame: &mut InterruptFrame, path: &str, arguments: Vec<String>, environment: Vec<String>) -> Result<()> {
    let process = current();
    let cwd = process.cwd.lock().clone();
    let image = load_image(&cwd, path, arguments, &environment)?;

    // From here on, nothing can fail: the old program is gone.
    task::set_address_space(image.space.root());
    let old = process.space.lock().replace(image.space);
    drop(old);
    process.files.lock().exec();
    // Handlers point into the old program; ignored signals stay ignored.
    for action in process.actions.lock().iter_mut() {
        if *action != SIG_IGN {
            *action = SIG_DFL;
        }
    }
    let name = base_name(&image.path).to_string();
    task::set_name(&name);
    *process.name.lock() = name;
    task::reset_fpu();
    syscall::set_fs_base(0);
    *frame = syscall::user_frame(image.entry, image.stack_pointer);
    Ok(())
}

/// Ends the running process with a wait status (see [`exit_status`] and
/// [`signal_status`]).
pub fn exit(status: i32) -> ! {
    let process = current();
    process.files.lock().close_all();
    task::set_address_space(memory::kernel_page_table_root());
    let space = process.space.lock().take();
    drop(space);

    for other in PROCESSES.lock().values() {
        if other.parent() == process.pid {
            other.parent.store(INIT, Ordering::Relaxed);
        }
    }
    *process.status.lock() = Some(status);
    if process.pid == INIT && INIT_RUNNING.load(Ordering::Relaxed) {
        panic!("init exited (wait status {status:#x})");
    }
    CHILD_EVENTS.wake_all();
    task::exit()
}

/// The wait status of a process that called `exit(code)`.
pub fn exit_status(code: i32) -> i32 {
    (code & 0xFF) << 8
}

/// The wait status of a process ended by a signal.
pub fn signal_status(signal: u32) -> i32 {
    (signal & 0x7F) as i32
}

/// `wait4`: waits for a child to exit and collects it. `pid` selects which
/// children qualify, as in Linux: a pid, -1 for any, 0 for the caller's
/// group, -group for that group. With `no_hang`, returns `None` instead of
/// waiting.
pub fn wait(pid: i64, no_hang: bool) -> Result<Option<(Pid, i32)>> {
    let me = current();
    CHILD_EVENTS.wait_until(|| {
        let mut processes = PROCESSES.lock();
        let qualifies = |process: &Process| {
            process.parent() == me.pid
                && match pid {
                    -1 => true,
                    0 => process.group() == me.group(),
                    pid if pid > 0 => process.pid == pid as Pid,
                    group => process.group() == (-group) as Pid,
                }
        };
        let children: Vec<Arc<Process>> = processes.values().filter(|p| qualifies(p)).cloned().collect();
        if children.is_empty() {
            return Some(Err(Errno::ECHILD));
        }
        if let Some(child) = children.iter().find(|child| child.status.lock().is_some()) {
            let status = child.status.lock().unwrap();
            processes.remove(&child.pid);
            return Some(Ok(Some((child.pid, status))));
        }
        drop(processes);
        if no_hang {
            return Some(Ok(None));
        }
        signal_pending().then_some(Err(Errno::EINTR))
    })
}

/// Waits for a process the kernel started to exit, collects it, and returns
/// its wait status.
#[cfg(test)]
pub fn wait_for_exit(pid: Pid) -> i32 {
    CHILD_EVENTS.wait_until(|| {
        let mut processes = PROCESSES.lock();
        let status = (*processes.get(&pid)?.status.lock())?;
        processes.remove(&pid);
        Some(status)
    })
}

/// Marks a signal pending and wakes the process so it notices.
pub fn send_signal(process: &Process, signal: u32) {
    if signal == 0 || signal as usize >= SIGNAL_COUNT || process.status.lock().is_some() {
        return;
    }
    process.pending.fetch_or(signal_bit(signal), Ordering::Relaxed);
    if let Some(thread) = *process.thread.lock() {
        task::interrupt(thread);
    }
}

pub fn send_signal_to_self(signal: u32) {
    if let Some(process) = task::current_process() {
        send_signal(&process, signal);
    }
}

/// Sends `signal` to every process in a group.
pub fn signal_group(group: Pid, signal: u32) {
    let targets: Vec<Arc<Process>> = PROCESSES.lock().values().filter(|p| p.group() == group).cloned().collect();
    for process in targets {
        send_signal(&process, signal);
    }
}

/// `kill`: sends a signal to a process (pid > 0), the caller's group (0),
/// every process but init (-1), or a group (-group).
pub fn kill(pid: i64, signal: u32) -> Result<()> {
    if signal as usize >= SIGNAL_COUNT {
        return Err(Errno::EINVAL);
    }
    let me = current();
    let targets: Vec<Arc<Process>> = PROCESSES
        .lock()
        .values()
        .filter(|p| p.status.lock().is_none())
        .filter(|p| match pid {
            0 => p.group() == me.group(),
            -1 => p.pid != INIT && p.pid != me.pid,
            pid if pid > 0 => p.pid == pid as Pid,
            group => p.group() == (-group) as Pid,
        })
        .cloned()
        .collect();
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    for process in targets {
        send_signal(&process, signal);
    }
    Ok(())
}

/// A signal's bit in a signal set: as in Linux, signal n is bit n - 1.
fn signal_bit(signal: u32) -> u64 {
    1 << (signal - 1)
}

/// Signals whose default action is to do nothing.
fn ignored_by_default(signal: u32) -> bool {
    // Stopping isn't supported, so the stop signals are ignored too.
    matches!(signal, SIGCHLD | SIGCONT | SIGURG | SIGWINCH | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

/// The pending signals that will actually do something.
fn effective_signals(process: &Process) -> u64 {
    let pending = process.pending.load(Ordering::Relaxed);
    if pending == 0 {
        return 0;
    }
    let actions = process.actions.lock();
    (1..SIGNAL_COUNT as u32)
        .filter(|&signal| pending & signal_bit(signal) != 0)
        .filter(|&signal| {
            signal == SIGKILL
                || match actions[signal as usize] {
                    SIG_IGN => false,
                    SIG_DFL => !ignored_by_default(signal),
                    _ => true,
                }
        })
        .fold(0, |mask, signal| mask | signal_bit(signal))
}

/// Whether the running process has a signal that should interrupt a
/// blocking system call.
pub fn signal_pending() -> bool {
    task::current_process().is_some_and(|process| effective_signals(&process) != 0)
}

/// Called with interrupts disabled whenever the kernel is about to return to
/// user mode: acts on pending signals and switches threads if one is due.
pub fn before_return_to_user() {
    if let Some(process) = task::current_process() {
        let effective = effective_signals(&process);
        // Signals that do nothing are simply dropped.
        process.pending.fetch_and(effective, Ordering::Relaxed);
        if effective != 0 {
            let signal = effective.trailing_zeros() + 1;
            crate::arch::enable_interrupts();
            exit(signal_status(signal));
        }
    }
    task::preempt_point();
}

/// Handles a CPU exception raised by user code: page faults inside the
/// program's areas are resolved; anything else ends the process with the
/// matching signal, as Linux would.
pub fn handle_user_exception(frame: &mut InterruptFrame) {
    let process = current();
    let vector = frame.vector as u8;
    if vector == interrupts::PAGE_FAULT {
        let address = crate::arch::read_cr2();
        let write = frame.error_code & 2 != 0;
        let execute = frame.error_code & 0x10 != 0;
        let resolved = process.space.lock().as_mut().is_some_and(|space| space.handle_fault(address, write, execute));
        if resolved {
            return;
        }
        warn!(
            "{} (pid {}): segmentation fault: {} {address:#x} at {:#x}",
            process.name.lock(),
            process.pid,
            if execute {
                "executing"
            } else if write {
                "writing"
            } else {
                "reading"
            },
            frame.rip,
        );
        crate::arch::enable_interrupts();
        exit(signal_status(SIGSEGV));
    }

    let (signal, description) = match vector {
        0 => (SIGFPE, "division by zero"),
        3 => (SIGTRAP, "breakpoint"),
        6 => (SIGILL, "invalid instruction"),
        13 => (SIGSEGV, "general protection fault"),
        16 | 19 => (SIGFPE, "floating point exception"),
        17 => (SIGBUS, "misaligned access"),
        _ => (SIGSEGV, "CPU exception"),
    };
    warn!("{} (pid {}): {description} at {:#x}", process.name.lock(), process.pid, frame.rip);
    crate::arch::enable_interrupts();
    exit(signal_status(signal));
}

/// A snapshot of a process, for `ps`.
pub struct ProcessInfo {
    pub pid: Pid,
    pub parent: Pid,
    pub name: String,
    pub state: char,
    pub cpu_ms: u64,
    pub memory: u64,
}

pub fn list() -> Vec<ProcessInfo> {
    let threads = task::list();
    let processes: Vec<Arc<Process>> = PROCESSES.lock().values().cloned().collect();
    processes
        .iter()
        .map(|process| {
            let thread_id = *process.thread.lock();
            let thread = threads.iter().find(|t| Some(t.id) == thread_id);
            let state = if process.status.lock().is_some() {
                'Z'
            } else {
                match thread.map(|t| t.state) {
                    Some(task::State::Running) => 'R',
                    Some(task::State::Ready) => 'R',
                    _ => 'S',
                }
            };
            ProcessInfo {
                pid: process.pid,
                parent: process.parent(),
                name: process.name.lock().clone(),
                state,
                cpu_ms: thread.map_or(0, |t| t.cpu_ticks),
                memory: process.space.lock().as_ref().map_or(0, AddressSpace::resident_bytes),
            }
        })
        .collect()
}

pub fn count() -> usize {
    PROCESSES.lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a shell command line as a user process and returns its wait
    /// status.
    fn shell(command: &str) -> i32 {
        let pid = start("/bin/sh", vec!["sh".into(), "-c".into(), command.into()]).expect("can't start the shell");
        wait_for_exit(pid)
    }

    fn read_file(path: &str) -> String {
        let inode = fs::lookup("/", path, true).unwrap();
        let mut data = vec![0; inode.metadata().size as usize];
        inode.read_at(0, &mut data).unwrap();
        String::from_utf8(data).unwrap()
    }

    crate::ktest! {
        fn programs_report_their_exit_codes() {
            assert_eq!(shell("true"), exit_status(0));
            assert_eq!(shell("false"), exit_status(1));
            assert_eq!(shell("exit 42"), exit_status(42));
        }

        fn signals_end_processes() {
            assert_eq!(shell("kill -SEGV $$"), signal_status(SIGSEGV));
            assert_eq!(shell("kill -TERM $$"), signal_status(SIGTERM));
        }

        fn missing_programs_are_reported() {
            assert_eq!(shell("no-such-program"), exit_status(127));
            assert_eq!(start("/no/such/file", vec![]).err(), Some(Errno::ENOENT));
        }

        fn pipelines_connect_processes() {
            // Three processes, two pipes, and a redirection.
            assert_eq!(shell("seq 50 | grep 5 | wc -l > /tmp/pipeline-test"), exit_status(0));
            assert_eq!(read_file("/tmp/pipeline-test").trim(), "6");
        }

        fn a_finished_program_gives_its_memory_back() {
            // The first run may grow kernel structures that stay.
            shell("true");
            let before = memory::frames::stats().free_bytes;
            assert_eq!(shell("seq 1000 > /dev/null"), exit_status(0));
            // Reaping the thread (and its kernel stack) happens at a switch.
            task::yield_now();
            let after = memory::frames::stats().free_bytes;
            assert!(after + 16 * memory::PAGE_SIZE >= before, "{} KiB not returned", (before - after) / 1024);
        }
    }
}
