//! What a plugin's output event stream keeps between events.

use tutti_midi_protocol::Translator;
use tutti_ump::{MidiChannel, NoteNumber};

/// The notes a plugin may name in an expression at once. A plugin sounds a few
/// dozen voices; past this the notes are counted ([`OutputStream::notes_dropped`])
/// and their expression falls back to the host's own note ids.
const OWN_NOTES: usize = 256;

/// A note the plugin started under an id of its own.
#[derive(Clone, Copy, Debug)]
struct OwnNote {
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
    notes_dropped: u64,
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
            notes_dropped: 0,
        }
    }

    /// Forgets the translator's runs and every note id; nothing pending is sent.
    pub fn reset(&mut self) {
        self.translator.reset();
        self.notes = [None; OWN_NOTES];
    }

    /// Note starts the plugin reported while its table was full, since
    /// construction: each fell back to the host's id space for its expressions.
    #[must_use]
    pub fn notes_dropped(&self) -> u64 {
        self.notes_dropped
    }

    /// The plugin started `note` on `channel` under `id` (`-1` is no id).
    pub(super) fn note_on(&mut self, id: i32, channel: MidiChannel, note: NoteNumber) {
        if id < 0 {
            return;
        }
        let entry = Some(OwnNote { id, channel, note });
        let slot = self
            .notes
            .iter()
            .position(|n| n.is_some_and(|n| n.id == id))
            .or_else(|| self.notes.iter().position(Option::is_none));
        match slot {
            Some(i) => self.notes[i] = entry,
            None => self.notes_dropped += 1,
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
