//! [`LockExt`]: a `Mutex`'s guard taken whether or not a holder panicked.
//!
//! A `std::sync::Mutex` is poisoned when a thread panics while holding it, and
//! every later `lock` answers `Err` carrying the guard. A panic is reported
//! where it is caught (a thread's [`Panicked`](crate::Panicked), a plugin's
//! contained call), not through the lock, and the state behind a lock is kept
//! valid at every store a holder can panic between. So a lock in tutti takes the
//! guard either way, and [`LockExt::lock_recover`] is the one spelling for it.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Takes a `Mutex`'s guard whether or not a panicking holder poisoned it.
pub trait LockExt<T: ?Sized>: sealed::Sealed {
    /// Blocks until the lock is free and returns its guard; a poisoned lock
    /// answers its guard as an unpoisoned one does.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    #[inline]
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

mod sealed {
    pub trait Sealed {}
    impl<T: ?Sized> Sealed for std::sync::Mutex<T> {}
}

#[cfg(test)]
mod tests;
