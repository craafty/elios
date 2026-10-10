//! Threads and the scheduler.
//!
//! Every thread has its own kernel stack. Switching threads means saving the
//! callee-saved registers on the current stack, storing the stack pointer,
//! and loading another thread's (see `switch_context`). Everything else the
//! thread was doing, including an interrupt it was in the middle of
//! handling, is already on its stack.
//!
//! Scheduling is round-robin with 10 ms time slices. The timer interrupt
//! only marks that a switch is due; the switch happens at the end of
//! interrupt handling, after the interrupt has been acknowledged.
//!
//! There is one CPU for now. The scheduler's lock is released before the
//! actual switch, which is safe only because interrupts stay disabled across
//! it; supporting more CPUs will need per-CPU run queues.

mod wait;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

pub use self::wait::WaitQueue;
use crate::arch;
use crate::memory::{self, KernelStack};
use crate::sync::SpinLock;
use crate::time;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Ready,
    /// Waiting on a [`WaitQueue`].
    Blocked,
    Sleeping,
    /// Finished; its resources are freed at the next switch.
    Exited,
}

/// What `list` reports about a thread.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub name: String,
    pub state: State,
    /// Timer ticks (milliseconds) spent running.
    pub cpu_ticks: u64,
}

const STACK_PAGES: u64 = 16;
const TIME_SLICE_TICKS: u64 = 10;

struct Thread {
    id: ThreadId,
    name: String,
    state: State,
    /// Saved stack pointer while not running.
    rsp: u64,
    /// `None` only while the stack is in use by `switch_context` on exit.
    stack: Option<KernelStack>,
    cpu_ticks: u64,
    /// Tick at which a sleeping thread wakes.
    wake_at: u64,
}

struct Scheduler {
    /// Boxed so a thread's saved `rsp` keeps its address while the map
    /// changes.
    threads: BTreeMap<ThreadId, Box<Thread>>,
    ready: VecDeque<ThreadId>,
    sleeping: Vec<ThreadId>,
    current: ThreadId,
    /// Runs when nothing else can; never in the ready queue.
    idle: ThreadId,
    slice_left: u64,
    next_id: u64,
}

static SCHEDULER: SpinLock<Option<Scheduler>> = SpinLock::new(None);
/// Set when the running thread should be switched out at the next chance.
static NEED_RESCHEDULE: AtomicBool = AtomicBool::new(false);
/// Woken whenever any thread exits; joiners wait here.
static EXITED: WaitQueue = WaitQueue::new();

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
    // SAFETY: `create_thread` leaked this box for us.
    let entry = unsafe { Box::from_raw(entry) };
    // Threads are switched to with interrupts disabled.
    arch::enable_interrupts();
    entry();
    exit()
}

/// Builds a thread whose first switch lands in `thread_trampoline`.
fn create_thread(id: ThreadId, name: &str, entry: Entry) -> Box<Thread> {
    let stack = memory::allocate_kernel_stack(STACK_PAGES);
    let entry = Box::into_raw(Box::new(entry));
    // The frame `switch_context` pops: r15, r14, r13, r12, rbx, rbp, then
    // the return address. Placing that at top - 8 leaves the stack 16-byte
    // aligned once it's popped, as the ABI expects at the `call`.
    let frame = [0, 0, 0, entry as u64, 0, 0, thread_trampoline as *const () as u64];
    let rsp = stack.top - size_of_val(&frame) as u64;
    // SAFETY: the top of a freshly mapped stack that nothing else uses.
    unsafe { (rsp as *mut [u64; 7]).write(frame) };
    Box::new(Thread { id, name: name.into(), state: State::Ready, rsp, stack: Some(stack), cpu_ticks: 0, wake_at: 0 })
}

/// Turns the code that's running into the first thread (it owns
/// `boot_stack`, which it's running on) and starts scheduling.
pub fn init(name: &str, boot_stack: KernelStack) {
    let main = ThreadId(1);
    let idle = ThreadId(2);
    let mut threads = BTreeMap::new();
    threads.insert(
        main,
        Box::new(Thread {
            id: main,
            name: name.into(),
            state: State::Running,
            rsp: 0,
            stack: Some(boot_stack),
            cpu_ticks: 0,
            wake_at: 0,
        }),
    );
    threads.insert(idle, create_thread(idle, "idle", Box::new(idle_loop)));
    *SCHEDULER.lock() = Some(Scheduler {
        threads,
        ready: VecDeque::new(),
        sleeping: Vec::new(),
        current: main,
        idle,
        slice_left: TIME_SLICE_TICKS,
        next_id: 3,
    });
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

/// Starts a new kernel thread running `f`.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> ThreadId {
    let id = with(|s| {
        s.next_id += 1;
        ThreadId(s.next_id - 1)
    });
    let thread = create_thread(id, name, Box::new(f));
    with(|s| {
        s.threads.insert(id, thread);
        s.ready.push_back(id);
    });
    id
}

pub fn current() -> ThreadId {
    with(|s| s.current)
}

/// Lets other ready threads run before continuing.
pub fn yield_now() {
    let enabled = arch::interrupts_enabled();
    arch::disable_interrupts();
    schedule();
    if enabled {
        arch::enable_interrupts();
    }
}

/// Sleeps for at least `ms` milliseconds.
pub fn sleep_ms(ms: u64) {
    let enabled = arch::interrupts_enabled();
    arch::disable_interrupts();
    with(|s| {
        let current = s.current;
        let thread = s.threads.get_mut(&current).unwrap();
        thread.state = State::Sleeping;
        // +1 so a sleep that starts just before a tick still lasts `ms`.
        thread.wake_at = time::ticks() + ms + 1;
        s.sleeping.push(current);
    });
    schedule();
    if enabled {
        arch::enable_interrupts();
    }
}

/// Ends the current thread.
pub fn exit() -> ! {
    arch::disable_interrupts();
    with(|s| {
        let current = s.current;
        s.threads.get_mut(&current).unwrap().state = State::Exited;
    });
    EXITED.wake_all();
    schedule();
    unreachable!("an exited thread was scheduled");
}

/// Waits for a thread to exit.
pub fn join(id: ThreadId) {
    EXITED.wait_until(|| {
        let finished = with(|s| s.threads.get(&id).is_none_or(|thread| thread.state == State::Exited));
        finished.then_some(())
    });
}

pub fn list() -> Vec<ThreadInfo> {
    with(|s| {
        s.threads
            .values()
            .filter(|thread| thread.state != State::Exited)
            .map(|thread| ThreadInfo {
                id: thread.id,
                name: thread.name.clone(),
                state: thread.state,
                cpu_ticks: thread.cpu_ticks,
            })
            .collect()
    })
}

/// Blocks the current thread until [`wake`]. Interrupts must be disabled,
/// so that a wakeup can't slip in before the thread is marked blocked.
fn block_current() {
    with(|s| {
        let current = s.current;
        s.threads.get_mut(&current).unwrap().state = State::Blocked;
    });
    schedule();
}

/// Makes a blocked thread ready again. Callable from interrupt handlers.
fn wake(id: ThreadId) {
    with(|s| {
        if let Some(thread) = s.threads.get_mut(&id)
            && thread.state == State::Blocked
        {
            thread.state = State::Ready;
            s.ready.push_back(id);
            if s.current == s.idle {
                NEED_RESCHEDULE.store(true, Ordering::Relaxed);
            }
        }
    });
}

/// Called from the timer interrupt every tick.
pub fn on_tick(now: u64) {
    let mut guard = SCHEDULER.lock();
    let Some(s) = guard.as_mut() else { return };

    let current = s.current;
    s.threads.get_mut(&current).unwrap().cpu_ticks += 1;

    let mut woke = false;
    let Scheduler { threads, sleeping, ready, .. } = &mut *s;
    sleeping.retain(|id| {
        let thread = threads.get_mut(id).unwrap();
        if thread.wake_at > now {
            return true;
        }
        thread.state = State::Ready;
        ready.push_back(*id);
        woke = true;
        false
    });

    s.slice_left = s.slice_left.saturating_sub(1);
    if s.slice_left == 0 || (woke && s.current == s.idle) {
        NEED_RESCHEDULE.store(true, Ordering::Relaxed);
    }
}

/// Called at the end of interrupt handling (interrupts disabled): switches
/// threads if one is due.
pub fn preempt_point() {
    if NEED_RESCHEDULE.swap(false, Ordering::Relaxed) {
        schedule();
    }
}

/// Picks the next thread and switches to it. Interrupts must be disabled.
fn schedule() {
    debug_assert!(!arch::interrupts_enabled());
    let switch = with(|s| {
        s.reap();
        let current = s.current;
        let thread = s.threads.get_mut(&current).unwrap();
        if thread.state == State::Running {
            thread.state = State::Ready;
            if current != s.idle {
                s.ready.push_back(current);
            }
        }

        let next = s.ready.pop_front().unwrap_or(s.idle);
        s.slice_left = TIME_SLICE_TICKS;
        let next_thread = s.threads.get_mut(&next).unwrap();
        next_thread.state = State::Running;
        let load_rsp = next_thread.rsp;
        if next == current {
            return None;
        }
        s.current = next;
        let save_rsp = &raw mut s.threads.get_mut(&current).unwrap().rsp;
        Some((save_rsp, load_rsp))
    });

    if let Some((save_rsp, load_rsp)) = switch {
        // SAFETY: both threads' records outlive the switch (the current one
        // isn't reaped while current, the next one is ready), and the saved
        // stack pointer is a stack set up for switch_context.
        unsafe { switch_context(save_rsp, load_rsp) };
    }
}

impl Scheduler {
    /// Frees threads that have exited, except the current one, whose stack
    /// is still in use.
    fn reap(&mut self) {
        let current = self.current;
        let dead: Vec<ThreadId> = self
            .threads
            .values()
            .filter(|thread| thread.state == State::Exited && thread.id != current)
            .map(|thread| thread.id)
            .collect();
        for id in dead {
            let mut thread = self.threads.remove(&id).unwrap();
            if let Some(stack) = thread.stack.take() {
                memory::free_kernel_stack(stack);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use core::sync::atomic::{AtomicU64, AtomicUsize};

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
            let ids = [b'a', b'b'].map(|letter| {
                let order = order.clone();
                spawn("taker", move || {
                    for _ in 0..3 {
                        order.lock().push(letter);
                        yield_now();
                    }
                })
            });
            ids.into_iter().for_each(join);
            assert_eq!(*order.lock(), vec![b'a', b'b', b'a', b'b', b'a', b'b']);
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

        fn exited_threads_give_back_their_stacks() {
            // Warm up: the first stacks may need new page tables, which stay.
            join(spawn("warmup", || {}));
            yield_now();
            let before = frames::stats().free_bytes;
            let ids: Vec<ThreadId> = (0..8).map(|_| spawn("short", || {})).collect();
            ids.into_iter().for_each(join);
            // Reaping happens at the next switch.
            yield_now();
            let after = frames::stats().free_bytes;
            assert!(after + 4 * memory::PAGE_SIZE >= before, "{} bytes leaked", before - after);
        }
    }
}
