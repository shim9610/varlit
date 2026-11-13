//! A mutual-exclusion cell that never allocates.
//!
//! `std::sync::Mutex` is futex-backed on Linux, but on Apple platforms it
//! boxes a `pthread_mutex_t` the first time it is locked, with an infallible
//! allocation. A reader opened while allocations are refused (the fallible
//! ownership contract) would abort there instead of returning
//! `Error::AllocationFailed`. This lock keeps its whole state inline.
//!
//! It guards a tile reader's cache. Handles are only shared across threads
//! through a mapped matrix, and those critical sections are short, so a
//! waiter spins briefly and then yields its time slice.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

/// The guarded value was left by a panic while locked.
#[derive(Debug)]
pub(crate) struct Poisoned;

pub(crate) struct ReaderLock<T> {
    locked: AtomicBool,
    poisoned: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through a guard, and `locked` admits one
// guard at a time; moving `T` between threads needs `T: Send`.
unsafe impl<T: Send> Send for ReaderLock<T> {}
// SAFETY: as above, a shared `ReaderLock` hands out one exclusive guard at a
// time, so sharing it needs only `T: Send`, as with `std::sync::Mutex`.
unsafe impl<T: Send> Sync for ReaderLock<T> {}

impl<T> ReaderLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// The guard if nothing holds the lock (and it is not poisoned), without
    /// waiting: for a buffer a caller can do without.
    pub(crate) fn try_lock(&self) -> Option<ReaderGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        let guard = ReaderGuard { lock: self };
        (!self.poisoned.load(Ordering::Relaxed)).then_some(guard)
    }

    pub(crate) fn lock(&self) -> Result<ReaderGuard<'_, T>, Poisoned> {
        let mut spins = 0u32;
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if spins < 64 {
                spins += 1;
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        let guard = ReaderGuard { lock: self };
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(Poisoned);
        }
        Ok(guard)
    }

    pub(crate) fn get_mut(&mut self) -> Result<&mut T, Poisoned> {
        if *self.poisoned.get_mut() {
            return Err(Poisoned);
        }
        Ok(self.value.get_mut())
    }
}

pub(crate) struct ReaderGuard<'a, T> {
    lock: &'a ReaderLock<T>,
}

impl<T> Deref for ReaderGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this guard is the only one alive while `locked` is set.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for ReaderGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: this guard is the only one alive while `locked` is set.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for ReaderGuard<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.lock.poisoned.store(true, Ordering::Relaxed);
        }
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn threads_take_turns_and_every_increment_lands() {
        let lock = Arc::new(ReaderLock::new(0u64));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        *lock.lock().unwrap() += 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*lock.lock().unwrap(), 40_000);
    }

    #[test]
    fn try_lock_takes_a_free_lock_and_never_waits() {
        let lock = ReaderLock::new(5u64);
        let held = lock.try_lock().expect("free");
        assert!(lock.try_lock().is_none());
        drop(held);
        *lock.try_lock().expect("free again") += 1;
        assert_eq!(*lock.lock().unwrap(), 6);
    }

    #[test]
    fn a_panic_while_locked_poisons_the_lock() {
        let lock = Arc::new(ReaderLock::new(1u64));
        let shared = Arc::clone(&lock);
        let joined = std::thread::spawn(move || {
            let _guard = shared.lock().unwrap();
            panic!("left while locked");
        })
        .join();
        assert!(joined.is_err());
        assert!(lock.lock().is_err());
        // The lock itself was released: a second attempt does not hang.
        assert!(lock.lock().is_err());
        assert!(lock.try_lock().is_none());
        let mut lock = Arc::try_unwrap(lock).ok().unwrap();
        assert!(lock.get_mut().is_err());
    }
}
