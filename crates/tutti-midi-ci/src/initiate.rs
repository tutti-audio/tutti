//! [`Initiator`]: what the Initiators of MIDI-CI categories have in common.

use tutti_platform::Instant;

use super::Decoded;

/// The Initiator of one MIDI-CI category toward one peer, a sans-IO state machine.
///
/// A driver that holds one per peer hands each CI message of the connection to
/// [`on_message`](Self::on_message), which keeps or ignores it, and calls
/// [`on_timeout`](Self::on_timeout) once the [`deadline`](Self::deadline) has
/// passed. A wait that runs out ends as an action of its own, so a peer that
/// refused, a peer that did not answer in time and a peer that is gone are three
/// different actions, the last of which is the driver's to make by dropping the
/// Initiator.
pub trait Initiator {
    /// What the Initiator asks its driver to do: a message to send or a change
    /// to tell.
    type Action;

    /// When [`on_timeout`](Self::on_timeout) is next needed: `None` while
    /// nothing waits.
    #[must_use]
    fn deadline(&self) -> Option<Instant>;

    /// A CI message of the connection's Group arrived.
    fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<Self::Action>;

    /// Time has passed: every wait that has run out ends. Calling it early is
    /// harmless: a wait that has not run out is kept.
    fn on_timeout(&mut self, now: Instant) -> Vec<Self::Action>;
}
