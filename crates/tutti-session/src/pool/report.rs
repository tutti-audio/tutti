//! The pool's settled outcome, and the handle that waits for it.

use std::fmt;
use std::sync::{Arc, Mutex};

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

/// Where the report is.
enum Cell<K> {
    Pending,
    Landed(PoolReport<K>),
    Taken,
}

/// The cell, whatever a panicking holder left: it only ever holds a whole value.
fn lock<K>(cell: &Mutex<Cell<K>>) -> std::sync::MutexGuard<'_, Cell<K>> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The wait for every declared entry's outcome: [`Session::pool_loaded`].
///
/// It lands once, in an [`update`](Session::update): the app calls `update`
/// until [`has_landed`](Self::has_landed), then [`take`](Self::take)s the
/// report. Dropping it ends the wait.
#[must_use = "a pool_loaded wait reports nothing until it is taken"]
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

    /// The [`PoolLoaded`] was dropped: nothing will read what lands.
    pub(crate) fn orphaned(&self) -> bool {
        Arc::strong_count(&self.cell) == 1
    }
}
