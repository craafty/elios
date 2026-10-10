//! User processes: creation (`fork`, `exec`), termination, waiting, and
//! signals.
//!
//! A process is an address space, open files, a working directory and one
//! thread. Signals are minimal for now: each is either ignored or ends the
//! process (handlers come later), which covers Ctrl+C and `kill`.

mod address_space;
pub mod elf;
pub mod signal;
pub mod user;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub use self::address_space::{AddressSpace, PROT_EXEC, PROT_READ, PROT_WRITE, USER_END};
#[cfg(test)]
use self::signal::SIGTERM;
use self::signal::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
pub use self::signal::{SIGINT, SIGKILL, SIGNAL_COUNT, SIGPIPE, SigAction, Signals, pending as signal_pending};
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

pub struct Process {
    pub pid: Pid,
    parent: AtomicU32,
    group: AtomicU32,
    pub name: SpinLock<String>,
    /// The program file and the arguments it was started with.
    pub executable: SpinLock<String>,
    pub arguments: SpinLock<Vec<String>>,
    pub space: SpinLock<Option<AddressSpace>>,
    pub files: SpinLock<FileTable>,
    pub cwd: SpinLock<String>,
    /// Set (as a wait status) once the process has exited.
    status: SpinLock<Option<i32>>,
    pub signals: SpinLock<Signals>,
    thread: SpinLock<Option<ThreadId>>,
    /// Whether to print every system call (see `strace`). Inherited by
    /// children and kept across `exec`.
    pub trace: AtomicBool,
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
        executable: SpinLock::new(String::new()),
        arguments: SpinLock::new(Vec::new()),
        space: SpinLock::new(None),
        files: SpinLock::new(files),
        cwd: SpinLock::new(cwd),
        status: SpinLock::new(None),
        signals: SpinLock::new(Signals::new()),
        thread: SpinLock::new(None),
        trace: AtomicBool::new(false),
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
    /// The program actually run, and its arguments (scripts gain their
    /// interpreter).
    path: String,
    arguments: Vec<String>,
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
    let mut program = elf::load(&mut space, &data)?;
    if let Some(interpreter) = &program.interpreter {
        let inode = fs::lookup(cwd, interpreter, true)?;
        let mut interpreter_data = vec![0; inode.metadata().size as usize];
        inode.read_at(0, &mut interpreter_data)?;
        elf::load_interpreter(&mut space, &mut program, &interpreter_data)?;
    }
    let stack_pointer = elf::build_stack(&mut space, &program, &arguments, environment, path)?;
    let path = fs::normalize(cwd, path);
    Ok(Image { space, entry: program.start(), stack_pointer, path, arguments })
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
    *process.executable.lock() = image.path;
    *process.arguments.lock() = image.arguments;
    *process.space.lock() = Some(image.space);
    start_thread(&process, syscall::user_frame(image.entry, image.stack_pointer), FpuState::initial(), 0);
    Ok(process.pid)
}

fn default_environment() -> Vec<String> {
    ["PATH=/bin:/usr/bin", "HOME=/home", "TERM=linux", "SHELL=/bin/sh", "USER=root"].map(String::from).to_vec()
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
    *child.signals.lock() = parent.signals.lock().for_child();
    *child.executable.lock() = parent.executable.lock().clone();
    *child.arguments.lock() = parent.arguments.lock().clone();
    child.trace.store(parent.trace.load(Ordering::Relaxed), Ordering::Relaxed);
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
    process.signals.lock().exec();
    let name = base_name(&image.path).to_string();
    task::set_name(&name);
    *process.name.lock() = name;
    *process.executable.lock() = image.path;
    *process.arguments.lock() = image.arguments;
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

pub fn send_signal(process: &Process, signal: u32) {
    signal::send(process, signal);
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

/// Called with interrupts disabled whenever the kernel is about to return to
/// user mode: acts on pending signals and switches threads if one is due.
/// `syscall` is the number of the system call returning, if any.
pub fn before_return_to_user(frame: &mut InterruptFrame, syscall: Option<u64>) {
    if let Some(process) = task::current_process() {
        signal::deliver(&process, frame, syscall);
    }
    task::preempt_point();
}

/// Handles a CPU exception raised by user code: page faults inside the
/// program's areas are resolved; anything else raises the matching signal,
/// as on Linux, which ends the process unless it handles it.
pub fn handle_user_exception(frame: &mut InterruptFrame) {
    let process = current();
    let vector = frame.vector as u8;
    let (signal, description, address) = if vector == interrupts::PAGE_FAULT {
        let address = crate::arch::read_cr2();
        let write = frame.error_code & 2 != 0;
        let execute = frame.error_code & 0x10 != 0;
        let resolved = process.space.lock().as_mut().is_some_and(|space| space.handle_fault(address, write, execute));
        if resolved {
            return;
        }
        let access = if execute {
            "executing"
        } else if write {
            "writing"
        } else {
            "reading"
        };
        (SIGSEGV, alloc::format!("segmentation fault: {access} {address:#x}"), address)
    } else {
        let (signal, description) = match vector {
            0 => (SIGFPE, "division by zero"),
            3 => (SIGTRAP, "breakpoint"),
            6 => (SIGILL, "invalid instruction"),
            13 => (SIGSEGV, "general protection fault"),
            16 | 19 => (SIGFPE, "floating point exception"),
            17 => (SIGBUS, "misaligned access"),
            _ => (SIGSEGV, "CPU exception"),
        };
        (signal, description.into(), frame.rip)
    };
    if !signal::raise_fault(&process, signal, address) {
        warn!("{} (pid {}): {description} at {:#x}", process.name.lock(), process.pid, frame.rip);
    }
    signal::fault(&process, frame, signal, address);
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

    /// Reads a whole file, until end of file (/proc files report size 0).
    fn read_file(path: &str) -> String {
        let inode = fs::lookup("/", path, true).unwrap();
        let mut data = Vec::new();
        let mut chunk = [0; 512];
        loop {
            let count = inode.read_at(data.len() as u64, &mut chunk).unwrap();
            if count == 0 {
                return String::from_utf8(data).unwrap();
            }
            data.extend_from_slice(&chunk[..count]);
        }
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

        fn linux_c_programs_pass_their_self_tests() {
            // Built by the host's C compiler, if there was one.
            for program in ["ctest", "sigtest", "hello-c", "hello-dynamic"] {
                let path = alloc::format!("/usr/bin/{program}");
                if fs::lookup("/", &path, true).is_err() {
                    continue;
                }
                assert_eq!(shell(&alloc::format!("{program} > /dev/null")), exit_status(0), "{program} failed");
            }
        }

        fn proc_describes_processes() {
            assert_eq!(shell("cat /proc/self/status > /tmp/proc-test"), exit_status(0));
            let status = read_file("/tmp/proc-test");
            assert!(status.starts_with("Name:\tcat\n"), "{status}");
            assert!(read_file("/proc/meminfo").starts_with("MemTotal:"));
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
