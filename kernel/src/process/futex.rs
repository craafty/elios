//! Futexes: the kernel half of user-space locks.
//!
//! A futex is an ordinary 32-bit variable in a program's memory. Threads
//! take and release locks with atomic instructions alone, and only call the
//! kernel when they must wait (`FUTEX_WAIT`: "sleep if the variable still
//! holds this value") or wake waiters (`FUTEX_WAKE`). glibc and musl build
//! their mutexes, condition variables, `pthread_join` and more on these two
//! operations.
//!
//! Waiters are kept in a map by [`Key`]: a futex in a process's private
//! memory is identified by the process and address, and one in shared
//! memory (`MAP_SHARED`, which other processes may map at other addresses)
//! by the memory object and offset, as on Linux. Each waiter has its own
//! wait queue, so waking one thread wakes exactly that thread, and a wake
//! only looks at the waiters for its own futex.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Pid, user};
use crate::errno::{Errno, Result};
use crate::memory::object::MemoryObject;
use crate::sync::SpinLock;
use crate::task::WaitQueue;

/// Which futex: what makes two waits and a wake meet.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Private {
        process: Pid,
        address: u64,
    },
    /// The memory object (by address) and the offset in it.
    Shared {
        object: usize,
        offset: u64,
    },
}

/// Identifies the futex at `address` in the running process. Operations
/// with `FUTEX_PRIVATE_FLAG` promise it isn't shared, which saves looking.
/// The object of a shared one is returned too, to be kept alive while
/// waited on (so its address can't be reused for another).
pub fn key(address: u64, private: bool) -> (Key, Option<Arc<MemoryObject>>) {
    let process = super::current();
    if !private {
        let shared = process.space.lock().as_ref().and_then(|space| space.shared_object_at(address));
        if let Some((object, offset)) = shared {
            return (Key::Shared { object: Arc::as_ptr(&object) as usize, offset }, Some(object));
        }
    }
    (Key::Private { process: process.pid, address }, None)
}

struct Waiter {
    /// For the `_BITSET` operations: a wake only reaches waiters whose bits
    /// overlap its own.
    bitset: u32,
    /// The waiter sleeps here; it's woken once removed from the map.
    queue: Arc<WaitQueue>,
    /// Keeps a shared futex's memory object alive.
    _object: Option<Arc<MemoryObject>>,
}

/// Waiters by futex, each list in the order they started waiting.
static WAITERS: SpinLock<BTreeMap<Key, Vec<Waiter>>> = SpinLock::new(BTreeMap::new());

/// Removes the waiter sleeping on `queue` from `key`'s list; returns whether
/// it was there.
fn remove(waiters: &mut BTreeMap<Key, Vec<Waiter>>, key: Key, queue: &Arc<WaitQueue>) -> bool {
    let Some(list) = waiters.get_mut(&key) else { return false };
    let Some(index) = list.iter().position(|waiter| Arc::ptr_eq(&waiter.queue, queue)) else { return false };
    list.remove(index);
    if list.is_empty() {
        waiters.remove(&key);
    }
    true
}

/// `FUTEX_WAIT`: sleeps until woken, if the variable at `address` holds
/// `expected`. Gives up at timer tick `deadline`, or when a signal arrives.
pub fn wait(address: u64, private: bool, expected: u32, bitset: u32, deadline: Option<u64>) -> Result<()> {
    if !address.is_multiple_of(4) || bitset == 0 {
        return Err(Errno::EINVAL);
    }
    let (mut key, object) = key(address, private);
    let queue = Arc::new(WaitQueue::new());
    let mut waiter = Some(Waiter { bitset, queue: queue.clone(), _object: object });
    let outcome = queue.wait_until_deadline(deadline, || {
        let mut waiters = WAITERS.lock();
        if let Some(waiter) = waiter.take() {
            // Checking the value and joining the list happen under the lock,
            // so a wake that follows a change to the value can't be missed.
            let value: u32 = match user::read_value(address) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            if value != expected {
                return Some(Err(Errno::EAGAIN));
            }
            waiters.entry(key).or_default().push(waiter);
            return None;
        }
        // A requeue may have moved it to another futex.
        let Some(here) = find(&waiters, key, &queue) else {
            // Not in the map any more: a wake removed it.
            return Some(Ok(()));
        };
        key = here;
        if super::signal_pending() {
            remove(&mut waiters, key, &queue);
            return Some(Err(Errno::EINTR));
        }
        None
    });
    match outcome {
        Some(result) => result,
        None => {
            // Timed out, unless a wake removed it at the last moment.
            let mut waiters = WAITERS.lock();
            match find(&waiters, key, &queue) {
                Some(here) => {
                    remove(&mut waiters, here, &queue);
                    Err(Errno::ETIMEDOUT)
                }
                None => Ok(()),
            }
        }
    }
}

/// Where the waiter sleeping on `queue` is: under `key`, or (after a
/// requeue) under another key.
fn find(waiters: &BTreeMap<Key, Vec<Waiter>>, key: Key, queue: &Arc<WaitQueue>) -> Option<Key> {
    let here = |list: &Vec<Waiter>| list.iter().any(|waiter| Arc::ptr_eq(&waiter.queue, queue));
    if waiters.get(&key).is_some_and(here) {
        return Some(key);
    }
    waiters.iter().find(|(_, list)| here(list)).map(|(&key, _)| key)
}

/// `FUTEX_WAKE`: wakes up to `count` threads waiting at `address`, and
/// returns how many it woke.
pub fn wake(address: u64, private: bool, count: u32, bitset: u32) -> Result<u64> {
    if bitset == 0 {
        return Err(Errno::EINVAL);
    }
    let (key, _object) = key(address, private);
    let woken = {
        let mut waiters = WAITERS.lock();
        let Some(list) = waiters.get_mut(&key) else { return Ok(0) };
        let mut woken = Vec::new();
        let mut index = 0;
        while index < list.len() && woken.len() < count as usize {
            if list[index].bitset & bitset != 0 {
                woken.push(list.remove(index));
            } else {
                index += 1;
            }
        }
        if list.is_empty() {
            waiters.remove(&key);
        }
        woken
    };
    for waiter in &woken {
        waiter.queue.wake_all();
    }
    Ok(woken.len() as u64)
}

/// `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE`: wakes up to `count` waiters at
/// `from` and moves up to `requeue` more to wait at `to` instead. With
/// `expected`, does nothing unless `from` still holds that value.
pub fn requeue(from: u64, to: u64, private: bool, count: u32, requeue: u32, expected: Option<u32>) -> Result<u64> {
    let (from_key, _from_object) = key(from, private);
    let (to_key, to_object) = key(to, private);
    let woken = {
        let mut waiters = WAITERS.lock();
        if let Some(expected) = expected
            && user::read_value::<u32>(from)? != expected
        {
            return Err(Errno::EAGAIN);
        }
        let Some(mut list) = waiters.remove(&from_key) else { return Ok(0) };
        let woken: Vec<Waiter> = list.drain(..list.len().min(count as usize)).collect();
        let moved: Vec<Waiter> = list
            .drain(..list.len().min(requeue as usize))
            .map(|waiter| Waiter { _object: to_object.clone(), ..waiter })
            .collect();
        let total = woken.len() + moved.len();
        if !list.is_empty() {
            waiters.insert(from_key, list);
        }
        if !moved.is_empty() {
            waiters.entry(to_key).or_default().extend(moved);
        }
        (woken, total)
    };
    for waiter in &woken.0 {
        waiter.queue.wake_all();
    }
    Ok(woken.1 as u64)
}
