//! What a plugin's output event stream keeps between events.

use tutti_midi_protocol::Translator;
use tutti_ump::{MidiChannel, NoteNumber};

/// The notes a plugin may name in an expression at once. A plugin sounds a few
/// dozen voices; past this the oldest entry is evicted for a new note
/// ([`OutputStream::notes_evicted`]).
const OWN_NOTES: usize = 256;

/// A note the plugin started under an id of its own.
#[derive(Clone, Copy, Debug)]
struct OwnNote {
    /// When it started, in the stream's own count of note starts: the oldest
    /// is the one a full table evicts.
    born: u64,
    id: i32,
    channel: MidiChannel,
    note: NoteNumber,
}

/// The state a plugin's output stream carries from event to event and block to
/// block: the [`Translator`] of its MIDI 1.0 events (RPN, NRPN and Bank Select
/// runs span messages), and the table of the note ids the plugin gave its own
/// notes, so an expression on one is a message for that note.
///
/// One per activation, fixed-size and inline: nothing here allocates, so it is
/// built and used on the audio thread alike. [`reset`](Self::reset) forgets
/// everything, sending nothing.
#[derive(Debug)]
pub struct OutputStream {
    pub(super) translator: Translator,
    notes: [Option<OwnNote>; OWN_NOTES],
    started: u64,
    notes_evicted: u64,
    expressions_dropped: u64,
    buses_dropped: u64,
}

impl Default for OutputStream {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputStream {
    /// A stream with nothing selected and no note started.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            translator: Translator::new(),
            notes: [None; OWN_NOTES],
            started: 0,
            notes_evicted: 0,
            expressions_dropped: 0,
            buses_dropped: 0,
        }
    }

    /// Forgets the translator's runs and every note id; nothing pending is sent.
    pub fn reset(&mut self) {
        self.translator.reset();
        self.notes = [None; OWN_NOTES];
    }

    /// Notes the plugin started under an id of its own that evicted an older
    /// note's entry because the table was full, since construction.
    #[must_use]
    pub fn notes_evicted(&self) -> u64 {
        self.notes_evicted
    }

    /// Expressions dropped because their id names no note the plugin has
    /// started, since construction.
    #[must_use]
    pub fn expressions_dropped(&self) -> u64 {
        self.expressions_dropped
    }

    /// Events dropped because they left on an event bus past the sixteen Groups,
    /// since construction.
    #[must_use]
    pub fn buses_dropped(&self) -> u64 {
        self.buses_dropped
    }

    pub(super) fn count_bus_dropped(&mut self) {
        self.buses_dropped += 1;
    }

    pub(super) fn count_expression_dropped(&mut self) {
        self.expressions_dropped += 1;
    }

    /// The plugin started `note` on `channel` under `id` (`-1` is no id). A full
    /// table evicts its oldest entry for it.
    pub(super) fn note_on(&mut self, id: i32, channel: MidiChannel, note: NoteNumber) {
        if id < 0 {
            return;
        }
        self.started += 1;
        let entry = Some(OwnNote {
            born: self.started,
            id,
            channel,
            note,
        });
        let slot = self
            .notes
            .iter()
            .position(|n| n.is_some_and(|n| n.id == id))
            .or_else(|| self.notes.iter().position(Option::is_none))
            .or_else(|| {
                self.notes_evicted += 1;
                self.notes
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, n)| n.map_or(u64::MAX, |n| n.born))
                    .map(|(i, _)| i)
            });
        if let Some(i) = slot {
            self.notes[i] = entry;
        }
    }

    /// The plugin ended a note: by `id` when it names one, else by `channel` and
    /// `note`, as VST3 matches a note off.
    pub(super) fn note_off(&mut self, id: i32, channel: MidiChannel, note: NoteNumber) {
        for slot in &mut self.notes {
            let hit = match *slot {
                Some(n) if id >= 0 => n.id == id,
                Some(n) => n.channel == channel && n.note == note,
                None => false,
            };
            if hit {
                *slot = None;
                if id >= 0 {
                    return;
                }
            }
        }
    }

    /// The note the plugin gave `id`, if it has not ended it.
    pub(super) fn note_of(&self, id: i32) -> Option<(MidiChannel, NoteNumber)> {
        self.notes
            .iter()
            .flatten()
            .find(|n| n.id == id)
            .map(|n| (n.channel, n.note))
    }
}
