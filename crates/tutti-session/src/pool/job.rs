//! The pool's jobs: the wait for every declared entry ([`PoolLoaded`]).

use crate::job::job_kind;
use crate::{PoolReport, WaitError, WorkerError};

/// Why a pool job ended without its result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PoolJobError {
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] WorkerError),
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
