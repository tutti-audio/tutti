//! What a plugin's output event stream keeps between events.

use tutti_midi_protocol::Midi2Translator;
use tutti_ump::{MidiChannel, NoteNumber};

/// The notes a plugin may name in an expression at once. A plugin sounds a few
/// dozen voices; past this the oldest ended entry is evicted for a new note, or
/// the oldest live one when none has ended ([`OutputStream::notes_evicted`]).
const OWN_NOTES: usize = 256;

/// A note the plugin started under an id.
#[derive(Clone, Copy, Debug)]
struct OwnNote {
    /// When it started, in the stream's own count of note starts: the oldest
    /// is the one a full table evicts first.
    born: u64,
    id: i32,
    channel: MidiChannel,
    note: NoteNumber,
    /// The plugin sent its note off. The entry is kept: a voice in its release
    /// goes on sending expressions after it, and they are still that note's.
    ended: bool,
}

/// The state a plugin's output stream carries from event to event and block to
/// block: the [`Midi2Translator`] of its MIDI 1.0 events (RPN, NRPN and Bank Select
/// runs span messages), and the table of the notes the plugin started under an
/// id, so an expression on one is a message for that note.
///
/// An expression names its note by an id, and an id names a note here only if
/// the plugin's output started one under it: a note on a host-style id passed
/// through by a MIDI effect counts, a note whose note off has been sent counts
/// until its entry is evicted (release phases), and nothing else does. An
/// expression on any other id, including one whose note on had id `-1`, is
/// dropped and counted ([`expressions_dropped`](Self::expressions_dropped)),
/// never read as a `channel * 128 + note` of the host's.
///
/// A parameter run (select, Data Entry MSB) with no Data Entry LSB waits for
/// one: the translator never flushes a live output, which has no end (M2-104
/// Appendix D.3.3).
///
/// One per activation, fixed-size and inline: nothing here allocates, so it is
/// built and used on the audio thread alike. [`reset`](Self::reset) forgets
/// everything, sending nothing.
#[derive(Debug)]
pub struct OutputStream {
    pub(super) translator: Midi2Translator,
    notes: [Option<OwnNote>; OWN_NOTES],
    started: u64,
    notes_evicted: u64,
    expressions_dropped: u64,
    buses_dropped: u64,
    full_dropped: u64,
    sysex_dropped: u64,
    lists_busy: u64,
    untranslated_dropped: u64,
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
            translator: Midi2Translator::new(),
            notes: [None; OWN_NOTES],
            started: 0,
            notes_evicted: 0,
            expressions_dropped: 0,
            buses_dropped: 0,
            full_dropped: 0,
            sysex_dropped: 0,
            lists_busy: 0,
            untranslated_dropped: 0,
        }
    }

    /// Forgets the translator's runs and every note id; nothing pending is sent.
    pub fn reset(&mut self) {
        self.translator.reset();
        self.notes = [None; OWN_NOTES];
    }

    /// Notes the plugin started that evicted an older note's entry because the
    /// table was full, since construction.
    #[must_use]
    pub fn notes_evicted(&self) -> u64 {
        self.notes_evicted
    }

    /// Expressions dropped since construction: on an id no output note on
    /// started (or whose entry was evicted), with a value that is not a number,
    /// or of a dimension with no per-note message (vibrato, expression, a custom
    /// or unknown id) or past what Pitch 7.25 holds.
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

    /// SysEx messages dropped whole because the block's MIDI had no room for
    /// them, since construction.
    #[must_use]
    pub fn sysex_dropped(&self) -> u64 {
        self.sysex_dropped
    }

    /// Events dropped because they have no UMP form: bytes that are no MIDI
    /// message, a controller number with no message, and the dimensions a host
    /// does not translate (an expression's text or integer value, a chord, a
    /// scale), since construction.
    #[must_use]
    pub fn untranslated_dropped(&self) -> u64 {
        self.untranslated_dropped
    }

    /// Blocks whose output list a plugin thread held when the host came to read
    /// it, so nothing of it was read, since construction.
    #[must_use]
    pub fn lists_busy(&self) -> u64 {
        self.lists_busy
    }

    /// The events lost since construction: expressions dropped, events on a bus
    /// past the Groups, events a full block list had no room for, SysEx that did
    /// not fit, events with no UMP form, and one for each block whose list could
    /// not be read. What the host reports as lost MIDI, by its change from block
    /// to block. An eviction is not here: it loses a mapping, not an event, and any
    /// expression it later costs is already in `expressions_dropped`
    /// ([`notes_evicted`](Self::notes_evicted) is its own reading).
    #[must_use]
    pub fn lost_total(&self) -> u64 {
        self.expressions_dropped
            + self.buses_dropped
            + self.full_dropped
            + self.sysex_dropped
            + self.lists_busy
            + self.untranslated_dropped
    }

    pub(super) fn count_bus_dropped(&mut self) {
        self.buses_dropped += 1;
    }

    pub(super) fn count_expression_dropped(&mut self) {
        self.expressions_dropped += 1;
    }

    pub(super) fn count_full_dropped(&mut self) {
        self.full_dropped += 1;
    }

    pub(super) fn count_sysex_dropped(&mut self) {
        self.sysex_dropped += 1;
    }

    pub(super) fn count_untranslated(&mut self) {
        self.untranslated_dropped += 1;
    }

    pub(super) fn count_list_busy(&mut self) {
        self.lists_busy += 1;
    }

    /// The plugin started `note` on `channel` under `id` (`-1` is no id). A full
    /// table evicts its oldest ended entry for it, or its oldest live one.
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
            ended: false,
        });
        let existing = self
            .notes
            .iter()
            .position(|n| n.is_some_and(|n| n.id == id));
        let free = || self.notes.iter().position(Option::is_none);
        let slot = match existing.or_else(free) {
            Some(slot) => slot,
            None => {
                self.notes_evicted += 1;
                self.eviction()
            }
        };
        self.notes[slot] = entry;
    }

    /// The slot of the entry a full table gives up: the oldest ended, else the
    /// oldest.
    fn eviction(&self) -> usize {
        let key = |n: &Option<OwnNote>| n.map_or((true, u64::MAX), |n| (!n.ended, n.born));
        self.notes
            .iter()
            .enumerate()
            .min_by_key(|(_, n)| key(n))
            .map_or(0, |(slot, _)| slot)
    }

    /// The plugin ended a note: by `id` when it names one, else by `channel` and
    /// `note`, as VST3 matches a note off. The entry stays, marked ended.
    pub(super) fn note_off(&mut self, id: i32, channel: MidiChannel, note: NoteNumber) {
        for entry in self.notes.iter_mut().flatten() {
            let hit = if id >= 0 {
                entry.id == id
            } else {
                entry.channel == channel && entry.note == note
            };
            if hit {
                entry.ended = true;
            }
        }
    }

    /// The note the plugin gave `id`, ended or not.
    pub(super) fn note_of(&self, id: i32) -> Option<(MidiChannel, NoteNumber)> {
        self.notes
            .iter()
            .flatten()
            .find(|n| n.id == id)
            .map(|n| (n.channel, n.note))
    }
}
