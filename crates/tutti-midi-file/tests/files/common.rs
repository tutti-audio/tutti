//! What the file tests share: a track of events and a note.

use tutti_midi_file::{
    FileEvent, MetricalSequences, MetricalView, MidiFile, SysEx, Tick, Timebase, Track,
};
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

/// The metrical view of `file`; a test of a metrical file reads its beats, tempo map and clip
/// through it.
pub fn metrical(file: &MidiFile) -> MetricalView<'_> {
    file.timebase().metrical().expect("a metrical file")
}

/// The independent sequences of `file`, a metrical file of format 2 or above.
pub fn sequences(file: &MidiFile) -> MetricalSequences<'_> {
    let Timebase::MetricalSequences(view) = file.timebase() else {
        panic!("a metrical file of independent sequences");
    };
    view
}

/// A whole SysEx message of the 7-bit `bytes`.
pub fn complete(bytes: &[u8]) -> SysEx {
    SysEx::complete(bytes).expect("7-bit data")
}

/// The first packet of a divided SysEx message, of the 7-bit `bytes`.
pub fn start(bytes: &[u8]) -> SysEx {
    SysEx::start(bytes).expect("7-bit data")
}
