//! [`Taken`]: how much of a batch a sink took, always a whole number of
//! messages.

use tutti_ump::{Span, Ump, spans};

use crate::Outgoing;

/// How many packets of a batch a sink took: a prefix of the batch that ends
/// between two messages, so a message of several packets (a System Exclusive
/// message, a text) went whole or not at all.
///
/// The value is made only by the sink's own loop over the batch's messages
/// ([`through`](Self::through)) or by a count of room cut back to a message
/// boundary ([`within`](Self::within)), so a count that falls inside a message
/// cannot be written. What the sink did not take, the caller counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Taken(usize);

impl Taken {
    /// Nothing taken.
    pub const NONE: Self = Self(0);

    /// The longest prefix of `events` of at most `room` packets that ends
    /// between two messages. A message that does not fit whole is not taken,
    /// nor is anything after it.
    #[must_use]
    pub fn within(events: &[Outgoing], room: usize) -> Self {
        Self::within_by(events, |event| event.ump, room)
    }

    /// [`within`](Self::within) for items that carry a packet: `ump` reads it.
    #[must_use]
    pub fn within_by<T>(items: &[T], ump: impl Fn(&T) -> Ump, room: usize) -> Self {
        let mut taken = Self::NONE;
        for span in spans(items, ump) {
            if taken.0 + span.len() > room {
                break;
            }
            taken = taken.through(span);
        }
        taken
    }

    /// This and the packets of `message`, the next message of the batch after
    /// what is taken. A sink that sends one message at a time adds each as it
    /// goes and stops at the first it cannot send.
    #[must_use]
    pub const fn through<T>(self, message: Span<'_, T>) -> Self {
        Self(self.0 + message.len())
    }

    /// How many packets of the batch were taken.
    #[must_use]
    pub const fn packets(self) -> usize {
        self.0
    }

    /// Whether nothing was taken.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Whether the whole of `batch` was taken.
    #[must_use]
    pub const fn is_all<T>(self, batch: &[T]) -> bool {
        self.0 == batch.len()
    }

    /// How many packets of `batch` were left.
    #[must_use]
    pub const fn left_of<T>(self, batch: &[T]) -> usize {
        batch.len() - self.0
    }
}

#[cfg(test)]
mod tests;
