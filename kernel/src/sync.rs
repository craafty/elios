//! Synchronization primitives.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use crate::arch;

/// A busy-waiting mutual exclusion lock.
///
/// Interrupts are disabled while the lock is held. Otherwise an interrupt
/// handler that takes the same lock could interrupt the holder and spin
/// forever waiting for it.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock ensures only one thread accesses the value at a time.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let interrupts_were_enabled = arch::interrupts_enabled();
        arch::disable_interrupts();
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // Wait with plain loads so contended cores don't keep stealing
            // the cache line from each other.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self, interrupts_were_enabled }
    }

    /// Access through exclusive ownership needs no locking.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Takes the lock only if it is free right now.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let interrupts_were_enabled = arch::interrupts_enabled();
        arch::disable_interrupts();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return Some(SpinLockGuard { lock: self, interrupts_were_enabled });
        }
        if interrupts_were_enabled {
            arch::enable_interrupts();
        }
        None
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    interrupts_were_enabled: bool,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard means holding the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard means holding the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: core::fmt::Display> core::fmt::Display for SpinLockGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.interrupts_were_enabled {
            arch::enable_interrupts();
        }
    }
}

/// A lock that puts waiting threads to sleep instead of spinning, with
/// interrupts left enabled. For holding across slow operations like disk
/// I/O; needs the scheduler running.
pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: crate::task::WaitQueue,
    value: UnsafeCell<T>,
}

// SAFETY: the lock ensures only one thread accesses the value at a time.
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), waiters: crate::task::WaitQueue::new(), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.waiters.wait_until(|| {
            self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok().then_some(())
        });
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard means holding the lock.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard means holding the lock.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
        self.mutex.waiters.wake_all();
    }
}

/// A static whose contents can be borrowed mutably exactly once, giving out a
/// `&'static mut` without `static mut`.
pub struct TakeOnce<T> {
    taken: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is handed to at most one taker.
unsafe impl<T: Send> Sync for TakeOnce<T> {}

impl<T> TakeOnce<T> {
    pub const fn new(value: T) -> Self {
        Self { taken: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// The value on the first call; `None` on every call after that.
    // Handing out `&mut` from `&self` is the point: the flag makes it unique.
    #[allow(clippy::mut_from_ref)]
    pub fn take(&'static self) -> Option<&'static mut T> {
        if self.taken.swap(true, Ordering::AcqRel) {
            return None;
        }
        // SAFETY: the flag guarantees this is the only reference ever made.
        Some(unsafe { &mut *self.value.get() })
    }
}

/// A value set once, at runtime, then shared read-only forever. For things
/// discovered during boot, like the ACPI tables.
pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

const ONCE_EMPTY: u8 = 0;
const ONCE_WRITING: u8 = 1;
const ONCE_READY: u8 = 2;

// SAFETY: the value is written once before being published, then only read.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(ONCE_EMPTY), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Stores the value, or hands it back if one was already stored.
    pub fn set(&self, value: T) -> Result<&T, T> {
        if self.state.compare_exchange(ONCE_EMPTY, ONCE_WRITING, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Err(value);
        }
        // SAFETY: winning the exchange makes us the only writer, and readers
        // wait for ONCE_READY.
        let stored = unsafe { (*self.value.get()).write(value) };
        self.state.store(ONCE_READY, Ordering::Release);
        Ok(stored)
    }

    pub fn get(&self) -> Option<&T> {
        // SAFETY: ONCE_READY is only published after the value is written.
        (self.state.load(Ordering::Acquire) == ONCE_READY).then(|| unsafe { (*self.value.get()).assume_init_ref() })
    }
}

/// A fixed-size byte queue for one producer and one consumer, without locks.
/// Made for interrupt handlers handing data to the rest of the kernel.
pub struct ByteQueue<const N: usize> {
    bytes: [AtomicU8; N],
    /// Total bytes ever pushed and popped; their difference is the length.
    pushed: AtomicUsize,
    popped: AtomicUsize,
}

impl<const N: usize> ByteQueue<N> {
    pub const fn new() -> Self {
        Self { bytes: [const { AtomicU8::new(0) }; N], pushed: AtomicUsize::new(0), popped: AtomicUsize::new(0) }
    }

    /// Adds a byte; returns false (dropping it) if the queue is full. Only
    /// one context may push.
    pub fn push(&self, byte: u8) -> bool {
        let pushed = self.pushed.load(Ordering::Relaxed);
        if pushed.wrapping_sub(self.popped.load(Ordering::Acquire)) == N {
            return false;
        }
        self.bytes[pushed % N].store(byte, Ordering::Relaxed);
        self.pushed.store(pushed.wrapping_add(1), Ordering::Release);
        true
    }

    /// Removes the oldest byte. Only one context may pop.
    pub fn pop(&self) -> Option<u8> {
        let popped = self.popped.load(Ordering::Relaxed);
        if popped == self.pushed.load(Ordering::Acquire) {
            return None;
        }
        let byte = self.bytes[popped % N].load(Ordering::Relaxed);
        self.popped.store(popped.wrapping_add(1), Ordering::Release);
        Some(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn spinlock_guards_its_value() {
            let lock = SpinLock::new(5);
            *lock.lock() += 1;
            assert_eq!(*lock.lock(), 6);
        }

        fn spinlock_try_lock_fails_while_held() {
            let lock = SpinLock::new(());
            let guard = lock.lock();
            assert!(lock.try_lock().is_none());
            drop(guard);
            assert!(lock.try_lock().is_some());
        }

        fn spinlock_masks_interrupts_while_held() {
            let lock = SpinLock::new(());
            assert!(arch::interrupts_enabled());
            let guard = lock.lock();
            assert!(!arch::interrupts_enabled());
            let nested = SpinLock::new(());
            drop(nested.lock());
            assert!(!arch::interrupts_enabled(), "inner guard re-enabled interrupts early");
            drop(guard);
            assert!(arch::interrupts_enabled());
        }

        fn byte_queue_is_first_in_first_out() {
            let queue = ByteQueue::<2>::new();
            assert!(queue.push(1));
            assert!(queue.push(2));
            assert!(!queue.push(3));
            assert_eq!(queue.pop(), Some(1));
            assert!(queue.push(4));
            assert_eq!(queue.pop(), Some(2));
            assert_eq!(queue.pop(), Some(4));
            assert_eq!(queue.pop(), None);
        }

        fn once_keeps_the_first_value() {
            let once = Once::new();
            assert_eq!(once.get(), None);
            assert_eq!(once.set(1).copied(), Ok(1));
            assert_eq!(once.set(2).err(), Some(2));
            assert_eq!(once.get(), Some(&1));
        }

        fn take_once_hands_out_its_value_once() {
            static CELL: TakeOnce<u32> = TakeOnce::new(7);
            assert_eq!(CELL.take().map(|value| *value), Some(7));
            assert!(CELL.take().is_none());
        }
    }
}
