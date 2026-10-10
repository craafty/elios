//! Threads and the scheduler.
//!
//! Every thread has its own kernel stack. Switching threads means saving the
//! callee-saved registers on the current stack, storing the stack pointer,
//! and loading another thread's (see `switch_context`). Everything else the
//! thread was doing, including an interrupt it was in the middle of
//! handling, is already on its stack.
//!
//! Scheduling is round-robin with 10 ms time slices, from one ready queue
//! that every CPU takes threads from. Each CPU also has an idle thread, run
//! when nothing else is ready. The timer interrupt only marks that a switch
//! is due; the switch happens at the end of interrupt handling, after the
//! interrupt has been acknowledged.
//!
//! The scheduler's lock is held across a switch: the CPU switching away
//! takes it, and the thread it switches to releases it. Until then, no other
//! CPU can see the old thread as switched out (and start running it on a
//! stack that's still in use).

mod wait;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub use self::wait::WaitQueue;
use crate::arch::cpu::{self, Cpu};
use crate::arch::fpu::{self, FpuState};
use crate::arch::{self, smp, syscall};
use crate::memory::{self, KernelStack};
use crate::process::{Process, UserThread};
use crate::sync::SpinLock;
use crate::time;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Ready,
    /// Waiting to be woken (on a [`WaitQueue`]), or for a time to pass.
    Blocked,
    /// Finished; its resources are freed once no CPU is using its stack.
    Exited,
}

/// What `list` reports about a thread.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub state: State,
    /// Timer ticks (milliseconds) spent running.
    pub cpu_ticks: u64,
}

pub const STACK_PAGES: u64 = 16;
const TIME_SLICE_TICKS: u64 = 10;

struct Thread {
    id: ThreadId,
    name: String,
    state: State,
    /// Saved stack pointer while not running.
    rsp: u64,
    stack: Option<KernelStack>,
    cpu_ticks: u64,
    /// Tick at which a blocked thread wakes by itself (`u64::MAX`: never).
    wake_at: u64,
    /// A wakeup came while the thread was still running (about to block,
    /// say): its next attempt to block returns at once.
    wake_pending: bool,
    kernel_stack_top: u64,
    /// The page tables to run with (CR3).
    address_space: u64,
    /// The user thread this is, if it runs a program.
    user: Option<Arc<UserThread>>,
    /// User register state the kernel doesn't touch, saved while switched
    /// out.
    fpu: Box<FpuState>,
    fs_base: u64,
}

/// How to start a thread.
pub struct Spawn {
    /// From [`new_id`].
    pub id: ThreadId,
    pub name: String,
    pub user: Option<Arc<UserThread>>,
    pub address_space: u64,
    pub fpu: FpuState,
    pub fs_base: u64,
}

impl Spawn {
    pub fn kernel(name: &str) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            user: None,
            address_space: memory::kernel_page_table_root(),
            fpu: FpuState::initial(),
            fs_base: 0,
        }
    }
}

/// The scheduler's view of one CPU.
struct CpuSlot {
    current: ThreadId,
    /// Runs when nothing else can; never in the ready queue.
    idle: ThreadId,
    slice_left: u64,
    /// Asked to look for work, and hasn't yet.
    kicked: bool,
    /// Timer ticks spent running threads, and idle.
    busy_ticks: u64,
    idle_ticks: u64,
}

impl CpuSlot {
    fn new(current: ThreadId, idle: ThreadId) -> Self {
        Self { current, idle, slice_left: TIME_SLICE_TICKS, kicked: false, busy_ticks: 0, idle_ticks: 0 }
    }
}

struct Scheduler {
    /// Boxed so a thread's saved `rsp` keeps its address while the map
    /// changes.
    threads: BTreeMap<ThreadId, Box<Thread>>,
    ready: VecDeque<ThreadId>,
    /// Blocked threads with a time to wake at.
    sleeping: Vec<ThreadId>,
    /// Exited threads whose stacks haven't been freed yet.
    dead: Vec<ThreadId>,
    /// Indexed by CPU index.
    cpus: Vec<CpuSlot>,
}

static SCHEDULER: SpinLock<Option<Scheduler>> = SpinLock::new(None);
/// Woken whenever any thread exits; joiners wait here.
static EXITED: WaitQueue = WaitQueue::new();
/// Thread ids 1 and 2 are the boot and idle threads.
static NEXT_ID: AtomicU64 = AtomicU64::new(3);
/// How many exited threads haven't been completely freed yet (so most
/// switches needn't look). Counted up when a thread joins the dead list,
/// down once its memory is freed.
static DEAD: AtomicUsize = AtomicUsize::new(0);

/// A fresh thread id. Allocated ahead of [`spawn_with`] so a user thread's
/// records can name the thread before it starts.
pub fn new_id() -> ThreadId {
    ThreadId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

global_asm!(
    r#"
    .section .text.switch_context, "ax"

    // switch_context(save_rsp: *mut u64 [rdi], load_rsp: u64 [rsi])
    //
    // Saves the callee-saved registers on the current stack, stores the
    // stack pointer to *save_rsp, then does the reverse with load_rsp. The
    // final `ret` resumes the other thread where it called switch_context,
    // or, for a new thread, at thread_trampoline.
    .global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

    // A new thread's first stop: its entry closure is in r12.
    .global thread_trampoline
thread_trampoline:
    mov rdi, r12
    call thread_start
    ud2
    "#
);

unsafe extern "sysv64" {
    fn switch_context(save_rsp: *mut u64, load_rsp: u64);
    fn thread_trampoline();
}

type Entry = Box<dyn FnOnce() + Send>;

/// Where every new thread begins (called by `thread_trampoline`).
#[unsafe(no_mangle)]
extern "sysv64" fn thread_start(entry: *mut Entry) -> ! {
    finish_switch();
    // SAFETY: `create_thread` leaked this box for us.
    let entry = unsafe { Box::from_raw(entry) };
    // Threads are switched to with interrupts disabled.
    arch::enable_interrupts();
    entry();
    exit()
}

fn new_thread(options: Spawn, state: State, rsp: u64, stack: KernelStack) -> Box<Thread> {
    Box::new(Thread {
        id: options.id,
        name: options.name,
        state,
        rsp,
        kernel_stack_top: stack.top,
        stack: Some(stack),
        cpu_ticks: 0,
        wake_at: u64::MAX,
        wake_pending: false,
        address_space: options.address_space,
        user: options.user,
        fpu: Box::new(options.fpu),
        fs_base: options.fs_base,
    })
}

/// Builds a thread whose first switch lands in `thread_trampoline`.
fn create_thread(options: Spawn, entry: Entry) -> Box<Thread> {
    let stack = memory::allocate_kernel_stack(STACK_PAGES);
    let entry = Box::into_raw(Box::new(entry));
    // The frame `switch_context` pops: r15, r14, r13, r12, rbx, rbp, then
    // the return address. Placing that at top - 8 leaves the stack 16-byte
    // aligned once it's popped, as the ABI expects at the `call`.
    let frame = [0, 0, 0, entry as u64, 0, 0, thread_trampoline as *const () as u64];
    let rsp = stack.top - size_of_val(&frame) as u64;
    // SAFETY: the top of a freshly mapped stack that nothing else uses.
    unsafe { (rsp as *mut [u64; 7]).write(frame) };
    new_thread(options, State::Ready, rsp, stack)
}

/// Turns the code that's running on the boot CPU into the first thread (it
/// owns `boot_stack`, which it's running on) and starts scheduling.
pub fn init(name: &str, boot_stack: KernelStack) {
    let main = ThreadId(1);
    let idle = ThreadId(2);
    let cpu = cpu::current();
    syscall::set_kernel_stack(boot_stack.top);
    let mut threads = BTreeMap::new();
    let options = Spawn { id: main, ..Spawn::kernel(name) };
    threads.insert(main, new_thread(options, State::Running, 0, boot_stack));
    threads.insert(idle, create_thread(Spawn { id: idle, ..Spawn::kernel("idle") }, Box::new(idle_loop)));
    cpu.current_thread.store(main.0, Ordering::Relaxed);
    *SCHEDULER.lock() = Some(Scheduler {
        threads,
        ready: VecDeque::new(),
        sleeping: Vec::new(),
        dead: Vec::new(),
        cpus: alloc::vec![CpuSlot::new(main, idle)],
    });
    cpu.online.store(true, Ordering::Release);
}

/// Joins another CPU to the scheduler: the code running on it (on `stack`)
/// becomes its idle thread.
pub fn start_cpu(cpu: &'static Cpu, stack: KernelStack) -> ! {
    let id = new_id();
    syscall::set_kernel_stack(stack.top);
    let name = alloc::format!("idle{}", cpu.index());
    let thread = new_thread(Spawn { id, ..Spawn::kernel(&name) }, State::Running, 0, stack);
    with(|s| {
        s.threads.insert(id, thread);
        let index = cpu.index();
        if s.cpus.len() <= index {
            // Slots for CPUs that never started are marked kicked, so
            // they're never asked to run anything.
            let absent = || CpuSlot { kicked: true, ..CpuSlot::new(ThreadId(0), ThreadId(0)) };
            s.cpus.resize_with(index + 1, absent);
        }
        s.cpus[index] = CpuSlot::new(id, id);
        cpu.current_thread.store(id.0, Ordering::Relaxed);
        cpu.online.store(true, Ordering::Release);
    });
    idle_loop();
    unreachable!("the idle loop returned")
}

fn idle_loop() {
    loop {
        // Sleep until an interrupt; if it made a thread ready, the switch
        // happens on the way out of the interrupt.
        arch::wait_for_interrupt();
    }
}

fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    f(SCHEDULER.lock().as_mut().expect("scheduler used before init"))
}

/// Runs `f` with interrupts disabled, so the thread stays on this CPU.
fn pinned<R>(f: impl FnOnce() -> R) -> R {
    let enabled = arch::interrupts_enabled();
    arch::disable_interrupts();
    let result = f();
    if enabled {
        arch::enable_interrupts();
    }
    result
}

/// Starts a new kernel thread running `f`.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> ThreadId {
    spawn_with(Spawn::kernel(name), f)
}

/// Starts a thread with the given settings, such as a user process's
/// address space.
pub fn spawn_with(options: Spawn, f: impl FnOnce() + Send + 'static) -> ThreadId {
    let id = options.id;
    let thread = create_thread(options, Box::new(f));
    with(|s| {
        s.threads.insert(id, thread);
        s.ready.push_back(id);
        s.kick_idle_cpu();
    });
    id
}

pub fn current() -> ThreadId {
    pinned(|| ThreadId(cpu::current().current_thread.load(Ordering::Relaxed)))
}

/// The user thread that's running, if it is one.
pub fn current_user_thread() -> Option<Arc<UserThread>> {
    pinned(|| {
        let pointer = cpu::current().current_user.load(Ordering::Relaxed).cast::<UserThread>().cast_const();
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the running thread's record holds a reference, which it
        // keeps at least as long as the thread runs.
        unsafe {
            Arc::increment_strong_count(pointer);
            Some(Arc::from_raw(pointer))
        }
    })
}

/// The user process the running thread belongs to.
pub fn current_process() -> Option<Arc<Process>> {
    current_user_thread().map(|thread| thread.process.clone())
}

/// Changes the running thread's page tables (for exec and exit) and
/// switches to them.
pub fn set_address_space(root: u64) {
    pinned(|| {
        with(|s| {
            let current = s.current();
            s.threads.get_mut(&current).unwrap().address_space = root;
        });
        // SAFETY: callers pass tables that share the kernel half.
        unsafe { arch::set_page_table_root(root) };
    });
}

/// Saves the running thread's FPU registers into `state` (to give a forked
/// child the same registers).
pub fn save_fpu(state: &mut FpuState) {
    fpu::save(state);
}

/// Resets the running thread's FPU registers (for exec).
pub fn reset_fpu() {
    fpu::restore(&FpuState::initial());
}

pub fn set_name(name: &str) {
    pinned(|| {
        with(|s| {
            let current = s.current();
            s.threads.get_mut(&current).unwrap().name = name.into();
        })
    });
}

/// Lets other ready threads run before continuing.
pub fn yield_now() {
    pinned(|| switch(How::Yield));
}

/// Sleeps for at least `ms` milliseconds.
pub fn sleep_ms(ms: u64) {
    // +1 so a sleep that starts just before a tick still lasts `ms`.
    let deadline = time::ticks() + ms + 1;
    while time::ticks() < deadline {
        pinned(|| switch(How::Block(Some(deadline))));
    }
}

/// Ends the current thread.
pub fn exit() -> ! {
    arch::disable_interrupts();
    with(|s| {
        let current = s.current();
        s.threads.get_mut(&current).unwrap().state = State::Exited;
        s.dead.push(current);
        DEAD.fetch_add(1, Ordering::Relaxed);
    });
    EXITED.wake_all();
    switch(How::Exit);
    unreachable!("an exited thread was scheduled");
}

/// Waits for a thread to exit.
#[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
pub fn join(id: ThreadId) {
    EXITED.wait_until(|| {
        let finished = with(|s| s.threads.get(&id).is_none_or(|thread| thread.state == State::Exited));
        finished.then_some(())
    });
}

/// Each CPU's (busy, idle) time in timer ticks, by CPU index.
pub fn cpu_times() -> Vec<(u64, u64)> {
    with(|s| s.cpus.iter().map(|slot| (slot.busy_ticks, slot.idle_ticks)).collect())
}

pub fn list() -> Vec<ThreadInfo> {
    with(|s| {
        s.threads
            .values()
            .filter(|thread| thread.state != State::Exited)
            .map(|thread| ThreadInfo { id: thread.id, state: thread.state, cpu_ticks: thread.cpu_ticks })
            .collect()
    })
}

/// Blocks the current thread until [`wake`], or until timer tick
/// `deadline` if one is given. Returns at once if a wakeup already came.
/// Interrupts must be disabled.
fn block_current(deadline: Option<u64>) {
    switch(How::Block(deadline));
}

/// Makes a blocked thread ready again (if it's about to block, it won't).
/// Callable from interrupt handlers.
pub fn wake(id: ThreadId) {
    with(|s| s.wake(id));
}

/// Wakes a thread that's blocked, so it notices a signal.
pub fn interrupt(id: ThreadId) {
    wake(id);
}

/// Called from the timer interrupt every tick on every CPU; `now` is the
/// new tick count on the CPU that keeps time.
pub fn on_tick(now: Option<u64>) {
    let cpu = cpu::current();
    let mut guard = SCHEDULER.lock();
    let Some(s) = guard.as_mut() else { return };
    let Some(slot) = s.cpus.get_mut(cpu.index()) else { return };
    slot.slice_left = slot.slice_left.saturating_sub(1);
    let (current, idle, expired) = (slot.current, slot.idle, slot.slice_left == 0);
    if current == idle {
        slot.idle_ticks += 1;
    } else {
        slot.busy_ticks += 1;
    }
    if let Some(thread) = s.threads.get_mut(&current) {
        thread.cpu_ticks += 1;
    }

    if let Some(now) = now {
        let mut woken = Vec::new();
        let Scheduler { threads, sleeping, .. } = &mut *s;
        sleeping.retain(|id| {
            let Some(thread) = threads.get_mut(id) else { return false };
            // Woken early (a blocked thread with a deadline, say): forget it.
            if thread.state != State::Blocked {
                return false;
            }
            if thread.wake_at > now {
                return true;
            }
            woken.push(*id);
            false
        });
        for id in woken {
            s.wake(id);
        }
    }

    if expired || (current == idle && !s.ready.is_empty()) {
        cpu.need_reschedule.store(true, Ordering::Relaxed);
    }
}

/// Called at the end of interrupt handling (interrupts disabled): switches
/// threads if one is due.
pub fn preempt_point() {
    if cpu::current().need_reschedule.swap(false, Ordering::Relaxed) {
        switch(How::Yield);
    }
}

/// Why the running thread is giving up the CPU.
#[derive(Clone, Copy)]
enum How {
    /// It can keep running, after the others' turn.
    Yield,
    /// Until woken, or until a tick.
    Block(Option<u64>),
    /// It has exited.
    Exit,
}

/// Picks the next thread and switches to it. Interrupts must be disabled.
fn switch(how: How) {
    debug_assert!(!arch::interrupts_enabled());
    let cpu = cpu::current();
    let index = cpu.index();
    let mut guard = SCHEDULER.lock();
    let s = guard.as_mut().expect("scheduler used before init");
    let CpuSlot { current, idle, .. } = s.cpus[index];
    let thread = s.threads.get_mut(&current).unwrap();
    match how {
        How::Block(deadline) => {
            if thread.wake_pending {
                thread.wake_pending = false;
                return;
            }
            thread.state = State::Blocked;
            thread.wake_at = deadline.unwrap_or(u64::MAX);
            if deadline.is_some() {
                s.sleeping.push(current);
            }
        }
        How::Yield => {
            if thread.state == State::Running {
                thread.state = State::Ready;
                if current != idle {
                    s.ready.push_back(current);
                }
            }
        }
        How::Exit => {}
    }

    let next = s.ready.pop_front().unwrap_or(idle);
    let slot = &mut s.cpus[index];
    slot.slice_left = TIME_SLICE_TICKS;
    slot.kicked = false;
    slot.current = next;
    s.threads.get_mut(&next).unwrap().state = State::Running;
    if next == current {
        drop(guard);
        if DEAD.load(Ordering::Relaxed) > 0 {
            reap();
        }
        return;
    }

    // Swap the user-visible state the switch itself doesn't cover.
    let old = s.threads.get_mut(&current).unwrap();
    fpu::save(&mut old.fpu);
    old.fs_base = syscall::fs_base();
    let old_address_space = old.address_space;
    let save_rsp = &raw mut old.rsp;

    let new = &s.threads[&next];
    fpu::restore(&new.fpu);
    syscall::set_fs_base(new.fs_base);
    syscall::set_kernel_stack(new.kernel_stack_top);
    cpu.current_thread.store(next.0, Ordering::Relaxed);
    let user = new.user.as_ref().map_or(core::ptr::null_mut(), |user| Arc::as_ptr(user).cast_mut().cast());
    cpu.current_user.store(user, Ordering::Relaxed);
    if new.address_space != old_address_space {
        // SAFETY: every address space shares the kernel half, where this
        // code and the stacks live.
        unsafe { arch::set_page_table_root(new.address_space) };
    }
    let load_rsp = new.rsp;

    // The lock stays held through the switch; the next thread releases it.
    core::mem::forget(guard);
    // SAFETY: both threads' records outlive the switch (neither is freed
    // while it's some CPU's current thread or the lock is held), and the
    // saved stack pointer is a stack set up for switch_context.
    unsafe { switch_context(save_rsp, load_rsp) };
    finish_switch();
}

/// The first thing a thread does once switched to: releases the lock the
/// switching CPU held, and frees threads that have finished exiting.
fn finish_switch() {
    // SAFETY: the CPU that switched here holds the lock and forgot its guard.
    unsafe { SCHEDULER.force_unlock() };
    if DEAD.load(Ordering::Relaxed) > 0 {
        reap();
    }
}

/// Waits until exited threads have been freed (which happens at a later
/// switch, once no CPU is on their stacks), for tests that count memory.
#[cfg(test)]
pub fn wait_for_reaping() {
    for _ in 0..100 {
        if DEAD.load(Ordering::Relaxed) == 0 {
            return;
        }
        sleep_ms(5);
    }
}

/// Frees exited threads that no CPU is still running on.
fn reap() {
    let finished: Vec<Box<Thread>> = with(|s| {
        let in_use: Vec<ThreadId> = s.cpus.iter().map(|slot| slot.current).collect();
        let (finished, waiting): (Vec<ThreadId>, Vec<ThreadId>) = s.dead.iter().partition(|id| !in_use.contains(id));
        s.dead = waiting;
        finished.iter().filter_map(|id| s.threads.remove(id)).collect()
    });
    for mut thread in finished {
        if let Some(stack) = thread.stack.take() {
            memory::free_kernel_stack(stack);
        }
        drop(thread);
        DEAD.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Scheduler {
    /// The thread running on this CPU.
    fn current(&self) -> ThreadId {
        self.cpus[cpu::current().index()].current
    }

    fn wake(&mut self, id: ThreadId) {
        let Some(thread) = self.threads.get_mut(&id) else { return };
        match thread.state {
            State::Blocked => {
                thread.state = State::Ready;
                self.ready.push_back(id);
                self.kick_idle_cpu();
            }
            State::Running | State::Ready => thread.wake_pending = true,
            State::Exited => {}
        }
    }

    /// Gets an idle CPU (this one if it's idle) to pick up a newly ready
    /// thread.
    fn kick_idle_cpu(&mut self) {
        let me = cpu::current().index();
        let idle = |slot: &CpuSlot| slot.current == slot.idle && !slot.kicked;
        let target = if self.cpus.get(me).is_some_and(idle) { Some(me) } else { self.cpus.iter().position(idle) };
        let Some(target) = target else { return };
        self.cpus[target].kicked = true;
        if let Some(cpu) = cpu::get(target) {
            cpu.need_reschedule.store(true, Ordering::Relaxed);
            if target != me {
                smp::send_reschedule(target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

    use super::*;
    use crate::memory::frames;

    crate::ktest! {
        fn spawned_threads_run_and_can_be_joined() {
            let ran = Arc::new(AtomicBool::new(false));
            let flag = ran.clone();
            let id = spawn("test", move || flag.store(true, Ordering::Relaxed));
            join(id);
            assert!(ran.load(Ordering::Relaxed));
        }

        fn busy_threads_are_preempted() {
            // None of these ever yields; they only all make progress if the
            // timer takes the CPU away from them.
            let stop = Arc::new(AtomicBool::new(false));
            let counters: Arc<[AtomicU64; 3]> = Arc::new(Default::default());
            let ids: Vec<ThreadId> = (0..3)
                .map(|index| {
                    let (stop, counters) = (stop.clone(), counters.clone());
                    spawn("spinner", move || {
                        while !stop.load(Ordering::Relaxed) {
                            counters[index].fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            sleep_ms(100);
            stop.store(true, Ordering::Relaxed);
            ids.into_iter().for_each(join);
            for counter in counters.iter() {
                assert!(counter.load(Ordering::Relaxed) > 0, "a thread never ran");
            }
        }

        fn sleep_lasts_long_enough() {
            let start = time::uptime_nanos();
            sleep_ms(30);
            let elapsed_ms = (time::uptime_nanos() - start) / 1_000_000;
            assert!((30..300).contains(&elapsed_ms), "slept {elapsed_ms} ms");
        }

        fn yielding_threads_take_turns() {
            let order = Arc::new(SpinLock::new(Vec::new()));
            let ids = (*b"ab").map(|letter| {
                let order = order.clone();
                spawn("taker", move || {
                    for _ in 0..3 {
                        order.lock().push(letter);
                        yield_now();
                    }
                })
            });
            ids.into_iter().for_each(join);
            let order = order.lock();
            if cpu::online_count() == 1 {
                assert_eq!(*order, vec![b'a', b'b', b'a', b'b', b'a', b'b']);
            } else {
                // With more CPUs both run at once, in no particular order.
                assert_eq!(order.iter().filter(|&&letter| letter == b'a').count(), 3);
                assert_eq!(order.len(), 6);
            }
        }

        fn wait_queues_block_until_woken() {
            static QUEUE: WaitQueue = WaitQueue::new();
            static VALUE: AtomicUsize = AtomicUsize::new(0);
            let id = spawn("waiter", || {
                let seen = QUEUE.wait_until(|| match VALUE.load(Ordering::Relaxed) {
                    0 => None,
                    value => Some(value),
                });
                VALUE.store(seen * 10, Ordering::Relaxed);
            });
            sleep_ms(10);
            assert!(list().iter().any(|t| t.id == id && t.state == State::Blocked));
            VALUE.store(4, Ordering::Relaxed);
            QUEUE.wake_all();
            join(id);
            assert_eq!(VALUE.load(Ordering::Relaxed), 40);
        }

        fn threads_run_on_every_cpu() {
            let cpus = cpu::online_count();
            let seen = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let ids: Vec<ThreadId> = (0..cpus * 2)
                .map(|_| {
                    let (seen, stop) = (seen.clone(), stop.clone());
                    spawn("spreader", move || {
                        while !stop.load(Ordering::Relaxed) {
                            seen.fetch_or(1 << pinned(|| cpu::current().index()), Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            sleep_ms(300);
            stop.store(true, Ordering::Relaxed);
            ids.into_iter().for_each(join);
            assert_eq!(seen.load(Ordering::Relaxed).count_ones() as usize, cpus, "threads didn't reach every CPU");
        }

        fn spinlocks_exclude_other_cpus() {
            let counter = Arc::new(SpinLock::new(0u64));
            let ids: Vec<ThreadId> = (0..4)
                .map(|_| {
                    let counter = counter.clone();
                    spawn("incrementer", move || {
                        for _ in 0..50_000 {
                            // A read-modify-write that tears without the lock.
                            let mut value = counter.lock();
                            let read = *value;
                            core::hint::spin_loop();
                            *value = read + 1;
                        }
                    })
                })
                .collect();
            ids.into_iter().for_each(join);
            assert_eq!(*counter.lock(), 200_000);
        }

        fn wakeups_between_cpus_are_never_lost() {
            // Two threads hand a token back and forth; a lost wakeup hangs.
            static QUEUE: WaitQueue = WaitQueue::new();
            static TURN: AtomicU64 = AtomicU64::new(0);
            const ROUNDS: u64 = 2000;
            let ids = [0u64, 1].map(|me| {
                spawn("ping-pong", move || {
                    for round in 0..ROUNDS {
                        QUEUE.wait_until(|| (TURN.load(Ordering::Acquire) == round * 2 + me).then_some(()));
                        TURN.fetch_add(1, Ordering::Release);
                        QUEUE.wake_all();
                    }
                })
            });
            ids.into_iter().for_each(join);
            assert_eq!(TURN.load(Ordering::Relaxed), ROUNDS * 2);
        }

        fn exited_threads_give_back_their_stacks() {
            let round = || {
                let ids: Vec<ThreadId> = (0..8).map(|_| spawn("short", || {})).collect();
                ids.into_iter().for_each(join);
                wait_for_reaping();
            };
            // Warm up: the first round may grow structures that stay (heap
            // pages, page tables for the stack region).
            round();
            let before = frames::stats().free_bytes;
            round();
            let after = frames::stats().free_bytes;
            assert!(after + 4 * memory::PAGE_SIZE >= before, "{} bytes leaked", before - after);
        }
    }
}
