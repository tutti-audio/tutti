//! Resources and peers: what a token reads, and the one notice and error a
//! peer has.
//!
//! A *resource* is something the session holds for the app and names by a
//! token: a loaded wave, a MIDI port, a hosted plugin. A *peer* is a resource
//! with something live behind it that can change on its own, go away, come
//! back and be asked things (a port's device, a capture's audio device, a
//! plugin). Whatever its kind, a peer is read through [`Peer::info`] and
//! [`Peer::presence`], told about through one
//! [`SessionEvent::ResourceChanged`](crate::SessionEvent::ResourceChanged), and
//! asked with requests that fail as [`PeerError`].
//!
//! Only this crate implements [`Resource`]: a token is the session's, minted
//! with an id no other token of the session has.

use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::{WaitError, WorkerError};

/// A resource's identity in one session: unique among every token the session
/// has handed out, and never reused.
///
/// A [`SessionEvent::ResourceChanged`](crate::SessionEvent::ResourceChanged)
/// names the resource it is about by this id; the token's [`Resource::id`]
/// matches it. An id means nothing in another session, and an app cannot make
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceId(u64);

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resource {}", self.0)
    }
}

/// The session's source of [`ResourceId`]s: one counter, from 1. A clone
/// draws from the same counter, so a pool entry that mints when its load lands
/// (off the session's own call) takes ids no other token has.
#[derive(Clone, Debug)]
pub(crate) struct ResourceIds {
    #[cfg_attr(
        not(any(test, feature = "plugin", feature = "midi")),
        expect(
            dead_code,
            reason = "the ids of plugins and MIDI ports (features `plugin`, `midi`)"
        )
    )]
    last: Arc<tutti_rt::Mint>,
}

impl ResourceIds {
    pub(crate) fn new() -> Self {
        Self {
            last: Arc::new(tutti_rt::Mint::new()),
        }
    }

    /// The next id: distinct from every one this source (and its clones)
    /// returned before.
    #[cfg_attr(
        not(any(test, feature = "plugin", feature = "midi")),
        expect(
            dead_code,
            reason = "the ids of plugins and MIDI ports (features `plugin`, `midi`)"
        )
    )]
    pub(crate) fn mint(&self) -> ResourceId {
        ResourceId(self.last.mint().get())
    }
}

pub(crate) mod sealed {
    /// Names the types allowed to be a [`Resource`](super::Resource): the
    /// tokens of this crate.
    pub trait Sealed {}
}

/// What the session holds for the app and names by a token.
///
/// Sealed: the tokens are this crate's. A wave, an impulse response or a
/// SoundFont is a `Resource` only; a resource with something live behind it is
/// also a [`Peer`].
pub trait Resource: sealed::Sealed {
    /// The token's identity, the one a
    /// [`SessionEvent::ResourceChanged`](crate::SessionEvent::ResourceChanged)
    /// carries.
    fn id(&self) -> ResourceId;
}

/// A [`Resource`] with something live behind it: it can change on its own, go
/// away, come back, and be asked things.
///
/// Both readings are the session's last known, taken on the control thread.
/// Neither asks the peer, so neither blocks.
pub trait Peer: Resource {
    /// What the peer says of itself: one snapshot.
    type Info;
    /// Why the peer is not live.
    type Cause;

    /// The last known snapshot of what the peer says of itself. Never blocks.
    fn info(&self) -> Arc<Self::Info>;

    /// Whether the peer is there. Never blocks.
    fn presence(&self) -> Presence<Self::Cause>;
}

/// Whether a [`Peer`] is there, and when it is not, why.
///
/// A change of presence is noticed once, as
/// [`ResourceChanged`](crate::SessionEvent::ResourceChanged) with
/// [`ResourceFact::Presence`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence<C> {
    /// There and answering.
    Live,
    /// There, but not answering: an out-of-process plugin only.
    Stalled,
    /// Gone, and the session is bringing it back: this is the `attempt`th
    /// try, and `cause` is why it went (or why the last try failed).
    Recovering {
        /// Which try this is, from 1.
        attempt: u32,
        /// Why it is not live.
        cause: C,
    },
    /// Gone; the cause says whether it can return.
    Lost(C),
    /// Its pool entry was released, or swapped to another identity: final,
    /// this handle never returns. It has no cause, since nothing about it can
    /// change.
    Closed,
}

impl<C> Presence<C> {
    /// Whether the peer is there and answering.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }

    /// Why the peer is not live, for the two readings that have a cause.
    #[must_use]
    pub const fn cause(&self) -> Option<&C> {
        match self {
            Self::Recovering { cause, .. } | Self::Lost(cause) => Some(cause),
            Self::Live | Self::Stalled | Self::Closed => None,
        }
    }
}

/// Which reading of a peer a
/// [`ResourceChanged`](crate::SessionEvent::ResourceChanged) says changed:
/// read it again. The notice carries no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceFact {
    /// [`Peer::presence`].
    Presence,
    /// [`Peer::info`].
    Info,
    /// A plugin's editor asked for a size.
    Editor,
}

/// Why a request to a [`Peer`] failed. `E` is the request's own refusal.
///
/// A request that returns a [`Job`](crate::Job) and is made of a peer that is
/// `Lost` fails at the call: the job is already ended with
/// [`Disconnected`](Self::Disconnected), and lands in the next update.
///
/// # Examples
///
/// ```
/// use tutti_session::PeerError;
///
/// let nak: PeerError<&str> = PeerError::Refused("not supported");
/// assert_eq!(nak.to_string(), "the peer refused the request: not supported");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PeerError<E> {
    /// The peer went away before the request landed, or the session the
    /// request was queued on is gone.
    #[error("the peer went away before the request landed")]
    Disconnected,
    /// The peer did not answer in time.
    #[error("the peer did not answer in time")]
    TimedOut,
    /// The peer said it does not support the request.
    #[error("the peer does not support the request")]
    Unsupported,
    /// The peer was released before the request landed: its pool entry was
    /// released or swapped to another identity (its handle reads
    /// [`Presence::Closed`]), or every handle of its MIDI port was dropped.
    #[error("the peer was released")]
    Released,
    /// The peer answered, and the answer is not success.
    #[error("the peer refused the request: {0}")]
    Refused(E),
    /// The job's work ended without an answer (the session ended it, or it
    /// panicked).
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// [`Job::wait`](crate::Job::wait) could not wait for the answer.
    #[error(transparent)]
    Wait(#[from] WaitError),
}

#[cfg(test)]
mod tests;
