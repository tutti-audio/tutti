//! [`MidiOut`]: the terminal sink MIDI events are pushed at. The input
//! direction has no trait: a device's events reach the graph through an event
//! inlet ([`InputInlets`](crate::InputInlets)).
//!
//! The sink carries no address: an endpoint *is* its stream. Which node hears
//! what is the graph's wiring (a `MidiInputNode` in `tutti-midi-runtime`
//! sends each channel out of its own event port).

use tutti_graph::UmpAt;

use crate::{Outgoing, Taken};

// -----------------------------------------------------------------------------
// Delivery: write side (out) and read side (in)
// -----------------------------------------------------------------------------

/// A single **terminal sink** that MIDI events are pushed to.
///
/// A sink is already addressed: it *is* the destination (a
/// mailbox, a hardware wire), so `queue` carries no id. What was delivered
/// is read from the sink's own reader.
///
/// Implementations never block: a sink with no room takes less.
pub trait MidiOut: Send + Sync {
    /// Delivers `events`, each to go at its [`SendAt`](crate::SendAt), and
    /// returns what the sink [`Taken`] of them.
    ///
    /// Less than all means the rest were refused: a full ring, a device that
    /// would not take the write. The caller counts them. Dropping a note-off
    /// whose note-on landed is what produces a stuck note, so a caller with
    /// anywhere to report it must.
    ///
    /// What is taken is a **prefix** that ends between two messages, not a
    /// subset and not a cut inside one: a sink that cannot take a message of
    /// several packets (a System Exclusive message) whole takes none of it, and
    /// nothing after it, so the stream stays in order and no receiver is left
    /// waiting for the end of a message. One case can still cut a message: the
    /// ALSA sequencer's destination fifo cannot be read, so a destination that
    /// fills up between two packets of a message takes its head only. For a
    /// direct (`Now`) send that output takes the message, counts the lost tail
    /// in its losses, and does not send the head again. For a scheduled (`At`)
    /// message the overflow happens later, in the sequencer's queue, after the
    /// output has taken the whole message: the tail is lost and nothing counts it.
    ///
    /// A sink with no wire clock (a mailbox, a test's) ignores the time.
    fn queue_timed(&self, events: &[Outgoing]) -> Taken;

    /// [`queue_timed`](Self::queue_timed) for events that go at once.
    ///
    /// The default allocates, so it is for a sink on a control thread. A sink
    /// the audio thread writes to overrides it.
    fn queue(&self, events: &[UmpAt]) -> Taken {
        self.queue_timed(&Outgoing::now_batch(events))
    }
}
