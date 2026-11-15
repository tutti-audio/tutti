//! The pool's settled outcome, and the handle that waits for it.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tutti_rt::MainThreadMarker;

use crate::Session;
use crate::pool::entry::LoadError;

/// How one declared entry ended its load.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PoolOutcome {
    /// The entry is loaded.
    Loaded,
    /// The entry's load failed.
    Failed(LoadError),
}

/// Every declared entry's outcome, once none is pending: what
/// [`Session::pool_loaded`] lands.
///
/// The entries are in the order the pool declared them.
#[derive(Clone, Debug)]
pub struct PoolReport<K> {
    entries: Vec<(K, PoolOutcome)>,
}

impl<K> PoolReport<K> {
    pub(crate) const fn new(entries: Vec<(K, PoolOutcome)>) -> Self {
        Self { entries }
    }

    /// The entries and their outcomes, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &(K, PoolOutcome)> {
        self.entries.iter()
    }

    /// How many entries the report names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pool declared no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The outcome of the entry under `key`, `None` when it is not declared.
    #[must_use]
    pub fn outcome(&self, key: &K) -> Option<&PoolOutcome>
    where
        K: PartialEq,
    {
        self.entries
            .iter()
            .find(|(declared, _)| declared == key)
            .map(|(_, outcome)| outcome)
    }

    /// The entries whose load failed, with why.
    pub fn failures(&self) -> impl Iterator<Item = (&K, &LoadError)> {
        self.entries
            .iter()
            .filter_map(|(key, outcome)| match outcome {
                PoolOutcome::Failed(error) => Some((key, error)),
                PoolOutcome::Loaded => None,
            })
    }

    /// Whether every entry loaded.
    #[must_use]
    pub fn all_loaded(&self) -> bool {
        self.failures().next().is_none()
    }
}

/// Why [`PoolLoaded::wait`] did not return a report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PoolWaitError {
    /// An entry waits for its main-thread stage, and the calling thread is not
    /// the main thread, so no update here can run it. The wait is still
    /// pending: call it again from the main thread.
    #[error("an entry waits for the main thread, and this is not it")]
    NeedsMainThread,
    /// The report was already taken.
    #[error("the pool report was already taken")]
    Taken,
}

/// The cell, whatever a panicking holder left: it only ever holds a whole value.
fn lock<K>(cell: &Mutex<Cell<K>>) -> std::sync::MutexGuard<'_, Cell<K>> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where the report is.
enum Cell<K> {
    Pending,
    Landed(PoolReport<K>),
    Taken,
}

/// The wait for every declared entry's outcome: [`Session::pool_loaded`].
///
/// It lands once. [`take`](Self::take) reads it after an
/// [`update`](Session::update); [`wait`](Self::wait) updates until it lands.
/// Dropping it cancels nothing: it only reads.
#[must_use = "a pool_loaded wait reports nothing until it is taken or waited on"]
pub struct PoolLoaded<K> {
    cell: Arc<Mutex<Cell<K>>>,
}

impl<K> PoolLoaded<K> {
    pub(crate) fn new() -> (Self, Landing<K>) {
        let cell = Arc::new(Mutex::new(Cell::Pending));
        (
            Self {
                cell: Arc::clone(&cell),
            },
            Landing { cell },
        )
    }

    /// The report if it has landed and was not taken, `None` before it lands.
    /// A later call answers `None` too.
    pub fn take(&self) -> Option<PoolReport<K>> {
        let mut cell = lock(&self.cell);
        match std::mem::replace(&mut *cell, Cell::Taken) {
            Cell::Landed(report) => Some(report),
            Cell::Pending => {
                *cell = Cell::Pending;
                None
            }
            Cell::Taken => None,
        }
    }

    /// Whether the report landed and was not taken yet.
    #[must_use]
    pub fn has_landed(&self) -> bool {
        matches!(*lock(&self.cell), Cell::Landed(_))
    }

    /// Calls [`Session::update`] until the report lands, and returns it. The
    /// events of those updates stay queued: the app's next `update` returns
    /// them first, in the order they came.
    ///
    /// It runs an entry's main-thread stage only when called on the main
    /// thread, as `update` does.
    ///
    /// # Errors
    ///
    /// [`PoolWaitError::NeedsMainThread`] when an entry waits for its
    /// main-thread stage and this is not the main thread;
    /// [`PoolWaitError::Taken`] when the report was already taken.
    pub fn wait(&self, session: &mut Session) -> Result<PoolReport<K>, PoolWaitError> {
        loop {
            if let Some(report) = self.take() {
                return Ok(report);
            }
            if matches!(*lock(&self.cell), Cell::Taken) {
                return Err(PoolWaitError::Taken);
            }
            let events = session.update();
            session.pool.defer_events(events);
            if self.has_landed() {
                continue;
            }
            if session.pool.awaits_main() && MainThreadMarker::new().is_none() {
                return Err(PoolWaitError::NeedsMainThread);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl<K> fmt::Debug for PoolLoaded<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolLoaded").finish_non_exhaustive()
    }
}

/// The pool's end of a [`PoolLoaded`].
pub(crate) struct Landing<K> {
    cell: Arc<Mutex<Cell<K>>>,
}

impl<K> Landing<K> {
    pub(crate) fn land(&self, report: PoolReport<K>) {
        *lock(&self.cell) = Cell::Landed(report);
    }
}
