//! `refresh_pool`: the assets' identities checked by the app, then applied.

use std::io;
use tutti_platform::{Asset, Identity};

use crate::pool::key::PoolKey;

/// What [`Session::refresh_pool`](crate::Session::refresh_pool) found: the
/// entries it loaded again and the ones whose asset it could not read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub(crate) reloaded: Vec<PoolKey>,
    pub(crate) unreadable: Vec<(PoolKey, io::ErrorKind)>,
}

impl RefreshReport {
    /// The entries whose asset changed and that load again, in key order; each
    /// lands as a [`PoolEvent::Reloaded`](crate::PoolEvent::Reloaded), or
    /// [`Failed`](crate::PoolEvent::Failed).
    #[must_use]
    pub fn reloaded(&self) -> &[PoolKey] {
        &self.reloaded
    }

    /// The entries whose asset could not be read (removed, unreadable), with
    /// why. They keep the value they have.
    #[must_use]
    pub fn unreadable(&self) -> &[(PoolKey, io::ErrorKind)] {
        &self.unreadable
    }
}

/// An entry that reads an asset, as the pool stood when a refresh was cut:
/// the asset, the identity it was read under (none when it had none at its
/// load, a file that could not be stamped) and the slot's generation.
pub(crate) struct Stamped {
    pub(crate) key: PoolKey,
    pub(crate) asset: Asset,
    pub(crate) was: Option<Identity>,
    pub(crate) generation: u64,
}

/// One entry's asset, checked: the identity it was read under, the identity
/// now, and the generation the entry had when the refresh was cut.
pub(crate) struct Checked {
    pub(crate) key: PoolKey,
    pub(crate) was: Option<Identity>,
    /// `Ok(None)`: the asset opens and has no identity, so there is nothing to
    /// compare.
    pub(crate) now: Result<Option<Identity>, io::ErrorKind>,
    pub(crate) generation: u64,
}

/// The check of the assets the pool's entries read, cut from the pool by
/// [`Session::refresh_pool`](crate::Session::refresh_pool).
///
/// It holds each such entry's asset, the identity the entry was read under and
/// the entry's generation, and nothing of the session: run it on any thread,
/// then hand the result to [`Session::apply`](crate::Session::apply).
#[derive(Debug)]
#[must_use = "work does nothing until it runs"]
pub struct RefreshPool {
    entries: Vec<Stamped>,
}

impl RefreshPool {
    pub(crate) const fn new(entries: Vec<Stamped>) -> Self {
        Self { entries }
    }

    /// Takes each asset's identity again, reading the assets and no more: the
    /// session is not touched.
    ///
    /// An asset with no identity is read by opening it: one that does not open
    /// is unreadable, and one that does is not watched.
    pub fn run(self) -> RefreshedPool {
        RefreshedPool {
            checked: self
                .entries
                .into_iter()
                .map(
                    |Stamped {
                         key,
                         asset,
                         was,
                         generation,
                     }| Checked {
                        key,
                        was,
                        now: match asset.identity() {
                            Some(now) => Ok(Some(now)),
                            None => asset.open().map(|_| None).map_err(|error| error.kind()),
                        },
                        generation,
                    },
                )
                .collect(),
        }
    }
}

impl std::fmt::Debug for Stamped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stamped")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// What [`RefreshPool::run`] found: each entry's identity taken again.
///
/// [`Session::apply`](crate::Session::apply) takes it and loads again the
/// entries whose asset changed.
#[derive(Debug)]
#[must_use = "the refresh changes nothing until the session applies it"]
pub struct RefreshedPool {
    pub(crate) checked: Vec<Checked>,
}

impl std::fmt::Debug for Checked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checked")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Why [`Session::apply`](crate::Session::apply) did not apply a
/// [`RefreshedPool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum RefreshError {
    /// An entry the refresh checked was released, declared again or loaded
    /// again since the refresh was cut, so its identities describe an older
    /// state of the pool. Nothing was applied: run the refresh again.
    #[error("an entry the refresh checked changed since it was cut: run the refresh again")]
    Stale,
}
