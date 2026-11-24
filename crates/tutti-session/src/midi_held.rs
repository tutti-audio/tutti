//! What an instrument is holding: a fold of the MIDI its event input hears.

use tutti_graph::{EventTapReader, TapEvent};
use tutti_ump::{Held, Ump};
use tutti_units::{Frame, Losses};

/// Events read from the tap per pop.
const POP_CHUNK: usize = 64;

/// The keys and controllers held at an instrument's event input
/// ([`Midi::held`](crate::Midi::held)): the notes down, and the hold pedals and
/// pitch bends off their normal position, of everything the instrument hears
/// there, every source merged (the handle's keys, a clip, a routed device).
///
/// It folds an event tap on the control side, so nothing is copied out of the
/// audio thread but the events the tap carries. [`update`](Self::update) reads
/// the tap dry and returns the set as of the last block the tap was fed.
///
/// A source that is cut ends what it held (the executor sends the Note Offs,
/// the pedal-ups and the centred bends of an edge that goes), and the tap hears
/// those, so a removed source empties its part of the set. A node removed from
/// the graph feeds the tap no more: the set stays as it was.
#[derive(Debug)]
pub struct HeldReader {
    tap: EventTapReader,
    held: Held,
}

impl HeldReader {
    pub(crate) fn new(tap: EventTapReader) -> Self {
        Self {
            tap,
            held: Held::new(),
        }
    }

    /// Folds every event the tap holds into the held set and returns it, with
    /// what the tap lost since the last call.
    ///
    /// **After a loss the set may be wrong**: a lost Note Off leaves its key
    /// down, a lost Note On leaves it up. A non-note-off is refused once the
    /// tap's capacity is held and a Note Off is kept up to
    /// [`note_off_room`](tutti_graph::note_off_room) of it, so a burst that
    /// overflows loses starts before it loses ends. The count is
    /// [`Losses::lost`]; a reading with none is exact.
    pub fn update(&mut self) -> (&Held, Losses) {
        let mut events = [TapEvent {
            frame: Frame::ZERO,
            ump: Ump([0; 4]),
        }; POP_CHUNK];
        loop {
            let n = self.tap.pop_into(&mut events);
            for e in &events[..n] {
                self.held.observe(e.ump);
            }
            if n < POP_CHUNK {
                break;
            }
        }
        (&self.held, Losses::lost(self.tap.take_lost()))
    }

    /// The tap's reader, to close it
    /// (`Graph::taps().close_event(reader.tap().id())`) or reopen it.
    #[must_use]
    pub fn tap(&self) -> &EventTapReader {
        &self.tap
    }
}
