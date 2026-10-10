//! Synchronization primitives.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A busy-waiting mutual exclusion lock.
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
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // Wait with plain loads so contended cores don't keep stealing
            // the cache line from each other.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }

    /// Takes the lock only if it is free right now.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then_some(SpinLockGuard { lock: self })
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
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

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
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

        fn take_once_hands_out_its_value_once() {
            static CELL: TakeOnce<u32> = TakeOnce::new(7);
            assert_eq!(CELL.take().map(|value| *value), Some(7));
            assert!(CELL.take().is_none());
        }
    }
}
