//! What the file tests share: a track of events and a note.

use tutti_midi_file::{FileEvent, Tick, Track};
use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Voice1};

/// A track of `events`, in tick order, ending at the last of them.
pub fn track(events: Vec<(u64, FileEvent)>) -> Track {
    events.into_iter().map(|(t, e)| (Tick(t), e)).collect()
}

/// A Note On of `key` at velocity 100 on `channel`, in the first Group.
pub fn note(channel: u8, key: u8) -> FileEvent {
    Message::midi1(
        MidiChannel::of(channel),
        Voice1::note_on(NoteNumber::of(key), U7::of(100)),
    )
    .into()
}
