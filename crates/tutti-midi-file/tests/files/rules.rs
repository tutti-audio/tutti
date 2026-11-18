//! The rules SMF 1.0 puts on what a track holds and where: the Sequence Number,
//! Track Name, SMPTE Offset and copyright notice a track holds as fields and
//! the writer puts first, the MIDI Channel Prefix and Sequencer-Specific
//! events, the SysEx and placement rules a writer keeps, what a reader changes
//! in a file that breaks them, and the tempo and time signature a file
//! specifies. Each figure is a byte or a value written from the specification.

use crate::common::{metrical, note, sequences, track};
use crate::support::{Division, RawEvent, build_smf};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_midi_file::{
    FileEvent, Lead, Meta, MidiFile, Normalized, Placement, SmfDivision, SmfFormat,
    SmpteFractionalFrames, SmpteOffset, SysEx, TextKind, Tick, Track, TracksError,
};
use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode};
use tutti_types::{Beat, Bpm};
use tutti_ump::{ManufacturerId, MidiChannel, NoteNumber, U7};

const TPQ: u16 = 480;
const BEAT: u32 = TPQ as u32;
const DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);

fn meta(delta: u32, kind: u8, data: &[u8]) -> RawEvent {
    let mut bytes = vec![0xFF, kind, data.len() as u8];
    bytes.extend_from_slice(data);
    RawEvent::new(delta, bytes)
}

fn read(format: u16, tracks: &[Vec<RawEvent>]) -> MidiFile {
    MidiFile::from_bytes(&build_smf(format, Division::Metrical(TPQ), tracks)).unwrap()
}

fn smf(format: SmfFormat, tracks: Vec<Track>) -> Result<MidiFile, TracksError> {
    MidiFile::smf(format, DIVISION, tracks)
}

fn one(events: Vec<(u64, FileEvent)>) -> Result<MidiFile, TracksError> {
    smf(SmfFormat::SingleTrack, vec![track(events)])
}

fn ev(meta: Meta) -> FileEvent {
    FileEvent::Meta(meta)
}

/// A whole SysEx message and the first packet of a divided one.
fn complete(data: &[u8]) -> FileEvent {
    FileEvent::SysEx(SysEx::Complete(data.into()))
}

fn start(data: &[u8]) -> FileEvent {
    FileEvent::SysEx(SysEx::Start(data.into()))
}

/// What a read changed: a `Lead` event of `track` at `at`, and what was done.
fn changed(track: usize, at: u64, lead: Lead, placement: Placement) -> Normalized {
    Normalized::Lead {
        track,
        at: Tick(at),
        lead,
        placement,
    }
}

/// What a malformed meta event holds: its type byte and data.
fn malformed(kind: u8, data: &[u8]) -> (u8, Vec<u8>) {
    (kind, data.to_vec())
}

fn held(event: &FileEvent) -> (u8, Vec<u8>) {
    let FileEvent::Meta(Meta::Malformed(m)) = event else {
        panic!("a malformed meta event, not {event:?}");
    };
    (m.kind(), m.data().to_vec())
}

fn tempo(bpm: f64) -> FileEvent {
    ev(Meta::tempo(Bpm::of(bpm)))
}

fn played() -> Track {
    Track::new().note(
        Tick(0),
        Tick(10),
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    )
}

/// The body of the only track of `file`, from after its MTrk length.
fn body(file: &MidiFile) -> Vec<u8> {
    file.to_bytes()[22..].to_vec()
}

fn offset(h: u8, m: u8, s: u8, f: u8, rate: SmpteFrameRate, ff: u8) -> SmpteOffset {
    SmpteOffset::new(
        SmpteTimecode::new(h, m, s, f, rate).unwrap(),
        SmpteFractionalFrames::of(ff),
    )
    .unwrap()
}

// ---- Sequence Number --------------------------------------------------------

/// `FF 00 02 ssss` is a 16-bit number, most significant byte first.
#[test]
fn a_sequence_number_is_two_bytes_most_significant_first() {
    let file = read(0, &[vec![meta(0, 0x00, &[0x01, 0x02])]]);
    assert_eq!(file.tracks()[0].sequence_number(), Some(0x0102));
    assert_eq!(
        body(&file),
        [0x00, 0xFF, 0x00, 0x02, 0x01, 0x02, 0x00, 0xFF, 0x2F, 0x00]
    );
}

/// A Sequence Number and a Track Name are written at time 0, before any
/// channel message, however and whenever the builder is given them: they are
/// fields of the track, not events that could be placed late.
/// Spec: smf/sequence-number#1, smf/meta-events#10
#[test]
fn a_sequence_number_and_a_track_name_are_written_first_at_time_0() {
    let track = played().named("n").numbered(9);
    assert_eq!(
        body(&smf(SmfFormat::SingleTrack, vec![track]).unwrap()),
        [
            0x00, 0xFF, 0x00, 0x02, 0x00, 0x09, // the Sequence Number
            0x00, 0xFF, 0x03, 0x01, b'n', // the Track Name
            0x00, 0x90, 60, 100, // the note
            0x0A, 0x80, 60, 64, //
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
}

/// The patterns of a format 2 file are identified by their Sequence Numbers, and
/// one without is its place in the file (the first is 0).
/// Spec: smf/sequence-number#2, smf/sequence-number#3
#[test]
fn a_format_2_pattern_is_its_number_or_its_place() {
    let file = smf(
        SmfFormat::Sequential,
        vec![played().numbered(7), played(), played().numbered(0xFFFF)],
    )
    .unwrap();
    let numbers = |f: &MidiFile| {
        sequences(f)
            .sequences()
            .map(|s| s.number())
            .collect::<Vec<_>>()
    };
    assert_eq!(numbers(&file), [7, 1, 0xFFFF]);
    let back = MidiFile::from_bytes(&file.to_bytes()).unwrap();
    assert_eq!(numbers(&back), [7, 1, 0xFFFF]);
}

/// A format 0 or 1 file holds one sequence, whose number is in its first track.
/// A writer refuses a number in another track; a reader drops it and says so.
/// Spec: smf/sequence-number#4
#[test]
fn a_format_1_file_has_one_sequence_number_in_its_first_track() {
    let later = |format| smf(format, vec![played(), played().numbered(1)]).map(drop);
    assert_eq!(
        later(SmfFormat::Parallel),
        Err(TracksError::SequenceNumberNotInFirstTrack { track: 1 })
    );
    assert_eq!(later(SmfFormat::Sequential), Ok(()));
    let tracks = [
        vec![RawEvent::note_on(0, 0, 60, 100)],
        vec![meta(0, 0x00, &[0, 3])],
    ];
    let dropped = read(1, &tracks);
    assert_eq!(dropped.tracks()[1].sequence_number(), None);
    assert_eq!(
        dropped.normalized(),
        [changed(
            1,
            0,
            Lead::SequenceNumber(3),
            Placement::NotInFirstTrack
        )]
    );
    let kept = read(2, &tracks);
    assert_eq!(kept.tracks()[1].sequence_number(), Some(3));
    assert_eq!(kept.normalized(), []);
}

// ---- MIDI Channel Prefix ----------------------------------------------------

/// `FF 20 01 cc`: a channel 0 to 15; another value is kept as the file has it.
/// Spec: smf/channel-prefix#1
#[test]
fn a_channel_prefix_is_one_byte_0_to_15() {
    let file = read(0, &[vec![meta(0, 0x20, &[15]), meta(0, 0x20, &[16])]]);
    let events = file.tracks()[0].events();
    assert_eq!(events[0].1, ev(Meta::channel_prefix(MidiChannel::of(15))));
    assert_eq!(held(&events[1].1), malformed(0x20, &[16]));
    assert_eq!(
        body(&file),
        [0, 0xFF, 0x20, 1, 15, 0, 0xFF, 0x20, 1, 16, 0, 0xFF, 0x2F, 0]
    );
}

/// The prefix channel goes to the SysEx and meta events after it, until the next
/// channel message or prefix; an Instrument Name after a prefix is the
/// instrument of that channel.
/// Spec: smf/channel-prefix#2, smf/instrument-name#1
#[test]
fn a_prefix_holds_until_the_next_channel_message_or_prefix() {
    let text = |kind, s| ev(Meta::text(kind, s));
    let prefix = |c| ev(Meta::channel_prefix(MidiChannel::of(c)));
    let t = track(vec![
        (0, prefix(3)),
        (0, text(TextKind::InstrumentName, "flute")),
        (1, text(TextKind::Marker, "m")),
        (2, note(5, 60)),
        (3, text(TextKind::Marker, "after")),
        (4, prefix(9)),
        (4, text(TextKind::Text, "t")),
    ]);
    let channels: Vec<_> = t
        .events_with_channel()
        .map(|(_, _, c)| c.map(MidiChannel::get))
        .collect();
    assert_eq!(
        channels,
        [None, Some(3), Some(3), Some(5), None, None, Some(9)]
    );
}

/// Prefixes let several tracks share a format 0 track: their non-MIDI events
/// stay with the channel after a write and a read.
/// Spec: smf/channel-prefix#3
#[test]
fn prefixes_keep_the_tracks_of_a_format_0_file_apart() {
    let prefix = |c| ev(Meta::channel_prefix(MidiChannel::of(c)));
    let name = |s| ev(Meta::text(TextKind::InstrumentName, s));
    let file = one(vec![
        (0, prefix(0)),
        (0, name("bass")),
        (0, note(0, 40)),
        (0, prefix(1)),
        (0, name("lead")),
        (0, note(1, 72)),
    ])
    .unwrap();
    let back = MidiFile::from_bytes(&file.to_bytes()).unwrap();
    let names: Vec<_> = back.tracks()[0]
        .events_with_channel()
        .filter_map(|(_, e, c)| match e {
            FileEvent::Meta(Meta::Text { bytes, .. }) => Some((c?.get(), bytes.to_vec())),
            _ => None,
        })
        .collect();
    assert_eq!(names, [(0, b"bass".to_vec()), (1, b"lead".to_vec())]);
}

// ---- SMPTE Offset -----------------------------------------------------------

/// `FF 54 05 hr mn se fr ff`: the hour byte is MIDI Time Code's, `0rrhhhhh`
/// with rate codes 0 to 3 for 24, 25, 29.97 drop and 30; `ff` is 100ths of a
/// frame.
/// Spec: smf/smpte-offset#1, smf/smpte-offset#3, smf/smpte-offset#5
#[test]
fn an_smpte_offset_is_the_mtc_hour_byte_and_hundredths_of_a_frame() {
    let rates = [
        (0u8, SmpteFrameRate::Fps24, 23u8),
        (1, SmpteFrameRate::Fps25, 24),
        (2, SmpteFrameRate::Fps2997Df, 29),
        (3, SmpteFrameRate::Fps30, 29),
    ];
    for (code, rate, last_frame) in rates {
        let data = [code << 5 | 23, 59, 58, last_frame, 99];
        let file = read(0, &[vec![meta(0, 0x54, &data)]]);
        assert_eq!(
            file.smpte_offset(0),
            Some(offset(23, 59, 58, last_frame, rate, 99))
        );
        assert_eq!(body(&file)[..4], [0, 0xFF, 0x54, 5]);
        assert_eq!(body(&file)[4..9], data);
    }
}

/// A field out of its range is no SMPTE Offset: hour 24, the hour byte's top
/// bit, minute 60, a frame past the rate's count (24 at 24 a second), 100
/// hundredths; it is kept as read.
/// Spec: smf/smpte-offset#3
#[test]
fn an_smpte_offset_out_of_range_is_kept_as_read() {
    for data in [
        [24u8, 0, 0, 0, 0],
        [0x80, 0, 0, 0, 0],
        [0, 60, 0, 0, 0],
        [0, 0, 0, 24, 0],
        [0, 0, 0, 0, 100],
    ] {
        let file = read(0, &[vec![meta(0, 0x54, &data)]]);
        assert_eq!(
            held(&file.tracks()[0].events()[0].1),
            malformed(0x54, &data)
        );
    }
    // MTC has no code for 29.97 non-drop.
    assert_eq!(
        SmpteOffset::new(
            SmpteTimecode::origin(SmpteFrameRate::Fps2997Ndf),
            SmpteFractionalFrames::of(0)
        ),
        None
    );
    check_range_contract(&RangeContract {
        new: SmpteFractionalFrames::new,
        get: SmpteFractionalFrames::get,
        of: Some(SmpteFractionalFrames::of),
        valid: &[0, 50, 99],
        invalid: &[100, 255],
    });
}

/// The offset is at the start of its track, before any delta and any channel
/// message: the builder cannot place it later, it is written first behind a
/// copyright notice, and a reader drops one after a delta or a channel message
/// and says so.
/// Spec: smf/smpte-offset#2
#[test]
fn an_smpte_offset_is_at_the_start_of_the_track() {
    let at = offset(1, 0, 0, 0, SmpteFrameRate::Fps25, 0);
    let wire = [1 << 5 | 1, 0, 0, 0, 0];
    let written = smf(
        SmfFormat::SingleTrack,
        vec![played().smpte_start(at).copyright("(C)")],
    )
    .unwrap();
    assert_eq!(
        body(&written),
        [
            0x00, 0xFF, 0x02, 0x03, b'(', b'C', b')', // the notice
            0x00, 0xFF, 0x54, 0x05, wire[0], 0, 0, 0, 0, // the offset
            0x00, 0x90, 60, 100, // the note
            0x0A, 0x80, 60, 64, //
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
    let after_a_note = read(
        0,
        &[vec![RawEvent::note_on(0, 0, 60, 100), meta(0, 0x54, &wire)]],
    );
    assert_eq!(after_a_note.smpte_offset(0), None);
    assert_eq!(
        after_a_note.normalized(),
        [changed(0, 0, Lead::SmpteOffset(at), Placement::Late)]
    );
    let after_a_delta = read(0, &[vec![meta(5, 0x54, &wire)]]);
    assert_eq!(after_a_delta.smpte_offset(0), None);
    assert_eq!(
        after_a_delta.normalized(),
        [changed(0, 5, Lead::SmpteOffset(at), Placement::Late)]
    );
}

/// In a format 1 file the offset is stored in the first track and means
/// nothing in the others: a writer refuses it there, and a reader drops it and
/// says so.
/// Spec: smf/smpte-offset#4
#[test]
fn a_format_1_smpte_offset_is_the_first_tracks() {
    let at = offset(1, 2, 3, 4, SmpteFrameRate::Fps25, 5);
    assert_eq!(
        smf(
            SmfFormat::Parallel,
            vec![played(), played().smpte_start(at)]
        )
        .map(drop),
        Err(TracksError::SmpteOffsetNotInFirstTrack { track: 1 })
    );
    let tracks = [(); 2].map(|()| vec![meta(0, 0x54, &[1 << 5 | 1, 2, 3, 4, 5])]);
    let first = read(1, &tracks);
    assert_eq!(
        (first.smpte_offset(0), first.smpte_offset(1)),
        (Some(at), None)
    );
    assert_eq!(
        first.normalized(),
        [changed(
            1,
            0,
            Lead::SmpteOffset(at),
            Placement::NotInFirstTrack
        )]
    );
    let second = read(2, &tracks);
    assert_eq!(
        (second.smpte_offset(0), second.smpte_offset(1)),
        (Some(at), Some(at))
    );
    assert_eq!(second.normalized(), []);
}

// ---- Sequencer-Specific -----------------------------------------------------

/// `FF 7F len data`: the data starts with one manufacturer id byte, or after a
/// `00` three.
/// Spec: smf/sequencer-specific#1
#[test]
fn a_sequencer_specific_event_starts_with_its_manufacturer_id() {
    let one_byte = [0x7D, 1, 2];
    let three = [0x00, 0x00, 0x41, 9, 8];
    let file = read(0, &[vec![meta(0, 0x7F, &one_byte), meta(0, 0x7F, &three)]]);
    let events = file.tracks()[0].events();
    assert_eq!(
        events[0].1,
        ev(Meta::SequencerSpecific {
            manufacturer: ManufacturerId::of(0x7D),
            data: Box::new([1, 2])
        })
    );
    assert_eq!(
        events[1].1,
        ev(Meta::SequencerSpecific {
            manufacturer: ManufacturerId::of(0x8041),
            data: Box::new([9, 8])
        })
    );
    let want: Vec<u8> = [
        &[0, 0xFF, 0x7F, 3][..],
        &one_byte,
        &[0, 0xFF, 0x7F, 5],
        &three,
    ]
    .concat();
    assert_eq!(body(&file)[..want.len()], want);
    // No id, an id of zeros, and a byte with the top bit set are no manufacturer.
    for data in [&[][..], &[0, 0, 0], &[0, 1], &[0x80]] {
        let file = read(0, &[vec![meta(0, 0x7F, data)]]);
        assert_eq!(held(&file.tracks()[0].events()[0].1), malformed(0x7F, data));
    }
}

// ---- text -------------------------------------------------------------------

/// A program without non-ASCII characters ignores them: the ASCII view of a
/// text event drops the bytes with the top bit set, UTF-8 or Latin-1, and the
/// event keeps all of its bytes.
/// Spec: smf/text-event#5
#[test]
fn the_ascii_view_of_a_text_event_leaves_out_non_ascii_bytes() {
    let utf8 = Meta::text(TextKind::Lyric, "caf\u{e9}!");
    let latin1 = Meta::Text {
        kind: TextKind::Text,
        bytes: Box::new(*b"caf\xE9!"),
    };
    assert_eq!(utf8.ascii_text().as_deref(), Some("caf!"));
    assert_eq!(latin1.ascii_text().as_deref(), Some("caf!"));
    let Meta::Text { bytes, .. } = &utf8 else {
        unreachable!()
    };
    assert_eq!(bytes.len(), 6);
    assert_eq!(Meta::tempo(Bpm::of(120.0)).ascii_text(), None);
}

/// A track opens with its copyright notice, Sequence Number, Track Name and
/// SMPTE Offset at tick 0, in that order whichever order they were given: the
/// order is tutti's, as the specification orders only the notice (first) and
/// puts the others at the start.
#[test]
fn a_track_opens_with_copyright_number_name_and_smpte_offset() {
    let at = offset(1, 0, 0, 0, SmpteFrameRate::Fps25, 0);
    let t = Track::new()
        .smpte_start(at)
        .named("N")
        .numbered(1)
        .copyright("C");
    let file = smf(SmfFormat::SingleTrack, vec![t]).unwrap();
    assert_eq!(
        body(&file),
        [
            0x00,
            0xFF,
            0x02,
            0x01,
            b'C', //
            0x00,
            0xFF,
            0x00,
            0x02,
            0x00,
            0x01, //
            0x00,
            0xFF,
            0x03,
            0x01,
            b'N', //
            0x00,
            0xFF,
            0x54,
            0x05,
            1 << 5 | 1,
            0,
            0,
            0,
            0, //
            0x00,
            0xFF,
            0x2F,
            0x00,
        ]
    );
    assert!(file.tracks()[0].events().is_empty());
}

/// All of a file's copyright notices go together in one event, the first event
/// of the first track at time 0: the track holds one, a second joins it on a new
/// line, and it is written before the events however early they were added. A
/// writer refuses a notice in another track.
/// Spec: smf/copyright-notice#3, smf/copyright-notice#4, smf/meta-events#9
#[test]
fn all_copyright_notices_are_one_event_first_in_the_first_track() {
    let t = played().copyright("(C) A").copyright("(C) B");
    assert_eq!(t.copyright_bytes(), Some(&b"(C) A\n(C) B"[..]));
    let file = smf(SmfFormat::SingleTrack, vec![t]).unwrap();
    assert_eq!(
        body(&file)[..14],
        [
            0, 0xFF, 0x02, 0x0B, b'(', b'C', b')', b' ', b'A', b'\n', b'(', b'C', b')', b' '
        ]
    );
    assert_eq!(body(&file)[14..19], [b'B', 0x00, 0x90, 60, 100]);
    let other = |format| smf(format, vec![played(), played().copyright("(C)")]).map(drop);
    for format in [SmfFormat::Parallel, SmfFormat::Sequential] {
        assert_eq!(
            other(format),
            Err(TracksError::CopyrightNotInFirstTrack { track: 1 })
        );
    }
}

// ---- track chunks -----------------------------------------------------------

/// A track chunk may carry all 16 channels.
/// Spec: smf/chunks#9
#[test]
fn a_track_carries_16_channels() {
    let mut t = Track::new();
    for c in 0..16u8 {
        t = t.note(
            Tick(u64::from(c)),
            Tick(u64::from(c) + 5),
            MidiChannel::of(c),
            NoteNumber::of(60),
            U7::of(100),
        );
    }
    let file = smf(SmfFormat::SingleTrack, vec![t]).unwrap();
    let back = MidiFile::from_bytes(&file.to_bytes()).unwrap();
    let channels: Vec<u8> = back.tracks()[0]
        .notes()
        .iter()
        .map(|n| n.channel.get())
        .collect();
    assert_eq!(channels, (0..16).collect::<Vec<u8>>());
}

/// A track chunk is a type, a length and one or more events: a writer gives an
/// empty track its End of Track, and a reader keeps a chunk with no event as an
/// empty track.
/// Spec: smf/track-chunks#2
#[test]
fn a_track_chunk_always_has_an_event() {
    let written = smf(SmfFormat::SingleTrack, vec![Track::new()]).unwrap();
    assert_eq!(body(&written), [0x00, 0xFF, 0x2F, 0x00]);
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    bytes.truncate(bytes.len() - 4);
    bytes[18..22].copy_from_slice(&0u32.to_be_bytes());
    let read = MidiFile::from_bytes(&bytes).unwrap();
    assert!(read.tracks()[0].events().is_empty());
    assert_eq!(read.tracks()[0].end(), Tick(0));
}

/// A System Exclusive message ends in `F7` however a device sent it: whole in an
/// `F0` event, whose `F7` the writer adds, or in packets with the last ending
/// it, and nothing that is transmitted between its packets (a meta event may
/// be). A writer refuses a divided message that is not ended or is interrupted,
/// and a lone `F7` escape needs no `F7` at its end.
/// Spec: smf/track-chunks#12, smf/track-chunks#18, smf/track-chunks#19, smf/track-chunks#20
#[test]
fn a_sysex_message_ends_in_f7() {
    let escape = |d: &[u8]| FileEvent::Escape(d.into());
    let marker = ev(Meta::text(TextKind::Marker, "m"));
    let check = |events| one(events).map(drop);
    let file = one(vec![(0, complete(&[0x43, 0x12, 0x00, 0x07]))]).unwrap();
    assert_eq!(
        body(&file)[..8],
        [0x00, 0xF0, 0x05, 0x43, 0x12, 0x00, 0x07, 0xF7]
    );
    let ended = |t| {
        Err(TracksError::SysExNotEnded {
            track: 0,
            at: Tick(t),
        })
    };
    let interrupted = |t| {
        Err(TracksError::SysExInterrupted {
            track: 0,
            at: Tick(t),
        })
    };
    assert_eq!(check(vec![(5, start(&[0x43, 0x12]))]), ended(5));
    assert_eq!(check(vec![(5, start(&[]))]), ended(5));
    // Divided: the last packet ends it.
    let divided = |last: &[u8]| {
        vec![
            (0, start(&[0x43])),
            (1, escape(&[0x12])),
            (2, marker.clone()),
            (3, escape(last)),
        ]
    };
    assert_eq!(check(divided(&[0x00, 0xF7])), Ok(()));
    assert_eq!(check(divided(&[0x00])), ended(0));
    // A channel message or another `F0` inside it.
    assert_eq!(
        check(vec![(0, start(&[1])), (2, note(0, 60))]),
        interrupted(2)
    );
    assert_eq!(
        check(vec![(0, start(&[1])), (2, complete(&[2]))]),
        interrupted(2)
    );
    // An escape with no open message needs no `F7`.
    let lone = vec![
        (0, escape(&[0xF3, 1])),
        (0, complete(&[0x43])),
        (1, escape(&[9])),
    ];
    assert_eq!(check(lone), Ok(()));
}

// ---- what a read changes ------------------------------------------------------

/// A Sequence Number or an SMPTE Offset after a channel message, and a Track
/// Name or copyright notice after tick 0, are not where SMF 1.0 section 3.3
/// puts them: a read drops each and lists it as `Late`, and the file then
/// writes and reads back with nothing to change.
#[test]
fn a_late_opening_meta_is_dropped_and_listed() {
    let at = offset(1, 0, 0, 0, SmpteFrameRate::Fps25, 0);
    let file = read(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            meta(0, 0x00, &[0, 4]),
            meta(0, 0x54, &[1 << 5 | 1, 0, 0, 0, 0]),
            meta(5, 0x03, b"late"),
            meta(475, 0x02, b"(C)"),
        ]],
    );
    let t = &file.tracks()[0];
    assert_eq!(
        (t.sequence_number(), t.name(), t.copyright_bytes()),
        (None, None, None)
    );
    assert_eq!(file.smpte_offset(0), None);
    assert_eq!(t.events().len(), 1);
    assert_eq!(
        file.normalized(),
        [
            changed(0, 0, Lead::SequenceNumber(4), Placement::Late),
            changed(0, 0, Lead::SmpteOffset(at), Placement::Late),
            changed(
                0,
                5,
                Lead::TrackName(b"late".as_slice().into()),
                Placement::Late
            ),
            changed(
                0,
                480,
                Lead::Copyright(b"(C)".as_slice().into()),
                Placement::Late
            ),
        ]
    );
    let again = MidiFile::from_bytes(&file.to_bytes()).unwrap();
    assert_eq!((&again, again.normalized()), (&file, [].as_slice()));
}

/// A Track Name or copyright notice at tick 0 but after a channel message is at
/// time 0 as the specification asks: it becomes the track's field, written
/// first, and the read lists it as moved. One before any such event is where it
/// belongs and is not listed.
#[test]
fn an_opening_meta_at_time_0_after_an_event_is_moved() {
    let moved = read(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            meta(0, 0x03, b"n"),
            meta(0, 0x02, b"(C)"),
        ]],
    );
    let t = &moved.tracks()[0];
    assert_eq!(
        (t.name(), t.copyright_bytes()),
        (Some("n"), Some(&b"(C)"[..]))
    );
    assert_eq!(t.events().len(), 1);
    assert_eq!(
        moved.normalized(),
        [
            changed(
                0,
                0,
                Lead::TrackName(b"n".as_slice().into()),
                Placement::Moved
            ),
            changed(
                0,
                0,
                Lead::Copyright(b"(C)".as_slice().into()),
                Placement::Moved
            ),
        ]
    );
    assert_eq!(
        body(&moved)[..9],
        [0, 0xFF, 0x02, 3, b'(', b'C', b')', 0, 0xFF]
    );
    let first = read(
        0,
        &[vec![meta(0, 0x03, b"n"), RawEvent::note_on(0, 0, 60, 100)]],
    );
    assert_eq!(first.normalized(), []);
}

/// A track holds one of each: a second Sequence Number, Track Name, SMPTE
/// Offset or copyright notice is dropped and listed as a duplicate, the first
/// kept.
#[test]
fn a_second_opening_meta_is_a_listed_duplicate() {
    let file = read(
        0,
        &[vec![
            meta(0, 0x00, &[0, 1]),
            meta(0, 0x00, &[0, 2]),
            meta(0, 0x03, b"a"),
            meta(0, 0x03, b"b"),
            meta(0, 0x02, b"(C) 1"),
            meta(0, 0x02, b"(C) 2"),
        ]],
    );
    let t = &file.tracks()[0];
    assert_eq!(
        (t.sequence_number(), t.name(), t.copyright_bytes()),
        (Some(1), Some("a"), Some(&b"(C) 1"[..]))
    );
    assert_eq!(
        file.normalized(),
        [
            changed(0, 0, Lead::SequenceNumber(2), Placement::Duplicate),
            changed(
                0,
                0,
                Lead::TrackName(b"b".as_slice().into()),
                Placement::Duplicate
            ),
            changed(
                0,
                0,
                Lead::Copyright(b"(C) 2".as_slice().into()),
                Placement::Duplicate
            ),
        ]
    );
}

/// A copyright notice is in the first track: one in another track is dropped
/// and listed, in every format.
#[test]
fn a_copyright_notice_outside_the_first_track_is_dropped_and_listed() {
    let tracks = [vec![meta(0, 0x02, b"(C)")], vec![meta(0, 0x02, b"(C)")]];
    for format in [1, 2] {
        let file = read(format, &tracks);
        assert_eq!(
            (
                file.tracks()[0].copyright_bytes(),
                file.tracks()[1].copyright_bytes()
            ),
            (Some(&b"(C)"[..]), None)
        );
        assert_eq!(
            file.normalized(),
            [changed(
                1,
                0,
                Lead::Copyright(b"(C)".as_slice().into()),
                Placement::NotInFirstTrack
            )]
        );
    }
}

/// A divided System Exclusive message that is interrupted by a channel message
/// or another `F0` event, or never ended by an `F7` packet, is closed: its first
/// packet is a whole message and the writer ends it with `F7`. A divided message
/// that is ended is read as it is.
#[test]
fn an_unended_divided_sysex_is_closed_and_listed() {
    let closed = |at| Normalized::SysExClosed {
        track: 0,
        at: Tick(at),
    };
    // Never ended.
    let file = read(0, &[vec![RawEvent::new(7, [0xF0, 2, 1, 2])]]);
    assert_eq!(file.tracks()[0].events(), [(Tick(7), complete(&[1, 2]))]);
    assert_eq!(file.normalized(), [closed(7)]);
    assert_eq!(body(&file)[..6], [7, 0xF0, 3, 1, 2, 0xF7]);
    // Interrupted by a channel message, and by another `F0`.
    let file = read(
        0,
        &[vec![
            RawEvent::new(0, [0xF0, 2, 1, 2]),
            RawEvent::note_on(3, 0, 60, 100),
            RawEvent::new(0, [0xF0, 2, 3, 4]),
            RawEvent::new(0, [0xF0, 2, 5, 0xF7]),
        ]],
    );
    let events: Vec<_> = file.tracks()[0].events().iter().map(|(_, e)| e).collect();
    assert_eq!(events[0], &complete(&[1, 2]));
    assert_eq!(events[2], &complete(&[3, 4]));
    assert_eq!(events[3], &complete(&[5]));
    assert_eq!(file.normalized(), [closed(0), closed(3)]);
    let again = MidiFile::from_bytes(&file.to_bytes()).unwrap();
    assert_eq!((&again, again.normalized()), (&file, [].as_slice()));
    // Ended: the packets stay as the file has them.
    let ended = read(
        0,
        &[vec![
            RawEvent::new(0, [0xF0, 2, 1, 2]),
            RawEvent::new(1, [0xF7, 2, 3, 0xF7]),
        ]],
    );
    assert_eq!(ended.tracks()[0].events()[0].1, start(&[1, 2]));
    assert_eq!(ended.normalized(), []);
}

// ---- formats ----------------------------------------------------------------

/// In a format 1 file the tempo map (Set Tempo and Time Signature) is the first
/// track: a writer refuses it in another track, a format 2 file has one in each
/// pattern, and a reader keeps a file with it elsewhere and its tempo map pools
/// every track.
/// Spec: smf/formats#7, smf/formats#9, smf/formats#14
#[test]
fn a_format_1_tempo_map_is_the_first_track() {
    let time = ev(Meta::TimeSignature {
        numerator: 3,
        denominator_power: 2,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    });
    let tracks = |event: FileEvent| vec![played(), track(vec![(480, event)])];
    for event in [tempo(90.0), time] {
        assert_eq!(
            smf(SmfFormat::Parallel, tracks(event.clone())).map(drop),
            Err(TracksError::TempoMapNotInFirstTrack {
                track: 1,
                at: Tick(480)
            })
        );
        assert!(smf(SmfFormat::Sequential, tracks(event)).is_ok());
    }
    let first = vec![track(vec![(0, tempo(90.0))]), played()];
    assert!(smf(SmfFormat::Parallel, first).is_ok());
    let kept = read(
        1,
        &[
            vec![RawEvent::note_on(0, 0, 60, 100)],
            vec![RawEvent::tempo(0, 1_000_000)],
        ],
    );
    assert_eq!(
        metrical(&kept).tempo_map().tempo_at(Beat(0.0)),
        Bpm::of(60.0)
    );
}

/// In a format 0 file the tempo is scattered through the one track, and the tempo
/// map is the tempo events and none of what is between them.
/// Spec: smf/formats#8
#[test]
fn a_format_0_tempo_map_is_the_scattered_tempos() {
    let file = read(
        0,
        &[vec![
            RawEvent::tempo(0, 500_000),
            RawEvent::note_on(BEAT, 0, 60, 100),
            RawEvent::tempo(BEAT, 1_000_000),
            RawEvent::note_off(BEAT, 0, 60, 0),
            RawEvent::time_signature(BEAT, 3, 2),
            RawEvent::tempo(BEAT, 250_000),
        ]],
    );
    let map = metrical(&file).tempo_map().clone();
    assert_eq!(map.points().len(), 3);
    let at = |beat| map.tempo_at(Beat(beat));
    assert_eq!(
        (at(0.5), at(2.5), at(5.5)),
        (Bpm::of(120.0), Bpm::of(60.0), Bpm::of(240.0))
    );
}

/// A file that specifies none is given the tempo and time signature a reader
/// assumes (120 in 4/4): in a format 0 track, the first track of a format 1
/// file (the one that holds the tempo map), and each pattern of a format 2 file.
/// What it already has at tick 0 stays.
#[test]
fn a_file_is_made_to_specify_tempo_and_time_signature() {
    let signature = ev(Meta::TimeSignature {
        numerator: 4,
        denominator_power: 2,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    });
    let head = |file: &MidiFile, track: usize| -> Vec<FileEvent> {
        let events = file.tracks()[track].events();
        events.iter().take(2).map(|(_, e)| e.clone()).collect()
    };
    let defaults = vec![tempo(120.0), signature.clone()];
    let specify = |f: MidiFile| f.with_default_tempo_and_meter().unwrap();
    let format_0 = specify(smf(SmfFormat::SingleTrack, vec![played()]).unwrap());
    assert_eq!(head(&format_0, 0), defaults);
    let format_1 = specify(smf(SmfFormat::Parallel, vec![played(), played()]).unwrap());
    assert_eq!(head(&format_1, 0), defaults);
    assert_eq!(format_1.tracks()[1], played());
    let format_2 = specify(smf(SmfFormat::Sequential, vec![played(), played()]).unwrap());
    assert_eq!(
        (head(&format_2, 0), head(&format_2, 1)),
        (defaults.clone(), defaults)
    );
    // A copyright notice and a tempo already there stay; the signature joins.
    let has = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(90.0)))
        .copyright("c");
    let kept = specify(smf(SmfFormat::SingleTrack, vec![has]).unwrap());
    assert_eq!(kept.tracks()[0].copyright_bytes(), Some(&b"c"[..]));
    assert_eq!(head(&kept, 0), [tempo(90.0), signature]);
}
