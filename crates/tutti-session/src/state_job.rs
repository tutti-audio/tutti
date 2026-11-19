//! The jobs that save and restore a peer's state: [`SaveState`] and
//! [`RestoreState`], and why one failed ([`StateJobFailed`]).

use crate::job::job_kind;
use crate::{PeerState, WaitError, WorkerError};

job_kind! {
    /// A peer's saved state: a plugin's ([`Session::save_state`](crate::Session::save_state))
    /// or a device's ([`ListedPeer::save_state`](crate::ListedPeer::save_state)).
    pub SaveState => PeerState, StateJobFailed
}

job_kind! {
    /// A state put back into a peer: a plugin
    /// ([`Session::restore_state`](crate::Session::restore_state)) or a device
    /// ([`ListedPeer::restore_state`](crate::ListedPeer::restore_state)).
    pub RestoreState => (), StateJobFailed
}

/// How a state job ended without its result.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateJobFailed {
    /// The plugin's own refusal: it is gone, busy, refused, has no state route,
    /// or an in-process save the main thread did not run in time
    /// ([`StateError::Unavailable`](tutti_plugin::error::StateError::Unavailable)).
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Plugin(#[from] tutti_plugin::error::StateError),
    /// The device's failure: its Property Exchange call for the State resource
    /// ended without the state, or no call was made.
    #[cfg(feature = "midi-hardware")]
    #[error(transparent)]
    Device(#[from] crate::PeerError<crate::PeFailure>),
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The wait could not run ([`Job::wait`](crate::Job::wait)).
    #[error(transparent)]
    Wait(#[from] WaitError),
}
