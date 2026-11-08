//! User processes: creation (`fork`, `clone`, `exec`), threads,
//! termination, waiting, and signals.
//!
//! A process is an address space, open files, a working directory, signal
//! handlers, and one or more threads. Each thread is a scheduler thread
//! (see `task`) plus a [`UserThread`] record: its id as programs see it, its
//! signal mask and the like. The process ends when its last thread does.

pub mod address_space;
pub mod credentials;
pub mod elf;
pub mod futex;
pub mod signal;
pub mod timers;
pub mod user;
pub mod vdso;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub use self::address_space::{AddressSpace, PROT_EXEC, PROT_READ, PROT_WRITE, USER_END};
use self::credentials::{Credentials, Uid};
#[cfg(test)]
use self::signal::SIGTERM;
use self::signal::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
pub use self::signal::{
    SIGINT, SIGKILL, SIGNAL_COUNT, SIGPIPE, SigAction, Signals, ThreadSignals, pending as signal_pending,
};
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
    /// The running threads. Each also refers back to the process; the last
    /// one to exit ends it.
    pub threads: SpinLock<Vec<Arc<UserThread>>>,
    /// Woken when one of the threads exits.
    thread_exits: WaitQueue,
    /// Set when threads should end (see [`Ending`]).
    ending: SpinLock<Option<Ending>>,
    /// Whether to print every system call (see `strace`). Inherited by
    /// children and kept across `exec`.
    pub trace: AtomicBool,
    /// Samples of where the process was at timer ticks, while profiling
    /// (see /proc/PID/profile): user instruction pointer -> count, or for
    /// ticks spent in the kernel on its behalf, [`KERNEL_SAMPLE`] plus the
    /// system call running (+1; 0 for page faults and interrupts).
    pub profile: SpinLock<Option<BTreeMap<u64, u32>>>,
    /// The kernel's instruction pointers at those kernel samples.
    pub kernel_profile: SpinLock<Option<BTreeMap<u64, u32>>>,
    pub profiling: AtomicBool,
    /// The signal the parent gets when this process ends (normally SIGCHLD;
    /// 0 for none).
    exit_signal: AtomicU32,
    /// Who the process acts as (see [`credentials`]).
    pub credentials: SpinLock<Arc<Credentials>>,
    /// Permission bits taken away from the files it creates.
    umask: AtomicU32,
    /// The controlling terminal, which `/dev/tty` opens (inherited; a new
    /// session starts without one).
    pub terminal: SpinLock<Option<Arc<crate::tty::Tty>>>,
    /// `PR_SET_NO_NEW_PRIVS`: setuid programs don't gain privileges.
    /// Inherited, and permanent.
    pub no_new_privileges: AtomicBool,
    /// `PR_SET_PDEATHSIG`: the signal to get when the parent exits (0:
    /// none). Not inherited.
    pub death_signal: AtomicU32,
}

/// Why a process's threads are ending.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// The whole process is exiting, with this wait status (`exit_group`,
    /// or a fatal signal).
    Exit(i32),
    /// This thread is running `execve`; every other thread ends.
    Exec(ThreadId),
}

/// One thread of a user process.
pub struct UserThread {
    /// The scheduler's thread.
    pub id: ThreadId,
    /// The id programs see (`gettid`). The first thread's is the pid; the
    /// others' come from the same numbering.
    tid: AtomicU32,
    pub process: Arc<Process>,
    pub signals: SpinLock<ThreadSignals>,
    /// Set by `set_tid_address` or `CLONE_CHILD_CLEARTID`: when the thread
    /// exits, the kernel stores 0 here and wakes a futex waiter, which is
    /// how `pthread_join` learns a thread has finished.
    clear_child_tid: AtomicU64,
    /// The system call running now (number + 1; 0 for none) and its first
    /// arguments, for /proc/PID/syscall and /proc/PID/threads.
    pub syscall: AtomicU64,
    pub syscall_arguments: [AtomicU64; 3],
    /// The user stack pointer and instruction pointer at the last system
    /// call, for /proc/PID/stacks.
    pub syscall_stack: AtomicU64,
    pub syscall_rip: AtomicU64,
    /// Pages of user memory the running system call found unmapped by
    /// another thread, and the frame put there instead (see
    /// [`handle_kernel_fault`]).
    scratch: SpinLock<Option<(u64, Vec<u64>)>>,
}

impl UserThread {
    pub fn tid(&self) -> Pid {
        self.tid.load(Ordering::Relaxed)
    }

    pub fn set_clear_child_tid(&self, address: u64) {
        self.clear_child_tid.store(address, Ordering::Relaxed);
    }
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

    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::Relaxed)
    }

    /// Sets the umask, returning the old one.
    pub fn set_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask & 0o777, Ordering::Relaxed)
    }
}

static PROCESSES: SpinLock<BTreeMap<Pid, Arc<Process>>> = SpinLock::new(BTreeMap::new());
static NEXT_PID: AtomicU32 = AtomicU32::new(INIT);
/// Woken when any process exits, for `wait`.
static CHILD_EVENTS: WaitQueue = WaitQueue::new();
/// Whether process 1 is init (it isn't when tests start processes).
static INIT_RUNNING: AtomicBool = AtomicBool::new(false);

/// What a new process starts with.
struct Inherited {
    cwd: String,
    files: FileTable,
    credentials: Arc<Credentials>,
    umask: u32,
    terminal: Option<Arc<crate::tty::Tty>>,
}

fn create(parent: Pid, group: Option<Pid>, name: &str, inherited: Inherited) -> Arc<Process> {
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let process = Arc::new(Process {
        pid,
        parent: AtomicU32::new(parent),
        group: AtomicU32::new(group.unwrap_or(pid)),
        name: SpinLock::new(name.to_string()),
        executable: SpinLock::new(String::new()),
        arguments: SpinLock::new(Vec::new()),
        space: SpinLock::new(None),
        files: SpinLock::new(inherited.files),
        cwd: SpinLock::new(inherited.cwd),
        status: SpinLock::new(None),
        signals: SpinLock::new(Signals::new()),
        threads: SpinLock::new(Vec::new()),
        thread_exits: WaitQueue::new(),
        ending: SpinLock::new(None),
        trace: AtomicBool::new(false),
        profile: SpinLock::new(None),
        kernel_profile: SpinLock::new(None),
        profiling: AtomicBool::new(false),
        exit_signal: AtomicU32::new(signal::SIGCHLD),
        credentials: SpinLock::new(inherited.credentials),
        umask: AtomicU32::new(inherited.umask),
        terminal: SpinLock::new(inherited.terminal),
        no_new_privileges: AtomicBool::new(false),
        death_signal: AtomicU32::new(0),
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

/// The running user thread.
pub fn current_thread() -> Arc<UserThread> {
    task::current_user_thread().expect("not running in a user process")
}

/// How a new thread starts out.
struct NewThread {
    tid: Pid,
    signals: ThreadSignals,
    frame: InterruptFrame,
    fpu: FpuState,
    fs_base: u64,
    gs_base: u64,
    clear_child_tid: u64,
}

/// Adds a thread to `process` that enters user mode with `new.frame`.
fn start_thread(process: &Arc<Process>, new: NewThread) {
    let address_space = process.space.lock().as_ref().expect("process without memory").root();
    let thread = Arc::new(UserThread {
        id: task::new_id(),
        tid: AtomicU32::new(new.tid),
        process: process.clone(),
        signals: SpinLock::new(new.signals),
        clear_child_tid: AtomicU64::new(new.clear_child_tid),
        syscall: AtomicU64::new(0),
        syscall_arguments: Default::default(),
        syscall_stack: AtomicU64::new(0),
        syscall_rip: AtomicU64::new(0),
        scratch: SpinLock::new(None),
    });
    process.threads.lock().push(thread.clone());
    let options = Spawn {
        id: thread.id,
        name: process.name.lock().clone(),
        user: Some(thread),
        address_space,
        fpu: new.fpu,
        fs_base: new.fs_base,
        gs_base: new.gs_base,
    };
    let frame = new.frame;
    task::spawn_with(options, move || {
        // SAFETY: the frame was built for this process's address space,
        // which the scheduler has made active.
        unsafe { syscall::enter_user(&frame) }
    });
}

/// What it takes to run a program file.
struct Image {
    space: AddressSpace,
    entry: u64,
    stack_pointer: u64,
    /// The program actually run, with symbolic links resolved (for
    /// `/proc/PID/exe`), and its arguments (scripts gain their
    /// interpreter).
    path: String,
    /// The process's name: the program's file name as it was run (so
    /// BusyBox's links name their applets).
    name: String,
    arguments: Vec<String>,
    /// Who the program runs as: changed if it's setuid or setgid.
    credentials: Credentials,
}

/// Loads the program at `path` (resolved from `cwd`) into a new address
/// space, for a process with `credentials`. Scripts starting with `#!` run
/// their interpreter instead. The program must be executable by them. A
/// setuid or setgid program changes the credentials, if `setuid_allowed`.
fn load_image(
    credentials: &Credentials,
    setuid_allowed: bool,
    cwd: &str,
    path: &str,
    arguments: Vec<String>,
    environment: &[String],
) -> Result<Image> {
    let (inode, resolved) = fs::lookup_with_path(credentials, cwd, path)?;
    let metadata = inode.metadata();
    if metadata.kind != FileKind::Regular {
        return Err(Errno::EACCES);
    }
    fs::check_access(credentials, &metadata, fs::MAY_EXEC)?;
    let size = metadata.size as usize;
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
        // As on Linux, a script's own setuid bit is ignored (the
        // interpreter's counts).
        return load_image(credentials, setuid_allowed, cwd, interpreter, script_arguments, environment);
    }

    let credentials = if setuid_allowed {
        credentials.after_exec(metadata.permissions, metadata.uid, metadata.gid)
    } else {
        credentials.after_exec(0, 0, 0)
    };
    let mut space = AddressSpace::new()?;
    let mut program = elf::load(&mut space, &data)?;
    space.name_unnamed(&resolved, program.base);
    if let Some(interpreter) = program.interpreter.clone() {
        let inode = fs::lookup_as(&credentials, cwd, &interpreter, true)?;
        let mut interpreter_data = vec![0; inode.metadata().size as usize];
        inode.read_at(0, &mut interpreter_data)?;
        elf::load_interpreter(&mut space, &mut program, &interpreter_data)?;
        space.name_unnamed(&fs::normalize(cwd, &interpreter), program.interpreter_base);
    }
    program.vdso = vdso::map(&mut space)?;
    let stack_pointer = elf::build_stack(&mut space, &program, &arguments, environment, path, &credentials)?;
    space.set_name(stack_pointer, "[stack]".into());
    let name = base_name(&fs::normalize(cwd, path)).to_string();
    Ok(Image { space, entry: program.start(), stack_pointer, path: resolved, name, arguments, credentials })
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
    let root = credentials::kernel();
    let image = load_image(&root, true, "/", path, arguments, &environment)?;
    let console = fs::lookup("/", "/dev/console", true)?;
    let mut files = FileTable::default();
    let terminal = OpenFile::new(console, "/dev/console".into(), fs::O_RDWR);
    for _ in 0..3 {
        files.insert(terminal.clone(), false, 0)?;
    }
    let inherited = Inherited {
        cwd: "/".into(),
        files,
        credentials: Arc::new(image.credentials),
        umask: 0o022,
        terminal: Some(crate::tty::console()),
    };
    let process = create(0, None, base_name(path), inherited);
    *process.executable.lock() = image.path;
    *process.arguments.lock() = image.arguments;
    *process.space.lock() = Some(image.space);
    let frame = syscall::user_frame(image.entry, image.stack_pointer);
    let signals = ThreadSignals::default();
    start_thread(
        &process,
        NewThread {
            tid: process.pid,
            signals,
            frame,
            fpu: FpuState::initial(),
            fs_base: 0,
            gs_base: 0,
            clear_child_tid: 0,
        },
    );
    Ok(process.pid)
}

fn default_environment() -> Vec<String> {
    ["PATH=/bin:/usr/bin:/sbin:/usr/sbin", "HOME=/root", "TERM=linux", "SHELL=/bin/sh", "USER=root", "LOGNAME=root"]
        .map(String::from)
        .to_vec()
}

// `clone` flags (Linux values).
pub const CLONE_VM: u64 = 0x100;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_THREAD: u64 = 0x1_0000;
pub const CLONE_SETTLS: u64 = 0x8_0000;
pub const CLONE_PARENT_SETTID: u64 = 0x10_0000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
pub const CLONE_CHILD_SETTID: u64 = 0x100_0000;
/// The flags that ask for new namespaces (mount, UTS, IPC, user, PID,
/// network, cgroup, time). eliOS has none, and refuses them as Linux does
/// for someone not allowed to make them: sandboxing tools like bubblewrap
/// then know to do without.
pub const CLONE_NEW_NAMESPACES: u64 =
    0x2_0000 | 0x200_0000 | 0x400_0000 | 0x800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000 | 0x80;

/// The arguments of `clone` and `clone3` that eliOS uses.
#[derive(Debug, Clone, Copy)]
pub struct CloneArgs {
    pub flags: u64,
    /// For a new process: the signal its parent gets when it ends.
    pub exit_signal: u32,
    /// The new thread's stack pointer, or 0 to keep the caller's.
    pub stack: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub tls: u64,
}

/// `fork`: a copy of the running process, continuing from the same system
/// call but seeing 0 as its result.
pub fn fork(frame: &InterruptFrame) -> Result<Pid> {
    let args = CloneArgs { flags: 0, exit_signal: signal::SIGCHLD, stack: 0, parent_tid: 0, child_tid: 0, tls: 0 };
    clone(frame, &args)
}

/// `clone`: a new thread in this process (`CLONE_THREAD`), or a new
/// process. A new process always gets a copy of the caller's memory, even
/// with `CLONE_VM`: memory isn't shared between processes, and the programs
/// that ask for it (`vfork`, glibc's `posix_spawn`) work with a copy.
pub fn clone(frame: &InterruptFrame, args: &CloneArgs) -> Result<Pid> {
    let me = current_thread();
    let flags = args.flags;
    if flags & CLONE_NEW_NAMESPACES != 0 {
        return Err(Errno::EPERM);
    }
    let mut child_frame = *frame;
    child_frame.rax = 0;
    if args.stack != 0 {
        child_frame.rsp = args.stack;
    }
    let mut fpu = FpuState::initial();
    task::save_fpu(&mut fpu);
    let fs_base = if flags & CLONE_SETTLS != 0 {
        if args.tls >= USER_END {
            return Err(Errno::EPERM);
        }
        args.tls
    } else {
        syscall::fs_base()
    };
    let clear_child_tid = if flags & CLONE_CHILD_CLEARTID != 0 { args.child_tid } else { 0 };

    if flags & CLONE_THREAD != 0 {
        if flags & (CLONE_VM | CLONE_SIGHAND) != CLONE_VM | CLONE_SIGHAND {
            return Err(Errno::EINVAL);
        }
        let process = me.process.clone();
        if process.ending.lock().is_some() {
            return Err(Errno::EAGAIN);
        }
        let tid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        // Memory is shared, so both ids land in the same place.
        if flags & CLONE_PARENT_SETTID != 0 {
            user::write_value(args.parent_tid, &tid)?;
        }
        if flags & CLONE_CHILD_SETTID != 0 {
            user::write_value(args.child_tid, &tid)?;
        }
        let signals = me.signals.lock().for_new_thread();
        start_thread(
            &process,
            NewThread {
                tid,
                signals,
                frame: child_frame,
                fpu,
                fs_base,
                gs_base: syscall::user_gs_base(),
                clear_child_tid,
            },
        );
        return Ok(tid);
    }

    let parent = &me.process;
    let mut space = parent.space.lock().as_mut().ok_or(Errno::EFAULT)?.duplicate()?;
    let name = parent.name.lock().clone();
    let inherited = Inherited {
        cwd: parent.cwd.lock().clone(),
        files: parent.files.lock().clone(),
        credentials: parent.credentials.lock().clone(),
        umask: parent.umask(),
        terminal: parent.terminal.lock().clone(),
    };
    let child = create(parent.pid, Some(parent.group()), &name, inherited);
    *child.signals.lock() = parent.signals.lock().for_child();
    *child.executable.lock() = parent.executable.lock().clone();
    *child.arguments.lock() = parent.arguments.lock().clone();
    child.trace.store(parent.trace.load(Ordering::Relaxed), Ordering::Relaxed);
    child.no_new_privileges.store(parent.no_new_privileges.load(Ordering::Relaxed), Ordering::Relaxed);
    child.exit_signal.store(args.exit_signal, Ordering::Relaxed);
    if flags & CLONE_PARENT_SETTID != 0 {
        user::write_value(args.parent_tid, &child.pid)?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        space.write(args.child_tid, &child.pid.to_le_bytes())?;
    }
    *child.space.lock() = Some(space);
    let signals = me.signals.lock().for_child();
    start_thread(
        &child,
        NewThread {
            tid: child.pid,
            signals,
            frame: child_frame,
            fpu,
            fs_base,
            gs_base: syscall::user_gs_base(),
            clear_child_tid,
        },
    );
    Ok(child.pid)
}

/// `execve`: replaces the running program. On success the system call
/// "returns" into the new program, so `frame` is rewritten to start it.
pub fn exec(frame: &mut InterruptFrame, path: &str, arguments: Vec<String>, environment: Vec<String>) -> Result<()> {
    let thread = current_thread();
    let process = &thread.process;
    let cwd = process.cwd.lock().clone();
    let credentials = process.credentials.lock().clone();
    // A traced program can't gain privileges: tracing shows (and could
    // be used to steer) what it does. Linux does the same under ptrace.
    let setuid_allowed = !process.trace.load(Ordering::Relaxed) && !process.no_new_privileges.load(Ordering::Relaxed);
    let image = load_image(&credentials, setuid_allowed, &cwd, path, arguments, &environment)?;
    end_other_threads(&thread)?;

    // From here on, nothing can fail: the old program is gone.
    task::set_address_space(image.space.root());
    let old = process.space.lock().replace(image.space);
    drop(old);
    // The survivor becomes the main thread, if it wasn't.
    thread.tid.store(process.pid, Ordering::Relaxed);
    if image.credentials != *credentials {
        *process.credentials.lock() = Arc::new(image.credentials);
    }
    process.files.lock().exec();
    timers::process_executed(process.pid);
    process.signals.lock().exec();
    thread.signals.lock().exec();
    let name = image.name.clone();
    task::set_name(&name);
    *process.name.lock() = name;
    *process.executable.lock() = image.path;
    *process.arguments.lock() = image.arguments;
    task::reset_fpu();
    syscall::set_fs_base(0);
    syscall::set_user_gs_base(0);
    *frame = syscall::user_frame(image.entry, image.stack_pointer);
    Ok(())
}

/// For `execve`: ends every thread of the process but `thread`, and waits
/// until they're gone.
fn end_other_threads(thread: &UserThread) -> Result<()> {
    let process = &thread.process;
    {
        let mut ending = process.ending.lock();
        if ending.is_some() {
            // The process is exiting, or another thread got here first.
            return Err(Errno::EINTR);
        }
        *ending = Some(Ending::Exec(thread.id));
    }
    interrupt_threads(process, thread.id);
    let outcome = process.thread_exits.wait_until(|| match *process.ending.lock() {
        // exit_group in another thread overrules us.
        Some(Ending::Exit(_)) => Some(Err(Errno::EINTR)),
        _ => (process.threads.lock().len() == 1).then_some(Ok(())),
    });
    if outcome.is_ok() {
        *process.ending.lock() = None;
    }
    outcome
}

/// Wakes every thread of `process` but `except` from blocking calls, so
/// they notice they should end.
fn interrupt_threads(process: &Process, except: ThreadId) {
    for thread in process.threads.lock().iter().filter(|thread| thread.id != except) {
        task::interrupt(thread.id);
    }
}

/// Whether `thread` should end: its process is exiting, or another thread
/// is running `execve`.
fn must_end(thread: &UserThread) -> bool {
    match *thread.process.ending.lock() {
        Some(Ending::Exit(_)) => true,
        Some(Ending::Exec(survivor)) => survivor != thread.id,
        None => false,
    }
}

/// `exit_group`: ends the running process, every thread of it, with a wait
/// status (see [`exit_status`] and [`signal_status`]).
pub fn exit_group(status: i32) -> ! {
    let thread = current_thread();
    {
        let mut ending = thread.process.ending.lock();
        // The first exit_group decides the status.
        if !matches!(*ending, Some(Ending::Exit(_))) {
            *ending = Some(Ending::Exit(status));
        }
    }
    interrupt_threads(&thread.process, thread.id);
    drop(thread);
    exit_thread(status)
}

/// `exit`: ends the running thread. The last thread to exit ends the
/// process, with `status` unless `exit_group` set one.
pub fn exit_thread(status: i32) -> ! {
    let thread = current_thread();
    let process = thread.process.clone();
    let clear_child_tid = thread.clear_child_tid.swap(0, Ordering::Relaxed);
    if clear_child_tid != 0 && user::write_value(clear_child_tid, &0u32).is_ok() {
        let _ = futex::wake(clear_child_tid, false, 1, u32::MAX);
    }
    // Other threads may free the memory once this one is off the list.
    task::set_address_space(memory::kernel_page_table_root());
    let last = {
        let mut threads = process.threads.lock();
        threads.retain(|other| !Arc::ptr_eq(other, &thread));
        threads.is_empty()
    };
    drop(thread);
    process.thread_exits.wake_all();
    if last {
        let status = match *process.ending.lock() {
            Some(Ending::Exit(status)) => status,
            _ => status,
        };
        end_process(&process, status);
    }
    drop(process);
    task::exit()
}

/// Releases a process whose threads have all exited, and makes it a zombie
/// for its parent to collect.
fn end_process(process: &Process, status: i32) {
    timers::process_ended(process.pid);
    process.files.lock().close_all();
    let space = process.space.lock().take();
    drop(space);

    let mut orphans = Vec::new();
    for other in PROCESSES.lock().values() {
        if other.parent() == process.pid {
            other.parent.store(INIT, Ordering::Relaxed);
            orphans.push(other.clone());
        }
    }
    // Children that asked to be told their parent died.
    for orphan in orphans {
        let signal = orphan.death_signal.load(Ordering::Relaxed);
        if signal != 0 && orphan.status.lock().is_none() {
            send_signal(&orphan, signal);
        }
    }
    *process.status.lock() = Some(status);
    if process.pid == INIT && INIT_RUNNING.load(Ordering::Relaxed) {
        panic!("init exited (wait status {status:#x})");
    }
    let exit_signal = process.exit_signal.load(Ordering::Relaxed);
    if exit_signal != 0
        && let Some(parent) = find(process.parent())
    {
        send_signal(&parent, exit_signal);
    }
    CHILD_EVENTS.wake_all();
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
/// waiting; with `keep` (`waitid`'s `WNOWAIT`), leaves the child to be
/// collected again. Returns the child's pid, wait status and real user.
pub fn wait(pid: i64, no_hang: bool, keep: bool) -> Result<Option<(Pid, i32, Uid)>> {
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
            if !keep {
                processes.remove(&child.pid);
            }
            return Some(Ok(Some((child.pid, status, child.credentials.lock().uid))));
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

/// Sends a signal to the running thread (for faults like `SIGPIPE` that
/// a system call raises).
pub fn send_signal_to_self(signal: u32) {
    if let Some(thread) = task::current_user_thread() {
        signal::send_to_thread(&thread, signal);
    }
}

/// `tgkill`: sends a signal to one thread. A `process` of `None` (from
/// `tkill`) matches any.
pub fn kill_thread(process: Option<Pid>, tid: Pid, signal: u32) -> Result<()> {
    if signal as usize >= SIGNAL_COUNT {
        return Err(Errno::EINVAL);
    }
    let processes: Vec<Arc<Process>> = PROCESSES.lock().values().cloned().collect();
    let thread = processes
        .iter()
        .filter(|candidate| process.is_none_or(|pid| candidate.pid == pid))
        .find_map(|candidate| candidate.threads.lock().iter().find(|thread| thread.tid() == tid).cloned())
        .ok_or(Errno::ESRCH)?;
    let credentials = current().credentials.lock().clone();
    if !credentials.may_signal(&thread.process.credentials.lock()) {
        return Err(Errno::EPERM);
    }
    if signal != 0 {
        signal::send_to_thread(&thread, signal);
    }
    Ok(())
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
    let credentials = me.credentials.lock().clone();
    // Zombies count as targets (they exist until reaped, and Linux finds
    // them: `killall` signals every match and reports a failure), but
    // there's nothing to deliver to them.
    let targets: Vec<Arc<Process>> = PROCESSES
        .lock()
        .values()
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
    // Only processes of the same user (or anyone, for root) can be
    // signalled; sending to a group succeeds if any of them could be.
    let permitted: Vec<&Arc<Process>> =
        targets.iter().filter(|target| credentials.may_signal(&target.credentials.lock())).collect();
    if permitted.is_empty() {
        return Err(Errno::EPERM);
    }
    // Signal 0 only checks that the process exists and may be signalled.
    if signal != 0 {
        for process in permitted.into_iter().filter(|p| p.status.lock().is_none()) {
            send_signal(process, signal);
        }
    }
    Ok(())
}

/// Called with interrupts disabled whenever the kernel is about to return to
/// user mode: acts on pending signals and switches threads if one is due.
/// `syscall` is the number of the system call returning, if any.
pub fn before_return_to_user(frame: &mut InterruptFrame, syscall: Option<u64>) {
    // Switch first, so whatever happened while this thread waited its turn
    // (a signal, another thread's exit_group) is acted on before it runs.
    task::preempt_point();
    if let Some(thread) = task::current_user_thread() {
        signal::deliver(&thread, frame, syscall);
    }
}

/// Marks a profile sample taken in the kernel (user addresses never have
/// these bits).
pub const KERNEL_SAMPLE: u64 = 0xFFFF_0000_0000_0000;

/// Called on every timer tick: if the running thread's process is being
/// profiled, counts where it was (user code's instruction pointer, or the
/// system call it's in).
pub fn sample_tick(frame: &InterruptFrame) {
    let Some(thread) = task::current_user_thread() else { return };
    if !thread.process.profiling.load(Ordering::Relaxed) {
        return;
    }
    let user = frame.cs & 3 == 3;
    let place = if user { frame.rip } else { KERNEL_SAMPLE | thread.syscall.load(Ordering::Relaxed) };
    // Interrupts are off, so no lock this CPU holds (the heap's included)
    // can be held by the code the tick interrupted.
    if let Some(samples) = thread.process.profile.lock().as_mut() {
        *samples.entry(place).or_insert(0) += 1;
    }
    if !user && let Some(samples) = thread.process.kernel_profile.lock().as_mut() {
        *samples.entry(frame.rip).or_insert(0) += 1;
    }
}

/// Starts or stops profiling a process (writing `1` or `0` to
/// /proc/PID/profile). Starting clears earlier samples.
pub fn set_profiling(process: &Process, on: bool) {
    if on {
        *process.profile.lock() = Some(BTreeMap::new());
        *process.kernel_profile.lock() = Some(BTreeMap::new());
    }
    process.profiling.store(on, Ordering::Relaxed);
}

/// Handles a CPU exception raised by user code: page faults inside the
/// program's areas are resolved; anything else raises the matching signal,
/// as on Linux, which ends the process unless it handles it.
pub fn handle_user_exception(frame: &mut InterruptFrame) {
    let thread = current_thread();
    let process = &thread.process;
    let vector = frame.vector as u8;
    let (signal, description, address) = if vector == interrupts::PAGE_FAULT {
        let address = crate::arch::read_cr2();
        let write = frame.error_code & 2 != 0;
        let execute = frame.error_code & 0x10 != 0;
        let resolved = address_space::resolve_fault(&process.space, address, write, execute);
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
    if !signal::raise_fault(&thread, signal, address) {
        // Where it happened, as `library+offset` for addr2line or objdump.
        let place = process.space.lock().as_ref().and_then(|space| space.describe(frame.rip));
        let place = place.map(|(name, offset)| alloc::format!(" ({name}+{offset:#x})")).unwrap_or_default();
        warn!(
            "{} (pid {}): {description} at {:#x}{place}, stack {:#x}, error code {:#x}",
            process.name.lock(),
            process.pid,
            frame.rip,
            frame.rsp,
            frame.error_code
        );
    } else {
        // Handled by the program, but worth knowing about when debugging.
        debug!("{} (pid {}): {description} at {:#x} (handled)", process.name.lock(), thread.tid(), frame.rip);
    }
    signal::fault(&thread, frame, signal, address);
}

/// Handles a page fault the kernel took on a user address while reading or
/// writing memory for a program. System calls check user memory and map
/// it before using it, but another thread of the program can change it
/// meanwhile: a `fork` makes its pages copy-on-write. Returns whether the
/// fault was resolved (and the access can be retried).
///
/// Another thread can also unmap the memory (or end the area) while a
/// system call uses it in place (`read` into a buffer, say), which leaves
/// nothing to resolve. Linux would make the access fail with `EFAULT`
/// through an exception table; here the kernel code using the buffer is
/// any code at all (a file system filling it, a socket copying from it),
/// so instead a scratch frame only the kernel can reach is mapped at the
/// page, the access completes there, and when the system call returns its
/// result becomes `EFAULT` and the scratch pages go (see
/// [`finish_system_call`]). The program sees the unmapped memory as it
/// left it: its own accesses still fault.
pub fn handle_kernel_fault(frame: &InterruptFrame, address: u64) -> bool {
    let Some(process) = task::current_process() else { return false };
    let write = frame.error_code & 2 != 0;
    let not_present = frame.error_code & 1 == 0;
    let interrupts_were_on = frame.rflags & (1 << 9) != 0;
    if address_space::resolve_kernel_fault(&process.space, address, write, interrupts_were_on) {
        return true;
    }
    // Only during a system call: that's when the result can say so.
    let Some(thread) = task::current_user_thread() else { return false };
    if !not_present || thread.syscall.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let mut scratch = thread.scratch.lock();
    if scratch.is_none() {
        let Some(frame) = memory::frames::allocate_zeroed() else { return false };
        *scratch = Some((frame, Vec::new()));
    }
    let (scratch_frame, pages) = scratch.as_mut().unwrap();
    let page = address & !(memory::PAGE_SIZE - 1);
    let mapped = process.space.lock().as_mut().is_some_and(|space| space.map_scratch_page(page, *scratch_frame));
    if mapped {
        pages.push(page);
    }
    if mapped && pages.len() == 1 {
        debug!(
            "{} (pid {}): system call {} found {address:#x} unmapped",
            process.name.lock(),
            thread.tid(),
            thread.syscall.load(Ordering::Relaxed) - 1
        );
    }
    mapped
}

/// Called as each system call returns: if it ran into memory another
/// thread had unmapped ([`handle_kernel_fault`]), removes the scratch
/// pages and fails the call with `EFAULT`, as Linux would.
pub fn finish_system_call(thread: &UserThread, result: Result<u64>) -> Result<u64> {
    let Some((frame, pages)) = thread.scratch.lock().take() else { return result };
    if let Some(space) = thread.process.space.lock().as_mut() {
        for &page in &pages {
            space.unmap_scratch_page(page, frame);
        }
    }
    // Other threads may have cached the pages, though only the kernel
    // could use them: the frame goes once no CPU can reach it.
    if let (Some(&low), Some(&high)) = (pages.iter().min(), pages.iter().max()) {
        crate::arch::smp::flush_tlb(low, high + memory::PAGE_SIZE);
    }
    memory::frames::free(frame);
    Err(Errno::EFAULT)
}

/// A snapshot of a process, for `ps`.
pub struct ProcessInfo {
    pub pid: Pid,
    pub parent: Pid,
    pub name: String,
    pub state: char,
    pub cpu_ms: u64,
    pub memory: u64,
    pub threads: usize,
    pub credentials: Credentials,
}

pub fn list() -> Vec<ProcessInfo> {
    let threads = task::list();
    let processes: Vec<Arc<Process>> = PROCESSES.lock().values().cloned().collect();
    processes
        .iter()
        .map(|process| {
            let ids: Vec<ThreadId> = process.threads.lock().iter().map(|thread| thread.id).collect();
            let mine = || threads.iter().filter(|thread| ids.contains(&thread.id));
            let state = if process.status.lock().is_some() {
                'Z'
            } else if mine().any(|thread| matches!(thread.state, task::State::Running | task::State::Ready)) {
                'R'
            } else {
                'S'
            };
            ProcessInfo {
                pid: process.pid,
                parent: process.parent(),
                name: process.name.lock().clone(),
                state,
                cpu_ms: mine().map(|thread| thread.cpu_ticks).sum(),
                memory: process.space.lock().as_ref().map_or(0, AddressSpace::resident_bytes),
                threads: ids.len(),
                credentials: (**process.credentials.lock()).clone(),
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

        fn scripts_get_their_arguments() {
            // "$@" keeps each argument a word of its own (launchers like
            // Firefox's rely on it) and disappears when there are none.
            let script = "printf \"<%s>\" \"$@\" > /tmp/args-test; echo \" $#\" >> /tmp/args-test";
            assert_eq!(shell(&alloc::format!("echo '{script}' > /tmp/args.sh")), exit_status(0));
            assert_eq!(shell("sh /tmp/args.sh one 'two three'"), exit_status(0));
            assert_eq!(read_file("/tmp/args-test").trim(), "<one><two three> 2");
            assert_eq!(shell("sh /tmp/args.sh"), exit_status(0));
            assert_eq!(read_file("/tmp/args-test").trim(), "<> 0");
        }

        fn maps_name_what_is_mapped() {
            assert_eq!(shell("grep stack /proc/self/maps > /tmp/maps-test"), exit_status(0));
            assert!(read_file("/tmp/maps-test").trim_end().ends_with("[stack]"));
            assert_eq!(shell("grep /bin/ /proc/self/maps > /dev/null"), exit_status(0));
        }

        fn linux_c_programs_pass_their_self_tests() {
            // Built by the host's C compiler, if there was one.
            for program in [
                "ctest", "sigtest", "threadtest", "nettest", "permtest", "timertest", "memtest", "eventtest", "unixtest", "ptytest", "vdsotest", "videotest",
                "hello-c",
                "hello-dynamic",
            ] {
                let path = alloc::format!("/usr/bin/{program}");
                if fs::lookup("/", &path, true).is_err() {
                    continue;
                }
                let output = alloc::format!("/tmp/{program}-output");
                let status = shell(&alloc::format!("{program} > {output} 2>&1"));
                assert_eq!(status, exit_status(0), "{program} failed:\n{}", read_file(&output));
            }
        }

        fn ping_reaches_loopback_and_the_gateway() {
            assert_eq!(shell("ping -c 1 127.0.0.1 > /dev/null"), exit_status(0));
            // With QEMU's user networking (see xtask), DHCP has run by now
            // and its virtual router answers pings.
            let configured = crate::net::interfaces().iter().any(|i| i.name == "eth0" && i.config.is_some());
            if configured {
                assert_eq!(shell("ping -c 1 10.0.2.2 > /dev/null"), exit_status(0));
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
            task::wait_for_reaping();
            let after = memory::frames::stats().free_bytes;
            assert!(after + 16 * memory::PAGE_SIZE >= before, "{} KiB not returned", (before - after) / 1024);
        }
    }
}
