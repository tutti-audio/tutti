//! Shared state a test node locks on the audio thread.

use std::sync::{Arc, LockResult, Mutex, MutexGuard};

/// Shared test state a test node may lock inside `process`, and the test reads
/// afterwards.
///
/// The mutex is taken once when the value is built, on the control side: the
/// first `lock` of a std mutex allocates on macOS (its pthread mutex is boxed
/// lazily), which the armed allocation check refuses inside a block, so a first
/// lock in `process` would abort there. Clones share one value.
#[derive(Debug)]
pub struct Shared<T>(Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Shared<T> {
    /// Shares `value`, its mutex already taken once.
    ///
    /// # Panics
    ///
    /// Never in practice: the fresh mutex is not poisoned.
    #[must_use]
    pub fn new(value: T) -> Self {
        let shared = Arc::new(Mutex::new(value));
        drop(shared.lock().expect("a fresh mutex is not poisoned"));
        Self(shared)
    }

    /// The value, locked.
    ///
    /// # Errors
    ///
    /// When the lock is poisoned.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }
}
