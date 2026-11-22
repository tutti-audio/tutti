//! [`Handshake`]: where an Initiator's opening inquiry stands.

use std::time::Duration;

use tutti_platform::Instant;

use crate::deadline::Deadline;

/// The opening inquiry of an Initiator with one peer (Profile Inquiry, Process
/// Inquiry Capabilities, Property Exchange Capabilities): not answered, waiting
/// for the answer, or answered.
///
/// `A` is what the Initiator keeps while it waits (Profile Replies heard so far)
/// and `K` what the answer gives. Asking again keeps the answer until the new
/// one arrives, so what the peer said before still holds while it is asked; a
/// refusal or a timeout of the new inquiry drops it.
#[derive(Clone, Copy, Debug)]
pub(super) enum Handshake<K, A = ()> {
    /// Not asked, or the wait ended with a refusal or a timeout.
    Unanswered,
    /// The inquiry is out; the answer is not in.
    Asking {
        /// When the wait ends.
        wait: Deadline,
        /// What the wait has collected.
        progress: A,
        /// The answer the peer gave before this inquiry, if it did.
        previous: Option<K>,
    },
    /// The peer answered.
    Answered(K),
}

impl<K, A> Handshake<K, A> {
    /// The inquiry goes out at `now` and waits `wait`.
    pub(super) fn ask(&mut self, now: Instant, wait: Duration, progress: A) {
        let previous = match std::mem::replace(self, Self::Unanswered) {
            Self::Answered(answer) => Some(answer),
            Self::Asking { previous, .. } => previous,
            Self::Unanswered => None,
        };
        *self = Self::Asking {
            wait: Deadline::after(now, wait),
            progress,
            previous,
        };
    }

    /// The answer, once it has come: while a new inquiry waits, the one before.
    pub(super) fn answer(&self) -> Option<&K> {
        match self {
            Self::Answered(answer) => Some(answer),
            Self::Asking { previous, .. } => previous.as_ref(),
            Self::Unanswered => None,
        }
    }

    /// The wait, while there is one.
    pub(super) fn wait(&self) -> Option<Deadline> {
        match self {
            Self::Asking { wait, .. } => Some(*wait),
            _ => None,
        }
    }

    /// The wait, to move its deadline.
    pub(super) fn wait_mut(&mut self) -> Option<&mut Deadline> {
        match self {
            Self::Asking { wait, .. } => Some(wait),
            _ => None,
        }
    }

    /// What the wait has collected, while there is one.
    pub(super) fn progress_mut(&mut self) -> Option<&mut A> {
        match self {
            Self::Asking { progress, .. } => Some(progress),
            _ => None,
        }
    }

    /// Whether the inquiry is waiting.
    pub(super) fn is_asking(&self) -> bool {
        matches!(self, Self::Asking { .. })
    }

    /// The wait ends without an answer when `now` is at its deadline; whether it did.
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if matches!(self, Self::Asking { wait, .. } if wait.expired(now)) {
            *self = Self::Unanswered;
            true
        } else {
            false
        }
    }

    /// The wait ends without an answer; whether one was running.
    pub(super) fn refuse(&mut self) -> bool {
        let was = self.is_asking();
        if was {
            *self = Self::Unanswered;
        }
        was
    }
}
