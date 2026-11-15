//! [`MidiMailbox`]: a bounded, lock-free ring of MIDI events between one
//! thread and another, split into a [`MidiSender`] and a [`MidiReceiver`].
//!
//! It carries a host's own outbound MIDI to the session's drain. It does not
//! carry MIDI into the graph (that is `tutti-graph`'s event inlet,
//! [`MidiKeys`](crate::MidiKeys)) or out of it (the hardware out reads an
//! event tap). Inside the graph MIDI travels on event ports, never through a
//! mailbox. It goes with the host's outbound path.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_graph::UmpAt;

use crossbeam_queue::ArrayQueue;
use tutti_ump::{Span, Ump, spans};

use crate::traits::MidiOut;
use crate::{Outgoing, Taken};

/// The ring's default capacity, in events.
pub const MAILBOX_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();

/// A bounded MIDI ring. Push and pop are lock-free and never allocate, so
/// either side may be the audio thread.
pub struct MidiMailbox {
    /// Each event with a raw `u32` stamp, not an `Offset`: the ring crosses
    /// threads ahead of any block, and what the stamp means is the reader's
    /// (a host's own events carry a block offset). The types stop here; a
    /// stamp becomes an `Offset` where a block is known.
    events: ArrayQueue<(u32, Ump)>,
    /// Held by the clone that is writing a message, so no other clone puts a
    /// packet between its packets.
    writing: AtomicBool,
}

impl std::fmt::Debug for MidiMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiMailbox")
            .field("events", &self.events.len())
            .finish()
    }
}

impl MidiMailbox {
    /// A ring of [`MAILBOX_CAPACITY`] events, as a sender and a receiver.
    pub fn pair() -> (MidiSender, MidiReceiver) {
        Self::with_capacity(MAILBOX_CAPACITY)
    }

    /// A ring of `capacity` events, as a sender and a receiver.
    #[must_use]
    pub fn with_capacity(capacity: NonZeroUsize) -> (MidiSender, MidiReceiver) {
        let slot = Arc::new(Self {
            events: ArrayQueue::new(capacity.get()),
            writing: AtomicBool::new(false),
        });
        (
            MidiSender {
                slot: Arc::clone(&slot),
            },
            MidiReceiver { slot },
        )
    }
}

/// The push half of a [`MidiMailbox`]. Cheap to clone; every clone pushes into
/// the same ring.
///
/// Implements [`MidiOut`].
#[derive(Clone, Debug)]
pub struct MidiSender {
    slot: Arc<MidiMailbox>,
}

impl MidiSender {
    /// Pushes `events` and returns what the ring [`Taken`]. Less than all means
    /// the ring was full and the rest were dropped: a prefix that ends between
    /// two messages is taken, so the stream stays in order and a message of
    /// several packets is in the ring whole or not at all. Dropping a note-off
    /// whose note-on landed is what leaves a note stuck, so a caller with
    /// anywhere to report it must. Never blocks.
    ///
    /// A message of several packets is written by one clone at a time: a clone
    /// that finds another writing refuses its message and takes nothing, so
    /// two clones never interleave their packets in the ring, and none waits.
    pub fn queue(&self, events: &[UmpAt]) -> Taken {
        self.push(spans(events, |event| event.1), |&(offset, ump)| {
            (offset.get(), ump)
        })
    }

    /// [`queue`](Self::queue) for events stamped with a raw `u32` that is not
    /// an offset into a block (the out node's graph-clock frame, wrapped at
    /// 2^32). Same prefix semantics: what is taken, never a gap.
    pub fn queue_stamped(&self, events: &[(u32, Ump)]) -> Taken {
        self.push(spans(events, |event| event.1), |&event| event)
    }

    /// Pushes the messages of `events` while the ring has room for each and no
    /// other clone is writing.
    fn push<'a, T: 'a>(
        &self,
        messages: impl Iterator<Item = Span<'a, T>>,
        stamped: impl Fn(&T) -> (u32, Ump),
    ) -> Taken {
        let ring = &self.slot.events;
        let mut taken = Taken::NONE;
        for message in messages {
            // One writer at a time, so a message's packets are together in the
            // ring and the room read below is still there when they are
            // written (the reader only adds room). A clone that finds another
            // writing refuses the message: it never waits.
            if self
                .slot
                .writing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                break;
            }
            let fits = ring.capacity() - ring.len() >= message.len();
            if fits {
                for event in message.packets() {
                    // The room was read under the claim: the push has room.
                    let _ = ring.push(stamped(event));
                }
            }
            self.slot.writing.store(false, Ordering::Release);
            if !fits {
                break;
            }
            taken = taken.through(message);
        }
        taken
    }
}

impl MidiOut for MidiSender {
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        self.push(Outgoing::messages(events), |event| {
            (event.at.get(), event.ump)
        })
    }

    fn queue(&self, events: &[UmpAt]) -> Taken {
        self.queue(events)
    }
}

/// The pop half of a [`MidiMailbox`]. One reader at a time: two polling the
/// same ring each take events the other never sees.
#[derive(Debug)]
pub struct MidiReceiver {
    slot: Arc<MidiMailbox>,
}

impl MidiReceiver {
    /// Pop up to `out.len()` events into `out`, oldest first, each with its raw
    /// stamp; return how many.
    pub fn poll_into(&self, out: &mut [(u32, Ump)]) -> usize {
        let mut count = 0;
        for slot in out.iter_mut() {
            match self.slot.events.pop() {
                Some(event) => {
                    *slot = event;
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    /// Drops everything waiting in the ring.
    pub fn clear(&self) {
        while self.slot.events.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests;
