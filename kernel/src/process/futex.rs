//! Futexes: the kernel half of user-space locks.
//!
//! A futex is an ordinary 32-bit variable in a program's memory. Threads
//! take and release locks with atomic instructions alone, and only call the
//! kernel when they must wait (`FUTEX_WAIT`: "sleep if the variable still
//! holds this value") or wake waiters (`FUTEX_WAKE`). glibc builds its
//! mutexes, condition variables, `pthread_join` and more on these two
//! operations.
//!
//! Waiters are kept in one list, keyed by process and address (eliOS has no
//! memory shared between processes, so every futex is private to one). Each
//! waiter has its own wait queue, so waking one thread wakes exactly that
//! thread.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Pid, user};
use crate::errno::{Errno, Result};
use crate::sync::SpinLock;
use crate::task::WaitQueue;

struct Waiter {
    process: Pid,
    address: u64,
    /// For the `_BITSET` operations: a wake only reaches waiters whose bits
    /// overlap its own.
    bitset: u32,
    /// The waiter sleeps here; it's woken once removed from the list.
    queue: Arc<WaitQueue>,
}

static WAITERS: SpinLock<Vec<Waiter>> = SpinLock::new(Vec::new());

/// `FUTEX_WAIT`: sleeps until woken, if the variable at `address` holds
/// `expected`. Gives up at timer tick `deadline`, or when a signal arrives.
pub fn wait(process: Pid, address: u64, expected: u32, bitset: u32, deadline: Option<u64>) -> Result<()> {
    if !address.is_multiple_of(4) || bitset == 0 {
        return Err(Errno::EINVAL);
    }
    let queue = Arc::new(WaitQueue::new());
    let mut queued = false;
    let outcome = queue.wait_until_deadline(deadline, || {
        let mut waiters = WAITERS.lock();
        if !queued {
            // Checking the value and joining the list happen under the lock,
            // so a wake that follows a change to the value can't be missed.
            let value: u32 = match user::read_value(address) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            if value != expected {
                return Some(Err(Errno::EAGAIN));
            }
            waiters.push(Waiter { process, address, bitset, queue: queue.clone() });
            queued = true;
            return None;
        }
        // Not in the list any more: a wake removed it.
        let Some(index) = waiters.iter().position(|waiter| Arc::ptr_eq(&waiter.queue, &queue)) else {
            return Some(Ok(()));
        };
        if super::signal_pending() {
            waiters.remove(index);
            return Some(Err(Errno::EINTR));
        }
        None
    });
    match outcome {
        Some(result) => result,
        None => {
            // Timed out, unless a wake removed it at the last moment.
            let mut waiters = WAITERS.lock();
            let before = waiters.len();
            waiters.retain(|waiter| !Arc::ptr_eq(&waiter.queue, &queue));
            if waiters.len() < before { Err(Errno::ETIMEDOUT) } else { Ok(()) }
        }
    }
}

/// `FUTEX_WAKE`: wakes up to `count` threads waiting at `address`, and
/// returns how many it woke.
pub fn wake(process: Pid, address: u64, count: u32, bitset: u32) -> Result<u64> {
    if bitset == 0 {
        return Err(Errno::EINVAL);
    }
    let woken =
        take(|waiter| waiter.process == process && waiter.address == address && waiter.bitset & bitset != 0, count);
    for waiter in &woken {
        waiter.queue.wake_all();
    }
    Ok(woken.len() as u64)
}

/// `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE`: wakes up to `count` waiters at
/// `from` and moves up to `requeue` more to wait at `to` instead. With
/// `expected`, does nothing unless `from` still holds that value.
pub fn requeue(process: Pid, from: u64, to: u64, count: u32, requeue: u32, expected: Option<u32>) -> Result<u64> {
    let mut waiters = WAITERS.lock();
    if let Some(expected) = expected
        && user::read_value::<u32>(from)? != expected
    {
        return Err(Errno::EAGAIN);
    }
    let mut woken = 0;
    let mut moved = 0;
    let mut index = 0;
    while index < waiters.len() {
        let waiter = &mut waiters[index];
        if waiter.process != process || waiter.address != from {
            index += 1;
        } else if woken < count {
            waiters.remove(index).queue.wake_all();
            woken += 1;
        } else if moved < requeue {
            waiter.address = to;
            moved += 1;
            index += 1;
        } else {
            break;
        }
    }
    Ok(u64::from(woken + moved))
}

/// Removes up to `count` waiters matching `matches`, in the order they
/// started waiting.
fn take(matches: impl Fn(&Waiter) -> bool, count: u32) -> Vec<Waiter> {
    let mut waiters = WAITERS.lock();
    let mut taken = Vec::new();
    let mut index = 0;
    while index < waiters.len() && taken.len() < count as usize {
        if matches(&waiters[index]) {
            taken.push(waiters.remove(index));
        } else {
            index += 1;
        }
    }
    taken
}
