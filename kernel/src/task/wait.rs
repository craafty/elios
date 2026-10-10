//! Wait queues: how threads sleep until something happens.

use alloc::collections::VecDeque;

use super::ThreadId;
use crate::arch;
use crate::sync::SpinLock;

/// Threads waiting for some condition. Whoever changes the condition calls
/// [`wake_all`](Self::wake_all); waiters recheck it and go back to sleep if
/// it still doesn't hold.
pub struct WaitQueue {
    waiters: SpinLock<VecDeque<ThreadId>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self { waiters: SpinLock::new(VecDeque::new()) }
    }

    /// Blocks until `ready` returns a value, and returns it. `ready` runs
    /// with interrupts disabled.
    pub fn wait_until<T>(&self, ready: impl FnMut() -> Option<T>) -> T {
        self.wait_until_deadline(None, ready).expect("waits without a deadline don't time out")
    }

    /// Like [`wait_until`](Self::wait_until), but gives up (returning
    /// `None`) at timer tick `deadline`.
    ///
    /// A waiter joins the queue *before* checking the condition. A wakeup
    /// can then never fall between the check and going to sleep, even from
    /// another CPU: either the check sees the change, or the wakeup finds
    /// the waiter queued (and if it hasn't blocked yet, it won't).
    pub fn wait_until_deadline<T>(&self, deadline: Option<u64>, mut ready: impl FnMut() -> Option<T>) -> Option<T> {
        let enabled = arch::interrupts_enabled();
        let me = super::current();
        let result = loop {
            arch::disable_interrupts();
            {
                let mut waiters = self.waiters.lock();
                if !waiters.contains(&me) {
                    waiters.push_back(me);
                }
            }
            if let Some(value) = ready() {
                break Some(value);
            }
            if deadline.is_some_and(|deadline| crate::time::ticks() >= deadline) {
                break None;
            }
            super::block_current(deadline);
            if enabled {
                arch::enable_interrupts();
            }
        };
        self.waiters.lock().retain(|&waiter| waiter != me);
        if enabled {
            arch::enable_interrupts();
        }
        result
    }

    /// Wakes every waiting thread. Callable from interrupt handlers.
    pub fn wake_all(&self) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for id in waiters {
            super::wake(id);
        }
    }
}
