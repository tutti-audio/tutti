//! The pool's jobs: the wait for every declared entry ([`PoolLoaded`]) and the
//! files check ([`RefreshPool`]).

use crate::job::job_kind;
use crate::{BackgroundError, PoolReport, RefreshReport, WaitError};

/// Why a pool job ended without its result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PoolJobError {
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] BackgroundError),
    /// The wait could not run.
    #[error(transparent)]
    Wait(#[from] WaitError),
}

job_kind! {
    /// The wait for every declared entry's outcome
    /// ([`Session::pool_loaded`](crate::Session::pool_loaded)): it lands once
    /// no declared entry is pending, as a [`PoolReport`] of the keys of type
    /// `K`.
    pub PoolLoaded<K> => PoolReport<K>, PoolJobError
}

job_kind! {
    /// The check of the files the pool's entries read
    /// ([`Session::refresh_pool`](crate::Session::refresh_pool)): it lands a
    /// [`RefreshReport`].
    pub RefreshPool => RefreshReport, PoolJobError
}
