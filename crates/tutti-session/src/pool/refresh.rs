//! `refresh_pool`: the files' stamps checked on a worker.

use std::io;
use std::path::PathBuf;

use crate::pool::key::PoolKey;
use crate::pool::stamp::Stamp;

/// What [`Session::refresh_pool`](crate::Session::refresh_pool) found: the
/// entries it loaded again and the ones whose file it could not read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefreshPool {
    pub(crate) reloaded: Vec<PoolKey>,
    pub(crate) unreadable: Vec<(PoolKey, io::ErrorKind)>,
}

impl RefreshPool {
    /// The entries whose file changed and that load again, in key order; each
    /// lands as a [`PoolEvent::Reloaded`](crate::PoolEvent::Reloaded), or
    /// [`Failed`](crate::PoolEvent::Failed).
    #[must_use]
    pub fn reloaded(&self) -> &[PoolKey] {
        &self.reloaded
    }

    /// The entries whose file could not be read (removed, unreadable), with
    /// why. They keep the value they have.
    #[must_use]
    pub fn unreadable(&self) -> &[(PoolKey, io::ErrorKind)] {
        &self.unreadable
    }
}

/// One entry's file, checked: the stamp it was read under (none when the stat
/// failed at its load) and the stamp now.
pub(crate) struct Checked {
    pub(crate) key: PoolKey,
    pub(crate) was: Option<Stamp>,
    pub(crate) now: Result<Stamp, io::ErrorKind>,
}

/// Takes each file's stamp again. This is the refresh's worker stage.
pub(crate) fn check(files: Vec<(PoolKey, PathBuf, Option<Stamp>)>) -> Vec<Checked> {
    files
        .into_iter()
        .map(|(key, path, was)| Checked {
            key,
            was,
            now: Stamp::of(&path).map_err(|error| error.kind()),
        })
        .collect()
}
