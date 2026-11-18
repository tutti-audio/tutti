//! The rules SMF 1.0 puts on what a track holds and where: the typed Sequence
//! Number, MIDI Channel Prefix, SMPTE Offset and Sequencer-Specific events,
//! the SysEx and placement rules a writer keeps (and a reader does not insist
//! on), and the tempo and time signature a file specifies. Each figure is a
//! byte or a value written from the specification.

use crate::common::{metrical, note, sequences, track};
use crate::support::{Division, RawEvent, build_smf};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_midi_file::{
    FileEvent, Meta, MidiFile, SmfDivision, SmfFormat, SmpteFractionalFrames, SmpteOffset,
    TextKind, Tick, Track, TracksError,
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

fn number_of(n: u16) -> FileEvent {
    ev(Meta::sequence_number(n))
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
    assert_eq!(
        file.tracks()[0].events()[0].1,
        ev(Meta::sequence_number(0x0102))
    );
    assert_eq!(
        body(&file),
        [0x00, 0xFF, 0x00, 0x02, 0x01, 0x02, 0x00, 0xFF, 0x2F, 0x00]
    );
}

/// A Sequence Number is at the start of its track: at time 0, before any channel
/// message, SysEx or escape; a Track Name is at time 0. A writer refuses
/// otherwise and a reader keeps the file, the late event not numbering or naming
/// the track.
/// Spec: smf/sequence-number#1, smf/meta-events#10
#[test]
fn a_sequence_number_and_a_track_name_are_at_the_start() {
    let name = |at| (at, ev(Meta::text(TextKind::TrackName, "n")));
    let first = (0, ev(Meta::text(TextKind::Text, "t")));
    assert!(one(vec![first, (0, number_of(1))]).is_ok());
    let refused = |events| one(events).map(drop);
    let at = |t| {
        Err(TracksError::SequenceNumberNotAtStart {
            track: 0,
            at: Tick(t),
        })
    };
    assert_eq!(refused(vec![(0, note(0, 60)), (0, number_of(1))]), at(0));
    assert_eq!(refused(vec![(5, number_of(1))]), at(5));
    assert_eq!(
        refused(vec![
            (0, FileEvent::SysEx(Box::new([0xF7]))),
            (0, number_of(1))
        ]),
        at(0)
    );
    assert_eq!(
        refused(vec![name(7)]),
        Err(TracksError::TrackNameNotAtStart {
            track: 0,
            at: Tick(7)
        })
    );
    assert!(one(vec![name(0)]).is_ok());
    // The builder puts it first, however late it is added.
    let built = played().numbered(9);
    assert_eq!(built.events()[0], (Tick(0), number_of(9)));
    assert!(smf(SmfFormat::SingleTrack, vec![built]).is_ok());

    let late = read(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            meta(0, 0x00, &[0, 4]),
            meta(5, 0x03, b"late"),
        ]],
    );
    assert_eq!(late.tracks()[0].sequence_number(), None);
    assert_eq!(late.tracks()[0].name(), None);
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
/// A writer refuses a number in another track; a reader keeps it.
/// Spec: smf/sequence-number#4
#[test]
fn a_format_1_file_has_one_sequence_number_in_its_first_track() {
    let later = |format| smf(format, vec![played(), played().numbered(1)]).map(drop);
    assert_eq!(
        later(SmfFormat::Parallel),
        Err(TracksError::SequenceNumberNotInFirstTrack { track: 1 })
    );
    assert_eq!(later(SmfFormat::Sequential), Ok(()));
    let kept = read(
        1,
        &[
            vec![RawEvent::note_on(0, 0, 60, 100)],
            vec![meta(0, 0x00, &[0, 3])],
        ],
    );
    assert_eq!(kept.tracks()[1].sequence_number(), Some(3));
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
            file.tracks()[0].events()[0].1,
            ev(Meta::smpte_offset(offset(23, 59, 58, last_frame, rate, 99)))
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
/// message: a writer refuses one later, a reader does not count one later as
/// the track's, and the builder puts it first, behind a copyright notice.
/// Spec: smf/smpte-offset#2
#[test]
fn an_smpte_offset_is_at_the_start_of_the_track() {
    let at = offset(1, 0, 0, 0, SmpteFrameRate::Fps25, 0);
    let late = |events| one(events).map(drop);
    let not_at_start = |t| {
        Err(TracksError::SmpteOffsetNotAtStart {
            track: 0,
            at: Tick(t),
        })
    };
    let smpte = || ev(Meta::smpte_offset(at));
    assert_eq!(late(vec![(480, smpte())]), not_at_start(480));
    assert_eq!(late(vec![(0, note(0, 60)), (0, smpte())]), not_at_start(0));
    assert_eq!(late(vec![(0, smpte()), (0, note(0, 60))]), Ok(()));
    let kept = read(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            meta(0, 0x54, &[1 << 5 | 1, 0, 0, 0, 0]),
        ]],
    );
    assert_eq!(kept.smpte_offset(0), None);
    let delayed = read(0, &[vec![meta(5, 0x54, &[1 << 5 | 1, 0, 0, 0, 0])]]);
    assert_eq!(delayed.smpte_offset(0), None);
    let t = played().copyright("(C)").smpte_start(at);
    let events = t.events();
    assert_eq!(
        events[0],
        (Tick(0), ev(Meta::text(TextKind::Copyright, "(C)")))
    );
    assert_eq!(events[1], (Tick(0), ev(Meta::smpte_offset(at))));
    assert_eq!(events.len(), 4);
}

/// In a format 1 file the offset is stored in the first track and means
/// nothing in the others: a writer refuses it there, and a reader reads none.
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
    let second = read(2, &tracks);
    assert_eq!(
        (second.smpte_offset(0), second.smpte_offset(1)),
        (Some(at), Some(at))
    );
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

/// The builder puts a copyright notice first at tick 0 and keeps a track's
/// notices in one event.
#[test]
fn the_copyright_builder_keeps_one_event_first() {
    let t = played()
        .copyright("(C) 2026 A")
        .numbered(1)
        .copyright("(C) 2026 B");
    let want = Meta::text(TextKind::Copyright, "(C) 2026 A\n(C) 2026 B");
    assert_eq!(t.events()[0], (Tick(0), ev(want)));
    assert_eq!(t.events()[1], (Tick(0), number_of(1)));
}

fn notice(s: &str) -> FileEvent {
    ev(Meta::text(TextKind::Copyright, s))
}

/// The copyright notice is the first event of the first track, at time 0: a
/// writer refuses one after another event at tick 0, or in another track.
/// Spec: smf/copyright-notice#4
#[test]
fn a_copyright_notice_is_the_first_event_of_the_first_track() {
    let not_first = |track, t| Err(TracksError::CopyrightNotFirst { track, at: Tick(t) });
    let check = |format, tracks| smf(format, tracks).map(drop);
    let first = track(vec![(0, notice("(C)")), (0, note(0, 60))]);
    assert_eq!(check(SmfFormat::SingleTrack, vec![first]), Ok(()));
    let after = track(vec![(0, note(0, 60)), (0, notice("(C)"))]);
    assert_eq!(check(SmfFormat::SingleTrack, vec![after]), not_first(0, 0));
    let other = track(vec![(0, notice("(C)"))]);
    assert_eq!(
        check(SmfFormat::Parallel, vec![played(), other]),
        not_first(1, 0)
    );
}

/// A copyright notice is placed as early as possible: a writer refuses one
/// after the start of the track.
/// Spec: smf/meta-events#9
#[test]
fn a_copyright_notice_is_not_placed_late() {
    let late = one(vec![(0, note(0, 60)), (480, notice("(C)"))]).map(drop);
    assert_eq!(
        late,
        Err(TracksError::CopyrightNotFirst {
            track: 0,
            at: Tick(480)
        })
    );
}

/// All of a file's copyright notices are one event: a writer refuses a second,
/// in the track or another.
/// Spec: smf/copyright-notice#3
#[test]
fn all_copyright_notices_are_one_event() {
    let two = |tracks| smf(SmfFormat::Sequential, tracks).map(drop);
    let together = |track| Err(TracksError::CopyrightNotTogether { track, at: Tick(0) });
    let both = track(vec![(0, notice("(C) A")), (0, notice("(C) B"))]);
    assert_eq!(two(vec![both]), together(0));
    let a = || track(vec![(0, notice("(C)"))]);
    assert_eq!(two(vec![a(), a()]), together(1));
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
/// `F0` event, or in packets with the last ending it, and nothing that is
/// transmitted between its packets (a meta event may be). A writer refuses what
/// does not, and a lone `F7` escape needs no `F7` at its end.
/// Spec: smf/track-chunks#12, smf/track-chunks#18, smf/track-chunks#19, smf/track-chunks#20
#[test]
fn a_sysex_message_ends_in_f7() {
    let sysex = |d: &[u8]| FileEvent::SysEx(d.into());
    let escape = |d: &[u8]| FileEvent::Escape(d.into());
    let marker = ev(Meta::text(TextKind::Marker, "m"));
    let check = |events| one(events).map(drop);
    let whole = [0x43, 0x12, 0x00, 0x07, 0xF7];
    let file = one(vec![(0, sysex(&whole))]).unwrap();
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
    assert_eq!(check(vec![(5, sysex(&[0x43, 0x12]))]), ended(5));
    assert_eq!(check(vec![(5, sysex(&[]))]), ended(5));
    // Divided: the last packet ends it.
    let divided = |last: &[u8]| {
        vec![
            (0, sysex(&[0x43])),
            (1, escape(&[0x12])),
            (2, marker.clone()),
            (3, escape(last)),
        ]
    };
    assert_eq!(check(divided(&[0x00, 0xF7])), Ok(()));
    assert_eq!(check(divided(&[0x00])), ended(3));
    // A channel message or another `F0` inside it.
    assert_eq!(
        check(vec![(0, sysex(&[1])), (2, note(0, 60))]),
        interrupted(2)
    );
    assert_eq!(
        check(vec![(0, sysex(&[1])), (2, sysex(&[2, 0xF7]))]),
        interrupted(2)
    );
    // An escape with no open message needs no `F7`.
    let lone = vec![
        (0, escape(&[0xF3, 1])),
        (0, sysex(&whole)),
        (1, escape(&[9])),
    ];
    assert_eq!(check(lone), Ok(()));
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
    let head = head(&kept, 0);
    assert_eq!(head[0], ev(Meta::text(TextKind::Copyright, "c")));
    assert_eq!(head[1], tempo(90.0));
    assert_eq!(kept.tracks()[0].events()[2].1, signature);
}
