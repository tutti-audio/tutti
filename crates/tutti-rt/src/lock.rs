//! [`LockExt`]: a `Mutex`'s guard taken whether or not a holder panicked.
//!
//! A `std::sync::Mutex` is poisoned when a thread panics while holding it, and
//! every later `lock` answers `Err` carrying the guard. A lock in tutti takes
//! the guard either way, and [`LockExt::lock_recover`] is the one spelling for
//! it: a caller that locks this way keeps the locked state valid at every point
//! where its holder can panic, so a guard taken after a panic reads valid state.

#![allow(clippy::disallowed_types, reason = "`LockExt` extends std's `Mutex`")]

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Takes a `Mutex`'s guard whether or not a panicking holder poisoned it.
pub trait LockExt<T: ?Sized>: sealed::Sealed {
    /// Blocks until the lock is free and returns its guard; a poisoned lock
    /// answers its guard as an unpoisoned one does. The caller keeps the
    /// locked state valid wherever a holder can panic: the guard does not
    /// repair state a panic left half-written.
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
