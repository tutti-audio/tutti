//! What the file tests share: a track of events and a note.

use tutti_midi_file::{
    ClipFile, ClipReadError, Layout, LeadTrack, Meta, MetricalSequences, MetricalView, Pattern,
    ReadRepair, SmfDivision, SmfEvent, SmfReadError, StandardMidiFile, SysEx, Tick, Timebase,
    Track, TracksError,
};
use tutti_ump::{MidiChannel, NoteNumber, U7, Voice1};

/// A track of `events`, in tick order, ending at the last of them.
pub(crate) fn track(events: Vec<(u64, SmfEvent)>) -> Track {
    events.into_iter().map(|(t, e)| (Tick(t), e)).collect()
}

/// A Note On of `key` at velocity 100 on `channel`.
pub(crate) fn note(channel: u8, key: u8) -> SmfEvent {
    SmfEvent::Channel {
        channel: MidiChannel::of(channel),
        message: Voice1::note_on(NoteNumber::of(key), U7::of(100)),
    }
}

/// The metrical view of `file`; a test of a metrical file reads its beats, tempo map and clip
/// through it.
pub(crate) fn metrical(file: &StandardMidiFile) -> MetricalView<'_> {
    file.timebase().metrical().expect("a metrical file")
}

/// The independent sequences of `file`, a metrical file of format 2 or above.
pub(crate) fn sequences(file: &StandardMidiFile) -> MetricalSequences<'_> {
    let Timebase::MetricalSequences(view) = file.timebase() else {
        panic!("a metrical file of independent sequences");
    };
    view
}

/// A whole SysEx message of the 7-bit `bytes`.
pub(crate) fn complete(bytes: &[u8]) -> SysEx {
    SysEx::complete(bytes).expect("7-bit data")
}

/// The first packet of a divided SysEx message, of the 7-bit `bytes`.
pub(crate) fn start(bytes: &[u8]) -> SysEx {
    SysEx::start(bytes).expect("7-bit data")
}

/// A format 0 file of `track`.
pub(crate) fn format0(
    division: SmfDivision,
    track: Track,
) -> Result<StandardMidiFile, TracksError> {
    StandardMidiFile::new(division, Layout::Format0(LeadTrack::new(track)))
}

/// A format 1 file of `tracks`, the first of which is its lead.
///
/// # Panics
///
/// If `tracks` is empty.
pub(crate) fn format1(
    division: SmfDivision,
    mut tracks: Vec<Track>,
) -> Result<StandardMidiFile, TracksError> {
    let lead = LeadTrack::new(tracks.remove(0));
    StandardMidiFile::new(division, Layout::Format1 { lead, tracks })
}

/// A format 2 file of `tracks`, each a pattern numbered by its place.
pub(crate) fn format2(
    division: SmfDivision,
    tracks: Vec<Track>,
) -> Result<StandardMidiFile, TracksError> {
    StandardMidiFile::new(
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
pub(crate) const DEFAULTS: [u8; 15] = [
    0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, 0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08,
];

/// Where each event of the track body `body` starts and ends, and whether it
/// is a meta event at delta-time 0.
fn event_spans(body: &[u8]) -> Vec<(usize, usize, bool)> {
    let vlq = |at: &mut usize| {
        let mut value = 0usize;
        loop {
            let byte = body[*at];
            *at += 1;
            value = value << 7 | usize::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return value;
            }
        }
    };
    let mut spans = Vec::new();
    let (mut at, mut running) = (0, 0u8);
    while at < body.len() {
        let start = at;
        let delta = vlq(&mut at);
        let mut status = body[at];
        if status >= 0x80 {
            at += 1;
            if status < 0xF0 {
                running = status;
            }
        } else {
            status = running;
        }
        let meta = status == 0xFF;
        match status {
            0xFF => {
                at += 1;
                let len = vlq(&mut at);
                at += len;
            }
            0xF0 | 0xF7 => {
                let len = vlq(&mut at);
                at += len;
            }
            0xC0..=0xDF => at += 1,
            _ => at += 2,
        }
        spans.push((start, at, meta && delta == 0));
    }
    spans
}

/// `bytes` of a track, or of a file of one track, without the default Set Tempo
/// and Time Signature of [`DEFAULTS`], for a test of what else it holds. Each
/// found must be an event of the track that has only meta events at delta-time
/// 0 before it (the place the writer puts it, before any other event); one
/// elsewhere fails. A file's track length is kept true.
pub(crate) fn without_defaults(bytes: &[u8]) -> Vec<u8> {
    let file = bytes.starts_with(b"MThd");
    let (head, body) = bytes.split_at(if file { 22 } else { 0 });
    let spans = event_spans(body);
    let mut out = body.to_vec();
    let mut removed = 0;
    for run in [&DEFAULTS[..7], &DEFAULTS[7..]] {
        let Some(nth) = spans.iter().position(|&(a, b, _)| body[a..b] == *run) else {
            continue;
        };
        assert!(
            spans[..nth].iter().all(|&(_, _, opening)| opening),
            "a default event {run:02X?} after an event that is not an opening meta event"
        );
        let (a, b, _) = spans[nth];
        out.drain(a - removed..b - removed);
        removed += b - a;
    }
    let mut out = [head, &out].concat();
    if file {
        let length = u32::from_be_bytes(out[18..22].try_into().unwrap()) - removed as u32;
        out[18..22].copy_from_slice(&length.to_be_bytes());
    }
    out
}

/// `file`'s changes a read made other than giving it the default tempo and
/// time signature.
pub(crate) fn placed(file: &StandardMidiFile) -> Vec<ReadRepair> {
    file.repairs()
        .iter()
        .filter(|n| {
            !matches!(
                n,
                ReadRepair::DefaultTempo { .. } | ReadRepair::DefaultTimeSignature { .. }
            )
        })
        .cloned()
        .collect()
}

/// The events of track `track` of `file`, a read file, without the Set Tempo and
/// Time Signature the read inserted there ([`ReadRepair::DefaultTempo`],
/// [`ReadRepair::DefaultTimeSignature`]): the first of each kind at tick 0.
pub(crate) fn unspecified(file: &StandardMidiFile, track: usize) -> Vec<(Tick, SmfEvent)> {
    let inserted = |wanted: ReadRepair| file.repairs().contains(&wanted);
    let mut tempo = inserted(ReadRepair::DefaultTempo { track });
    let mut meter = inserted(ReadRepair::DefaultTimeSignature { track });
    let events = file.tracks()[track].events();
    let opening = events
        .iter()
        .take_while(|(tick, e)| *tick == Tick(0) && matches!(e, SmfEvent::Meta(_)))
        .count();
    events
        .iter()
        .enumerate()
        .filter(|&(at, (_, event))| {
            let drop = at < opening
                && match event {
                    SmfEvent::Meta(Meta::Tempo { .. }) => std::mem::take(&mut tempo),
                    SmfEvent::Meta(Meta::TimeSignature { .. }) => std::mem::take(&mut meter),
                    _ => false,
                };
            !drop
        })
        .map(|(_, e)| e.clone())
        .collect()
}

/// Reads `bytes` as a Standard MIDI File.
pub(crate) fn read_bytes(bytes: impl AsRef<[u8]>) -> Result<StandardMidiFile, SmfReadError> {
    StandardMidiFile::from_bytes(bytes.as_ref())
}

/// Reads `bytes` as a Clip File.
pub(crate) fn read_clip(bytes: impl AsRef<[u8]>) -> Result<ClipFile, ClipReadError> {
    ClipFile::from_bytes(bytes.as_ref())
}

/// `file` written to a `Vec`.
pub(crate) fn encoded(file: &StandardMidiFile) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.write(&mut bytes).expect("a Vec takes the write");
    bytes
}

/// `file` written to a `Vec`.
pub(crate) fn encoded_clip(file: &ClipFile) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.write(&mut bytes).expect("a Vec takes the write");
    bytes
}
