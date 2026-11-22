//! The blocking face: [`Mutex`], [`RwLock`] and [`Condvar`].
//!
//! Each wraps std's and asserts, in a debug build, that it is not called inside
//! an [`RtScope`] before it waits, so a block that takes a lock
//! fails every realtime test that reaches it, as an allocation does under
//! `RtAllocCheck`. A poisoned lock answers its guard: the caller keeps the
//! locked state valid wherever a holder can panic, and the guard does not
//! repair state a panic left half-written.
//!
//! The wrapped types are std's under every cfg: a loom model that needs loom's
//! `Mutex` names it itself.

use std::fmt;
use std::sync::{
    self, MutexGuard, PoisonError, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult,
};
use std::time::Duration;

use crate::RtScope;

/// A mutual-exclusion lock whose [`lock`](Self::lock) refuses to wait inside an
/// audio block.
///
/// Wraps [`std::sync::Mutex`] and hands out its [`MutexGuard`], so
/// [`std::sync::Condvar`] works with it. Unlike std's, `lock` never answers a
/// poison error: it returns the guard of a lock whose holder panicked.
///
/// # Panics
///
/// [`lock`](Self::lock) panics, in a debug build, inside an
/// [`RtScope`]. [`try_lock`](Self::try_lock) never waits and
/// never panics for that reason.
///
/// # Examples
///
/// ```
/// use tutti_rt::Mutex;
///
/// let counter = Mutex::new(0);
/// *counter.lock() += 1;
/// assert_eq!(counter.into_inner(), 1);
/// ```
#[derive(Default)]
pub struct Mutex<T: ?Sized>(sync::Mutex<T>);

impl<T> Mutex<T> {
    /// A lock holding `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(sync::Mutex::new(value))
    }

    /// Consumes the lock and returns its value, whether or not a holder
    /// panicked.
    pub fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Waits until the lock is free and returns its guard; a poisoned lock
    /// answers its guard as an unpoisoned one does.
    ///
    /// # Panics
    ///
    /// In a debug build, inside an [`RtScope`].
    pub fn lock(&self) -> MutexGuard<'_, T> {
        RtScope::check_not_blocking("Mutex::lock");
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The guard if the lock is free now, `None` if another thread holds it.
    /// Never waits, so an audio block may call it; a poisoned lock answers its
    /// guard.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(sync::TryLockError::WouldBlock) => None,
        }
    }

    /// The value, borrowed through the exclusive borrow of the lock: no lock is
    /// taken.
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A reader-writer lock whose [`read`](Self::read) and [`write`](Self::write)
/// refuse to wait inside an audio block.
///
/// Wraps [`std::sync::RwLock`] and hands out its guards. Unlike std's, neither
/// call answers a poison error: each returns the guard of a lock whose holder
/// panicked.
///
/// # Panics
///
/// [`read`](Self::read) and [`write`](Self::write) panic, in a debug build,
/// inside an [`RtScope`].
///
/// # Examples
///
/// ```
/// use tutti_rt::RwLock;
///
/// let live = RwLock::new(true);
/// *live.write() = false;
/// assert!(!*live.read());
/// ```
pub struct RwLock<T: ?Sized>(sync::RwLock<T>);

impl<T> RwLock<T> {
    /// A lock holding `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(sync::RwLock::new(value))
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Waits until no writer holds the lock and returns a shared guard; a
    /// poisoned lock answers its guard as an unpoisoned one does.
    ///
    /// # Panics
    ///
    /// In a debug build, inside an [`RtScope`].
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        RtScope::check_not_blocking("RwLock::read");
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until no one holds the lock and returns the exclusive guard; a
    /// poisoned lock answers its guard as an unpoisoned one does.
    ///
    /// # Panics
    ///
    /// In a debug build, inside an [`RtScope`].
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        RtScope::check_not_blocking("RwLock::write");
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A condition variable whose waits refuse to run inside an audio block.
///
/// Wraps [`std::sync::Condvar`] over a [`Mutex`]'s guard. Each wait first
/// checks that it is outside an [`RtScope`], and returns the
/// guard of a lock whose holder panicked as it does any other. Notifying never
/// waits and is allowed anywhere.
///
/// # Panics
///
/// Every `wait` method panics, in a debug build, inside an
/// [`RtScope`].
///
/// # Examples
///
/// ```
/// use tutti_rt::{Condvar, Mutex};
///
/// let ready = Mutex::new(true);
/// let signal = Condvar::new();
/// let guard = signal.wait_while(ready.lock(), |ready| !*ready);
/// assert!(*guard);
/// ```
#[derive(Default)]
pub struct Condvar(sync::Condvar);

impl Condvar {
    /// A condition variable no thread waits on.
    #[must_use]
    pub const fn new() -> Self {
        Self(sync::Condvar::new())
    }

    /// Releases `guard`'s lock, waits for a notification, and returns the
    /// guard of the relocked mutex. May wake without one.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        RtScope::check_not_blocking("Condvar::wait");
        self.0.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, as [`wait`](Self::wait), until `condition` is false.
    pub fn wait_while<'a, T, F>(&self, guard: MutexGuard<'a, T>, condition: F) -> MutexGuard<'a, T>
    where
        F: FnMut(&mut T) -> bool,
    {
        RtScope::check_not_blocking("Condvar::wait_while");
        self.0
            .wait_while(guard, condition)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, as [`wait`](Self::wait), for at most `timeout`; the result says
    /// whether it elapsed.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        RtScope::check_not_blocking("Condvar::wait_timeout");
        self.0
            .wait_timeout(guard, timeout)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, as [`wait_while`](Self::wait_while), for at most `timeout`; the
    /// result says whether it elapsed with `condition` still true.
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        condition: F,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult)
    where
        F: FnMut(&mut T) -> bool,
    {
        RtScope::check_not_blocking("Condvar::wait_timeout_while");
        self.0
            .wait_timeout_while(guard, timeout, condition)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes one waiting thread, if any.
    pub fn notify_one(&self) {
        self.0.notify_one();
    }

    /// Wakes every waiting thread.
    pub fn notify_all(&self) {
        self.0.notify_all();
    }
}

impl fmt::Debug for Condvar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests;
