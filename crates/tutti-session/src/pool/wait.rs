//! A wait for something the session lands in an update: the cell its handle
//! reads and the end the session writes.

use std::fmt;
use std::sync::{Arc, Mutex};

/// Where the result is.
enum Cell<T> {
    Pending,
    Landed(T),
    Taken,
}

/// The cell, whatever a panicking holder left: it only ever holds a whole value.
fn lock<T>(cell: &Mutex<Cell<T>>) -> std::sync::MutexGuard<'_, Cell<T>> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The wait for a result the session lands once, in an
/// [`update`](crate::Session::update): the app calls `update` until
/// [`has_landed`](Self::has_landed), then [`take`](Self::take)s it. Dropping
/// it ends the wait.
///
/// [`Session::pool_loaded`](crate::Session::pool_loaded) and
/// [`Session::refresh_pool`](crate::Session::refresh_pool) return one.
#[must_use = "a wait reports nothing until it is taken"]
pub struct PoolWait<T> {
    cell: Arc<Mutex<Cell<T>>>,
}

impl<T> PoolWait<T> {
    pub(crate) fn new() -> (Self, Landing<T>) {
        let cell = Arc::new(Mutex::new(Cell::Pending));
        (
            Self {
                cell: Arc::clone(&cell),
            },
            Landing { cell },
        )
    }

    /// The result if it has landed and was not taken, `None` before it lands.
    /// A later call answers `None` too.
    pub fn take(&self) -> Option<T> {
        let mut cell = lock(&self.cell);
        match std::mem::replace(&mut *cell, Cell::Taken) {
            Cell::Landed(result) => Some(result),
            Cell::Pending => {
                *cell = Cell::Pending;
                None
            }
            Cell::Taken => None,
        }
    }

    /// Whether the result landed and was not taken yet.
    #[must_use]
    pub fn has_landed(&self) -> bool {
        matches!(*lock(&self.cell), Cell::Landed(_))
    }
}

impl<T> fmt::Debug for PoolWait<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolWait").finish_non_exhaustive()
    }
}

/// The session's end of a [`PoolWait`].
pub(crate) struct Landing<T> {
    cell: Arc<Mutex<Cell<T>>>,
}

impl<T> Landing<T> {
    pub(crate) fn land(&self, result: T) {
        *lock(&self.cell) = Cell::Landed(result);
    }

    /// The [`PoolWait`] was dropped: nothing will read what lands.
    pub(crate) fn orphaned(&self) -> bool {
        Arc::strong_count(&self.cell) == 1
    }
}
