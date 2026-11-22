//! A backend's send loop: one message at a time, stopping at the first the OS
//! refuses.
//!
//! ALSA and CoreMIDI take what they send from the OS, one message at a time,
//! and each stops at the first it cannot send: skipping would deliver a
//! note-off whose note-on never left. [`by_message`] is that loop, and what it
//! logs, once.

use tutti_midi_runtime::{Outgoing, Taken};
use tutti_ump::PacketSpan;
use tutti_units::Losses;

/// Why the OS did not take a message, and how much of it went first.
#[derive(Debug)]
pub(super) struct Refused<E> {
    /// What the OS said.
    pub(super) error: E,
    /// How many of the message's packets went before it said so: none for a message
    /// the OS takes whole or not at all.
    pub(super) sent: usize,
}

/// What [`by_message`] did with a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sent {
    /// What went: whole messages, and the message the OS cut, if one.
    pub(super) taken: Taken,
    /// The packets of a cut message that did not go, as [lost](Losses::lost).
    pub(super) losses: Losses,
}

/// Sends the messages of `events` through `send`, in order, until the OS
/// refuses one. A refused message with none of it sent is not taken, and
/// nothing after it; one the OS took part of is taken (its head is out, and
/// sending it again would double it) and its remaining packets are lost.
///
/// `backend` names the OS in the debug log of a refusal.
pub(super) fn by_message<E: std::error::Error>(
    events: &[Outgoing],
    backend: &str,
    mut send: impl FnMut(PacketSpan<'_, Outgoing>) -> Result<(), Refused<E>>,
) -> Sent {
    let mut taken = Taken::NONE;
    for message in Outgoing::messages(events) {
        match send(message) {
            Ok(()) => taken = taken.through(message),
            Err(Refused { error, sent }) => {
                tracing::debug!("{backend} send: {}", crate::error::chain(&error));
                if sent == 0 {
                    return Sent {
                        taken,
                        losses: Losses::NONE,
                    };
                }
                return Sent {
                    taken: taken.through(message),
                    losses: Losses::lost((message.len() - sent) as u64),
                };
            }
        }
    }
    Sent {
        taken,
        losses: Losses::NONE,
    }
}

#[cfg(test)]
mod tests;
