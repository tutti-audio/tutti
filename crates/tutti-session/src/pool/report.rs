//! The pool's settled outcome, and the handle that waits for it.

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

/// Every declared entry's outcome, once none is pending: the result of
/// [`Session::pool_loaded`](crate::Session::pool_loaded).
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
