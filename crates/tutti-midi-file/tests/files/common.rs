//! What the file tests share: a track of events and a note.

use tutti_midi_file::{
    FileEvent, Layout, LeadTrack, Meta, MetricalSequences, MetricalView, MidiFile, Normalized,
    Pattern, SmfDivision, SysEx, Tick, Timebase, Track, TracksError,
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

/// A format 0 file of `track`.
pub fn format0(division: SmfDivision, track: Track) -> Result<MidiFile, TracksError> {
    MidiFile::smf(division, Layout::Format0(LeadTrack::new(track)))
}

/// A format 1 file of `tracks`, the first of which is its lead.
///
/// # Panics
///
/// If `tracks` is empty.
pub fn format1(division: SmfDivision, mut tracks: Vec<Track>) -> Result<MidiFile, TracksError> {
    let lead = LeadTrack::new(tracks.remove(0));
    MidiFile::smf(division, Layout::Format1 { lead, tracks })
}

/// A format 2 file of `tracks`, each a pattern numbered by its place.
pub fn format2(division: SmfDivision, tracks: Vec<Track>) -> Result<MidiFile, TracksError> {
    MidiFile::smf(
        division,
        Layout::Format2 {
            copyright: None,
            patterns: tracks.into_iter().map(Pattern::new).collect(),
        },
    )
}

/// The Set Tempo for 500 000 us a quarter (120 beats a minute) and the Time
/// Signature of 4/4 with 24 clocks a click and 8 32nd notes to the quarter (SMF
/// 1.0 section 3.3's example), each at delta-time 0: what a file that gives
/// none is written with.
pub const DEFAULTS: [u8; 15] = [
    0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, 0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08,
];

/// `bytes` of a track, or of a file of one track, without the first run of
/// each of the default Set Tempo and Time Signature of [`DEFAULTS`], for a test
/// of what else it holds. A file's track length is kept true.
pub fn without_defaults(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut removed = 0;
    for run in [&DEFAULTS[..7], &DEFAULTS[7..]] {
        if let Some(at) = out.windows(run.len()).position(|w| w == run) {
            out.drain(at..at + run.len());
            removed += run.len() as u32;
        }
    }
    if out.starts_with(b"MThd") {
        let length = u32::from_be_bytes(out[18..22].try_into().unwrap()) - removed;
        out[18..22].copy_from_slice(&length.to_be_bytes());
    }
    out
}

/// `file`'s changes a read made other than giving it the default tempo and
/// time signature.
pub fn placed(file: &MidiFile) -> Vec<Normalized> {
    file.normalized()
        .iter()
        .filter(|n| {
            !matches!(
                n,
                Normalized::DefaultTempo { .. } | Normalized::DefaultTimeSignature { .. }
            )
        })
        .cloned()
        .collect()
}

/// The events of `track` without the default tempo and time signature a file
/// is given at tick 0.
pub fn unspecified(track: &Track) -> Vec<(Tick, FileEvent)> {
    let mut tempo = false;
    let mut meter = false;
    track
        .events()
        .iter()
        .filter(|(tick, event)| {
            let default = *tick == Tick(0)
                && match event {
                    FileEvent::Meta(Meta::Tempo {
                        micros_per_quarter, ..
                    }) => {
                        !std::mem::replace(&mut tempo, true) && micros_per_quarter.get() == 500_000
                    }
                    FileEvent::Meta(Meta::TimeSignature {
                        numerator: 4,
                        denominator_power: 2,
                        clocks_per_click: 24,
                        thirty_seconds_per_quarter: 8,
                        ..
                    }) => !std::mem::replace(&mut meter, true),
                    _ => false,
                };
            !default
        })
        .cloned()
        .collect()
}
