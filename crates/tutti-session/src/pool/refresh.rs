//! `refresh_pool`: the assets' identities checked on a worker.

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

/// One entry's asset, checked: the identity it was read under (none when it
/// had none at its load, a file that could not be stamped) and the identity now.
pub(crate) struct Checked {
    pub(crate) key: PoolKey,
    pub(crate) was: Option<Identity>,
    /// `Ok(None)`: the asset opens and has no identity, so there is nothing to
    /// compare.
    pub(crate) now: Result<Option<Identity>, io::ErrorKind>,
}

/// Takes each asset's identity again. This is the refresh's worker stage.
///
/// An asset with no identity is read by opening it: one that does not open is
/// unreadable, and one that does is not watched.
pub(crate) fn check(assets: Vec<(PoolKey, Asset, Option<Identity>)>) -> Vec<Checked> {
    assets
        .into_iter()
        .map(|(key, asset, was)| Checked {
            key,
            was,
            now: match asset.identity() {
                Some(now) => Ok(Some(now)),
                None => asset.open().map(|_| None).map_err(|error| error.kind()),
            },
        })
        .collect()
}
