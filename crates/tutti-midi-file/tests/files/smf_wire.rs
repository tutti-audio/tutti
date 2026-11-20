//! The SMF byte layer: variable-length quantities, running status, framing
//! and the writer's byte output, against bytes built from the SMF 1.0 spec
//! (`support`, which shares no code with the crate).

use crate::common::{self, metrical, note, track, unspecified, without_defaults};
use crate::common::{ToBytes, read_bytes};
use crate::support::{Division, RawEvent, build_smf};
use tutti_midi_file::{
    DropReason, FileEvent, Meta, MicrosPerQuarter, MidiFile, ReadError, SmfDivision, SmfFormat,
    SmfParseError, TextKind, Tick,
};
use tutti_ump::{Message, TicksPerQuarterNote, Voice1};
use tutti_units::Seconds;

const TPQ: u16 = 96;
const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(TPQ);
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);

fn file(events: Vec<RawEvent>) -> Vec<u8> {
    build_smf(0, Division::Metrical(TPQ), &[events])
}

/// A format 0 file whose one track body is exactly `body` (no End of Track
/// added), for bytes the builder cannot spell.
fn raw_track(body: &[u8]) -> Vec<u8> {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    bytes.truncate(14);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn ticks(bytes: &[u8]) -> Vec<u64> {
    let file = read_bytes(bytes).expect("reads");
    unspecified(&file, 0).iter().map(|(t, _)| t.0).collect()
}

fn voices(bytes: &[u8]) -> Vec<Voice1> {
    let file = read_bytes(bytes).expect("reads");
    unspecified(&file, 0)
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Message(Message::Midi1(_, v)) => Some(*v),
            _ => None,
        })
        .collect()
}

// ---- variable-length quantities (SMF 1.0 section 1.1) ----------------------

/// The spec's table of quantities and their bytes, every row: 0 is `00`, `7F`
/// is `7F`, `80` is `81 00`, `0FFFFFFF` is `FF FF FF 7F`.
const QUANTITIES: [(u32, &[u8]); 12] = [
    (0x0000_0000, &[0x00]),
    (0x0000_0040, &[0x40]),
    (0x0000_007F, &[0x7F]),
    (0x0000_0080, &[0x81, 0x00]),
    (0x0000_2000, &[0xC0, 0x00]),
    (0x0000_3FFF, &[0xFF, 0x7F]),
    (0x0000_4000, &[0x81, 0x80, 0x00]),
    (0x0010_0000, &[0xC0, 0x80, 0x00]),
    (0x001F_FFFF, &[0xFF, 0xFF, 0x7F]),
    (0x0020_0000, &[0x81, 0x80, 0x80, 0x00]),
    (0x0800_0000, &[0xC0, 0x80, 0x80, 0x00]),
    (0x0FFF_FFFF, &[0xFF, 0xFF, 0xFF, 0x7F]),
];

/// A delta of each quantity reads to its value, spelled by the spec's bytes
/// and not by the support encoder.
/// Spec: smf/conventions#1, smf/conventions#2, smf/conventions#3, smf/conventions#4, smf/conventions#5, smf/conventions#6, smf/track-chunks#3
#[test]
fn a_quantity_reads_to_its_value() {
    for (value, bytes) in QUANTITIES {
        // A delta, then a Note On: `delta 90 3C 40`.
        let body = [bytes, &[0x90, 0x3C, 0x40]].concat();
        assert_eq!(ticks(&raw_track(&body)), [u64::from(value)], "{value:#x}");
    }
}

/// A delta of each quantity is written as the spec's bytes: the shortest
/// spelling, no leading zero groups.
/// Spec: smf/conventions#1, smf/conventions#2, smf/conventions#3, smf/conventions#4, smf/conventions#5, smf/conventions#6, smf/track-chunks#3
#[test]
fn a_quantity_writes_to_its_bytes() {
    for (value, bytes) in QUANTITIES {
        let written = crate::common::format0(
            SMF_DIVISION,
            track(vec![(
                u64::from(value),
                FileEvent::Meta(Meta::Text {
                    kind: TextKind::Text,
                    bytes: Box::default(),
                }),
            )]),
        )
        .unwrap()
        .to_bytes();
        let written = without_defaults(&written);
        // 14-byte header, 8-byte track header, then the delta.
        assert_eq!(written.get(22..22 + bytes.len()), Some(bytes), "{value:#x}");
    }
}

/// A quantity over four bytes is not a quantity: refused, as a parse error
/// and not a panic or a wrapped value.
/// Spec: smf/conventions#7
#[test]
fn a_five_byte_quantity_is_refused() {
    let body = [0x81, 0x80, 0x80, 0x80, 0x00, 0x90, 0x3C, 0x40];
    assert!(matches!(
        read_bytes(raw_track(&body)),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 22,
            cause: SmfParseError::VariableLengthQuantity
        })
    ));
    // And one cut off mid-quantity.
    assert!(matches!(
        read_bytes(raw_track(&[0x81, 0x80])),
        Err(ReadError::Smf { .. })
    ));
}

// ---- running status (SMF 1.0 section 3) ------------------------------------

/// A channel event without its status byte repeats the last one's.
/// Spec: smf/track-chunks#4, smf/track-chunks#6
#[test]
fn running_status_repeats_the_last_channel_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 2, 60, 100),
        RawEvent::running_status(10, vec![62, 90]),
        RawEvent::running_status(10, vec![64, 80]),
    ]);
    let notes: Vec<_> = voices(&bytes)
        .into_iter()
        .filter_map(|v| match v {
            Voice1::NoteOn { note, velocity } => Some((note.get(), velocity.get())),
            _ => None,
        })
        .collect();
    assert_eq!(notes, [(60, 100), (62, 90), (64, 80)]);
    assert_eq!(ticks(&bytes), [0, 10, 20]);
}

/// A meta event cancels running status: a data byte after one has none.
/// (The bytes are chosen so that if the status were `FF`, they would read as
/// a valid meta event: type `3E`, length 0.)
/// Spec: smf/track-chunks#30, smf/track-chunks#31
#[test]
fn a_meta_event_cancels_running_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::track_name(0, "x"),
        RawEvent::running_status(0, vec![0x3E, 0x00]),
    ]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 31,
            cause: SmfParseError::NoRunningStatus
        })
    ));
}

/// A sysex event cancels running status the same way (under a surviving
/// `F0` status the byte `00` would be a valid empty sysex). The sysex is six
/// bytes with its `F7`, so the refused event starts at byte 32.
/// Spec: smf/track-chunks#30, smf/track-chunks#31
#[test]
fn a_sysex_cancels_running_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::sysex(0, &[0x7E, 0x00]),
        RawEvent::running_status(0, vec![0x00]),
    ]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 32,
            cause: SmfParseError::NoRunningStatus
        })
    ));
}

/// A first event with no status and none running is malformed.
/// Spec: smf/track-chunks#7
#[test]
fn a_first_event_needs_a_status() {
    assert!(matches!(
        read_bytes(raw_track(&[0x00, 0x3C, 0x40])),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 22,
            cause: SmfParseError::NoRunningStatus
        })
    ));
}

/// A status then a different channel status re-establishes: the status after
/// a Program Change (one data byte) is the Program Change's.
#[test]
fn running_status_follows_the_data_length_of_its_status() {
    let bytes = file(vec![
        RawEvent::new(0, vec![0xC1, 5]),
        RawEvent::running_status(0, vec![6]),
    ]);
    assert_eq!(voices(&bytes).len(), 2);
}

/// A real-time or system common status has no place in a file.
#[test]
fn system_statuses_are_refused() {
    for status in [0xF1u8, 0xF2, 0xF8, 0xFE] {
        assert!(
            matches!(
                read_bytes(raw_track(&[0x00, status, 0x00])),
                Err(ReadError::Smf {
                    track: Some(0),
                    offset: 22,
                    cause: SmfParseError::SystemStatus
                })
            ),
            "{status:#x}"
        );
    }
}

/// A data byte with its top bit set inside a channel message is malformed.
#[test]
fn a_high_data_byte_is_malformed() {
    assert!(matches!(
        read_bytes(raw_track(&[0x00, 0x90, 0x3C, 0x80])),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 22,
            cause: SmfParseError::ChannelData
        })
    ));
}

// ---- framing ---------------------------------------------------------------

/// A chunk of an unknown type between tracks is skipped (section 2).
/// Spec: smf/chunks#2, smf/chunks#3, smf/chunks#8
#[test]
fn an_alien_chunk_is_skipped() {
    let mut bytes = file(vec![RawEvent::note_on(0, 0, 60, 100)]);
    bytes.extend_from_slice(b"XFIH");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[1, 2, 3]);
    assert_eq!(voices(&bytes).len(), 1);
}

/// A track chunk past the count the header declares disagrees with it.
/// Spec: smf/header-chunk#9
#[test]
fn more_tracks_than_declared_is_malformed() {
    let two = build_smf(1, Division::Metrical(TPQ), &[vec![], vec![]]);
    let mut bytes = two.clone();
    // The header declares two (bytes 10..12); say one.
    bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: None,
            offset: 26,
            cause: SmfParseError::ExtraTracks
        })
    ));
    assert!(read_bytes(&two).is_ok());
}

/// A header chunk longer than six bytes is read for its first six.
/// Spec: smf/formats#18, smf/header-chunk#2, smf/header-chunk#3
#[test]
fn a_longer_header_is_read() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"MThd");
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 1, 0, TPQ as u8, 0xAA, 0xBB]);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    assert_eq!(
        metrical(&read_bytes(&bytes).unwrap()).ticks_per_quarter(),
        DIVISION
    );
}

// ---- meta events bigger than expected (SMF 1.0 section 3.3) ----------------

/// SMF 1.0 section 3.3: a meta event may be bigger than a program expects, and
/// the program ignores what is past what it knows. A Set Tempo, a Time Signature and a Key Signature with extra data
/// are read as themselves, from the bytes they define.
/// Spec: smf/time-signature#1, smf/track-chunks#27, smf/track-chunks#28
#[test]
fn a_meta_event_longer_than_defined_is_read_for_what_it_defines() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x51, 0x05, 0x07, 0xA1, 0x20, 0xAA, 0xBB, // tempo + 2
        0x00, 0xFF, 0x58, 0x06, 0x06, 0x03, 0x18, 0x08, 0x03, 0x04, // 6/8 + 2
        0x00, 0xFF, 0x59, 0x03, 0xFD, 0x01, 0x99, // 3 flats minor + 1
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let file = read_bytes(&bytes).unwrap();
    let metas: Vec<&Meta> = file.tracks()[0]
        .events()
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Meta(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(
        metas,
        [
            &Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::new([0xAA, 0xBB]),
            },
            &Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 24,
                thirty_seconds_per_quarter: 8,
                extra: Box::new([3, 4]),
            },
            &Meta::KeySignature {
                sharps: -3,
                minor: true,
                extra: Box::new([0x99]),
            },
        ]
    );
}

/// The file model is lossless: the bytes past what a program knows are written
/// back (SMF 1.0 section 3.3: a program adds nothing of its own to a meta event,
/// and so must not drop what is there).
/// Spec: smf/track-chunks#29
#[test]
fn a_longer_meta_event_is_written_back_as_it_was() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x51, 0x05, 0x07, 0xA1, 0x20, 0xAA, 0xBB, //
        0x00, 0xFF, 0x58, 0x06, 0x06, 0x03, 0x18, 0x08, 0x03, 0x04, //
        0x00, 0xFF, 0x59, 0x03, 0xFD, 0x01, 0x99, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let back = read_bytes(&bytes).unwrap().to_bytes();
    assert_eq!(back, bytes);
}

/// SMF 1.0 section 3.3: an end-of-track event is the last event in the
/// track. One with events after it is refused, not read as the track's
/// end (which would drop what follows).
/// Spec: smf/meta-events#11
#[test]
fn an_end_of_track_that_is_not_last_is_refused() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x2F, 0x00, //
        0x00, 0x90, 60, 100, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 22,
            cause: SmfParseError::EndOfTrackNotLast
        })
    ));
}

// ---- the writer ------------------------------------------------------------

fn track_body(events: Vec<(u64, FileEvent)>) -> Vec<u8> {
    let track = track(events);
    let bytes = crate::common::format0(SMF_DIVISION, track)
        .unwrap()
        .to_bytes();
    without_defaults(bytes.get(22..).unwrap())
}

/// Two notes of one status write the status once: `00 90 3C 64`, `00 3E 64`,
/// then End of Track.
/// Spec: smf/track-chunks#5, smf/track-chunks#6
#[test]
fn the_writer_elides_a_repeated_status() {
    assert_eq!(
        track_body(vec![(0, note(0, 60)), (0, note(0, 62))]),
        [0x00, 0x90, 60, 100, 0x00, 62, 100, 0x00, 0xFF, 0x2F, 0x00]
    );
}

/// After a meta event the status is stated again.
/// Spec: smf/track-chunks#30, smf/track-chunks#31
#[test]
fn the_writer_restates_a_status_after_a_meta_event() {
    let meta = FileEvent::Meta(Meta::Text {
        kind: TextKind::Marker,
        bytes: Box::new(*b"m"),
    });
    assert_eq!(
        track_body(vec![(0, note(0, 60)), (0, meta), (0, note(0, 62))]),
        [
            0x00, 0x90, 60, 100, // note
            0x00, 0xFF, 0x06, 0x01, b'm', // marker
            0x00, 0x90, 62, 100, // note, status restated
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
}

/// SMF 1.0 sections 3.1 and 3.3: a SysEx event is `F0`, a length and the bytes
/// (the closing `F7` among them, which the writer adds to a whole message), an
/// escape is `F7`, a length and the bytes.
/// Spec: smf/track-chunks#8, smf/track-chunks#10, smf/track-chunks#13, smf/track-chunks#14
#[test]
fn a_sysex_and_an_escape_are_written_as_their_spec_bytes() {
    assert_eq!(
        track_body(vec![
            (
                0,
                FileEvent::SysEx(common::complete(&[0x7E, 0x7F, 0x09, 0x01]))
            ),
            (0, FileEvent::Escape(Box::new([1, 2, 3]))),
        ]),
        [
            0x00, 0xF0, 0x05, 0x7E, 0x7F, 0x09, 0x01, 0xF7, //
            0x00, 0xF7, 0x03, 0x01, 0x02, 0x03, //
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
}

/// SMF 1.0 section 1.1: a length of 128 or more is a variable-length quantity
/// of two bytes or more (200 is `81 48`, 300 is `82 2C`).
/// Spec: smf/track-chunks#9, smf/track-chunks#24, smf/track-chunks#25
#[test]
fn a_length_of_128_or_more_is_a_multi_byte_quantity() {
    let text = vec![b'a'; 200];
    let want: Vec<u8> = [
        &[0x00, 0xFF, 0x01, 0x81, 0x48][..],
        &text,
        &[0x00, 0xFF, 0x2F, 0x00],
    ]
    .concat();
    let meta = FileEvent::Meta(Meta::Text {
        kind: TextKind::Text,
        bytes: text.into_boxed_slice(),
    });
    assert_eq!(track_body(vec![(0, meta)]), want);

    // 299 bytes and the `F7` the writer adds are 300 on the wire.
    let data = vec![0x11; 299];
    let want: Vec<u8> = [
        &[0x00, 0xF0, 0x82, 0x2C][..],
        &data,
        &[0xF7, 0x00, 0xFF, 0x2F, 0x00],
    ]
    .concat();
    assert_eq!(
        track_body(vec![(0, FileEvent::SysEx(common::complete(&data)))]),
        want
    );
}

/// Everything the SMF holds survives a write and a read, running status
/// included: equality is on the model.
#[test]
fn a_written_file_reads_back_equal() {
    let events = vec![
        (
            0,
            FileEvent::Meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::default(),
            }),
        ),
        (0, note(0, 60)),
        (5, note(0, 62)),
        (200, FileEvent::SysEx(common::complete(&[0x7E, 0x7F]))),
        (200, FileEvent::Escape(Box::new([1, 2]))),
        (300, note(0, 64)),
        (0x0FFF_FFFF, note(0, 65)),
    ];
    let track = track(events);
    let file = crate::common::format1(
        SMF_DIVISION,
        vec![track, crate::common::track(vec![(0, note(1, 60))])],
    )
    .unwrap();
    let back = read_bytes(file.to_bytes()).unwrap();
    assert_eq!(back.tracks(), file.tracks());
    assert_eq!(metrical(&back).ticks_per_quarter(), DIVISION);
}

/// SMF 1.0 section 3.3: Set Tempo is `FF 51 03`, Time Signature `FF 58 04`, Key
/// Signature `FF 59 02` (a mode of 0 or 1) and End of Track `FF 2F 00`. A
/// lenient reader keeps an event of such a type whose data is not that, as
/// `Meta::Malformed` with its bytes, so the file's other events are read; it
/// writes back byte for byte, reads back the same, and `to_clip` drops it.
/// A meta type byte of 0x80 or more is no type (section 3.3: below 128): the
/// reader keeps it the same way.
/// Spec: smf/track-chunks#25
#[test]
fn a_typed_meta_event_with_data_its_type_does_not_define_is_kept_malformed() {
    let cases: [(u8, &[u8]); 7] = [
        (0x51, &[0x07, 0xA1]),       // a tempo of two bytes
        (0x58, &[0x04, 0x02, 0x18]), // a time signature of three
        (0x59, &[0x00]),             // a key signature of one byte
        (0x59, &[0x00, 0x02]),       // a mode of 2
        (0x2F, &[0x00]),             // an End of Track with data
        (0x85, &[0x01, 0x02]),       // a type byte with its top bit set
        (0x58, &[]),                 // a time signature of none
    ];
    for (kind, data) in cases {
        let mut body = vec![0x00, 0xFF, kind, data.len() as u8];
        body.extend_from_slice(data);
        body.extend_from_slice(&[0x00, 0x90, 60, 100, 0x00, 0xFF, 0x2F, 0x00]);
        let read = read_bytes(raw_track(&body)).expect("reads");
        let events = &unspecified(&read, 0);
        // The note after it is read.
        assert_eq!(events.len(), 2, "{kind:#04x} {data:?}");
        let FileEvent::Meta(Meta::Malformed(malformed)) = &events[0].1 else {
            panic!("{kind:#04x} {data:?}: {:?}", events[0].1);
        };
        assert_eq!((malformed.kind(), malformed.data()), (kind, data));
        // Written back as the file had it, and read as the same.
        let written = read.to_bytes();
        let mut event = vec![0xFF, kind, data.len() as u8];
        event.extend_from_slice(data);
        assert!(
            written.windows(event.len()).any(|w| w == event),
            "{kind:#04x} {data:?}"
        );
        assert_eq!(read_bytes(&written).unwrap(), read);
        let converted = metrical(&read).to_clip().unwrap();
        let reasons: Vec<DropReason> = converted.dropped.iter().map(|d| d.reason).collect();
        assert_eq!(reasons, [DropReason::MalformedMeta], "{kind:#04x} {data:?}");
    }
}

/// A second header chunk is refused where it starts: the first chunk after the
/// header is bytes 14.. of the file.
#[test]
fn a_second_header_chunk_is_refused_where_it_starts() {
    let mut bytes = file(vec![]);
    bytes.truncate(14);
    bytes.extend_from_slice(b"MThd");
    bytes.extend_from_slice(&6u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 1, 0, TPQ as u8]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: None,
            offset: 14,
            cause: SmfParseError::SecondHeader
        })
    ));
}

/// A header chunk of four bytes has no division.
/// Spec: smf/header-chunk#2, smf/header-chunk#3
#[test]
fn a_header_shorter_than_six_bytes_is_refused() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"MThd");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 1]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: None,
            offset: 0,
            cause: SmfParseError::ShortHeader
        })
    ));
}

/// A format 0 file is one track (SMF 1.0 section 2.1).
/// Spec: smf/formats#1, smf/header-chunk#6, smf/header-chunk#10
#[test]
fn a_format_0_header_with_two_tracks_is_refused_at_the_header() {
    let two = build_smf(0, Division::Metrical(TPQ), &[vec![], vec![]]);
    assert!(matches!(
        read_bytes(&two),
        Err(ReadError::Smf {
            track: None,
            offset: 0,
            cause: SmfParseError::FormatZeroTracks
        })
    ));
}

/// The text kinds are the meta types 01 to 09 (SMF 1.0 section 3.3): a read of
/// each gives the kind, and a write the same bytes; the copyright notice (02)
/// and the Track Name (03) are the track's fields.
/// Spec: smf/copyright-notice#1, smf/text-event#2, smf/text-event#6, smf/track-chunks#24, smf/track-chunks#25
#[test]
fn every_text_meta_type_reads_and_writes_back() {
    for (meta_type, kind) in [
        (0x01, TextKind::Text),
        (0x04, TextKind::InstrumentName),
        (0x05, TextKind::Lyric),
        (0x06, TextKind::Marker),
        (0x07, TextKind::CuePoint),
        (0x08, TextKind::ProgramName),
        (0x09, TextKind::DeviceName),
    ] {
        let bytes = file(vec![RawEvent::new(0, vec![0xFF, meta_type, 1, b'x'])]);
        let read = read_bytes(&bytes).unwrap();
        assert_eq!(
            unspecified(&read, 0)[0].1,
            FileEvent::Meta(Meta::text(kind, "x")),
            "{meta_type:#x}"
        );
        assert_eq!(without_defaults(&read.to_bytes()), bytes, "{meta_type:#x}");
    }
    for meta_type in [0x02, 0x03] {
        let bytes = file(vec![RawEvent::new(0, vec![0xFF, meta_type, 1, b'x'])]);
        let read = read_bytes(&bytes).unwrap();
        let track = &read.tracks()[0];
        assert_eq!(unspecified(&read, 0), [], "{meta_type:#x}");
        let field = if meta_type == 0x02 {
            read.copyright()
        } else {
            track.name_bytes()
        };
        assert_eq!(field, Some(&b"x"[..]), "{meta_type:#x}");
        assert_eq!(without_defaults(&read.to_bytes()), bytes, "{meta_type:#x}");
    }
}

/// The header's format word is 0, 1 or 2 (SMF 1.0 section 2.1): a read of each
/// writes the word back.
/// Spec: smf/header-chunk#5
#[test]
fn every_format_word_reads_and_writes_back() {
    for word in 0u16..=2 {
        let tracks = if word == 0 {
            vec![vec![]]
        } else {
            vec![vec![], vec![]]
        };
        let bytes = build_smf(word, Division::Metrical(TPQ), &tracks);
        let read = read_bytes(&bytes).unwrap();
        assert_eq!(read.to_bytes()[8..10], word.to_be_bytes(), "format {word}");
    }
}

/// SMF 1.0 section 2.3: a track chunk's length is the bytes of its body. For
/// tracks with running status, a sysex and a long meta event, each length
/// field equals the body that follows.
/// Spec: smf/chunks#1, smf/chunks#5, smf/chunks#6, smf/end-of-track#1
#[test]
fn each_track_chunk_length_is_the_length_of_its_body() {
    let busy = track(vec![
        (0, note(0, 60)),
        (0, note(0, 62)),
        (5, FileEvent::SysEx(common::complete(&[0x7E; 129]))),
        (
            5,
            FileEvent::Meta(Meta::text(TextKind::Lyric, &"l".repeat(300))),
        ),
        (0x0FFF_FFFF, note(0, 64)),
    ]);
    let plain = track(vec![(0, note(1, 60))]);
    let bytes = crate::common::format1(SMF_DIVISION, vec![busy, plain])
        .unwrap()
        .to_bytes();
    // The header chunk is 14 bytes; each track chunk is a type, a length, a body.
    let mut at = 14;
    let mut chunks = 0;
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 4], b"MTrk");
        let len = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        // The body ends in End of Track, and the next chunk follows it.
        assert_eq!(&bytes[at + 8 + len - 3..at + 8 + len], [0xFF, 0x2F, 0x00]);
        at += 8 + len;
        chunks += 1;
    }
    assert_eq!((chunks, at), (2, bytes.len()));
}

// ---- the specification's own examples (SMF 1.0 sections 1.1, 3.1, 3.3, 4) ---

/// The spec's format 0 example file: 96 ticks per quarter, 4/4 and 500000
/// microseconds per quarter, three Program Changes and four notes, with
/// running status and a two-byte delta.
const EXAMPLE_FORMAT_0: [u8; 14 + 8 + 59] = [
    0x4D, 0x54, 0x68, 0x64, 0, 0, 0, 6, 0, 0, 0, 1, 0, 0x60, //
    0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 0x3B, //
    0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08, //
    0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, //
    0x00, 0xC0, 0x05, //
    0x00, 0xC1, 0x2E, //
    0x00, 0xC2, 0x46, //
    0x00, 0x92, 0x30, 0x60, //
    0x00, 0x3C, 0x60, //
    0x60, 0x91, 0x43, 0x40, //
    0x60, 0x90, 0x4C, 0x20, //
    0x81, 0x40, 0x82, 0x30, 0x40, //
    0x00, 0x3C, 0x40, //
    0x00, 0x81, 0x43, 0x40, //
    0x00, 0x80, 0x4C, 0x40, //
    0x00, 0xFF, 0x2F, 0x00,
];

/// The spec's format 1 example: the same music as four tracks, a tempo and
/// time signature track and three of notes.
const EXAMPLE_FORMAT_1: [u8; 14 + 28 + 24 + 23 + 29] = [
    0x4D, 0x54, 0x68, 0x64, 0, 0, 0, 6, 0, 1, 0, 4, 0, 0x60, //
    0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 0x14, //
    0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08, //
    0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, //
    0x83, 0x00, 0xFF, 0x2F, 0x00, //
    0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 0x10, //
    0x00, 0xC0, 0x05, //
    0x81, 0x40, 0x90, 0x4C, 0x20, //
    0x81, 0x40, 0x4C, 0x00, //
    0x00, 0xFF, 0x2F, 0x00, //
    0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 0x0F, //
    0x00, 0xC1, 0x2E, //
    0x60, 0x91, 0x43, 0x40, //
    0x82, 0x20, 0x43, 0x00, //
    0x00, 0xFF, 0x2F, 0x00, //
    0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 0x15, //
    0x00, 0xC2, 0x46, //
    0x00, 0x92, 0x30, 0x60, //
    0x00, 0x3C, 0x60, //
    0x83, 0x00, 0x30, 0x00, //
    0x00, 0x3C, 0x00, //
    0x00, 0xFF, 0x2F, 0x00,
];

fn tempo_500000() -> FileEvent {
    FileEvent::Meta(Meta::Tempo {
        micros_per_quarter: MicrosPerQuarter::of(500_000),
        extra: Box::default(),
    })
}

fn four_four() -> FileEvent {
    FileEvent::Meta(Meta::TimeSignature {
        numerator: 4,
        denominator_power: 2,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    })
}

/// The spec's format 0 example reads as its header, time signature and tempo,
/// the delta-times its bytes give (`81 40` is 192 ticks), and writes back as
/// the same bytes: the running status the example uses is the writer's too.
/// Spec: smf/examples#1, smf/examples#2
#[test]
fn the_spec_format_0_example_reads_as_its_figures_and_writes_back_byte_for_byte() {
    let file = read_bytes(EXAMPLE_FORMAT_0).unwrap();
    assert_eq!(file.format(), Some(SmfFormat::SingleTrack));
    assert_eq!(file.division(), Some(SMF_DIVISION));
    assert_eq!(file.tracks().len(), 1);
    let events = file.tracks()[0].events();
    assert_eq!(events[0], (Tick(0), four_four()));
    assert_eq!(events[1], (Tick(0), tempo_500000()));
    let at: Vec<u64> = events.iter().map(|(t, _)| t.0).collect();
    assert_eq!(at, [0, 0, 0, 0, 0, 0, 0, 96, 192, 384, 384, 384, 384]);
    assert_eq!(file.to_bytes(), EXAMPLE_FORMAT_0);
}

/// The spec's format 1 example reads as four tracks, the first holding the
/// time signature and tempo and ending at tick 384 (`83 00`), and writes back
/// as the same bytes.
/// Spec: smf/examples#3, smf/examples#4
#[test]
fn the_spec_format_1_example_reads_as_four_tracks_and_writes_back_byte_for_byte() {
    let file = read_bytes(EXAMPLE_FORMAT_1).unwrap();
    assert_eq!(file.format(), Some(SmfFormat::Parallel));
    assert_eq!(file.tracks().len(), 4);
    let tempo_track = &file.tracks()[0];
    assert_eq!(tempo_track.events()[0], (Tick(0), four_four()));
    assert_eq!(tempo_track.events()[1], (Tick(0), tempo_500000()));
    assert_eq!(tempo_track.end(), Tick(384));
    assert_eq!(file.to_bytes(), EXAMPLE_FORMAT_1);
}

/// The spec's SysEx examples: the transmitted message `F0 43 12 00 07 F7` is
/// stored as `F0 05 43 12 00 07 F7`, and `F0 43 12 00`, 200 ticks (`81 48`),
/// `43 12 00 43 12 00`, 100 ticks (`64`), `43 12 00 F7` are an `F0` event and
/// two `F7` events.
/// Spec: smf/track-chunks#8, smf/track-chunks#10, smf/track-chunks#11, smf/track-chunks#13, smf/track-chunks#14, smf/track-chunks#15, smf/track-chunks#16, smf/track-chunks#17, smf/track-chunks#22, smf/track-chunks#23
#[test]
fn the_spec_sysex_examples_read_as_their_events_and_write_back() {
    let whole = raw_track(&[
        0x00, 0xF0, 0x05, 0x43, 0x12, 0x00, 0x07, 0xF7, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let read = read_bytes(&whole).unwrap();
    assert_eq!(
        unspecified(&read, 0),
        [(
            Tick(0),
            FileEvent::SysEx(common::complete(&[0x43, 0x12, 0x00, 0x07]))
        )]
    );
    assert_eq!(without_defaults(&read.to_bytes()), whole);

    let packets = raw_track(&[
        0x00, 0xF0, 0x03, 0x43, 0x12, 0x00, //
        0x81, 0x48, 0xF7, 0x06, 0x43, 0x12, 0x00, 0x43, 0x12, 0x00, //
        0x64, 0xF7, 0x04, 0x43, 0x12, 0x00, 0xF7, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let read = read_bytes(&packets).unwrap();
    assert_eq!(
        unspecified(&read, 0),
        [
            (
                Tick(0),
                FileEvent::SysEx(common::start(&[0x43, 0x12, 0x00]))
            ),
            (
                Tick(200),
                FileEvent::Escape(Box::new([0x43, 0x12, 0x00, 0x43, 0x12, 0x00]))
            ),
            (
                Tick(300),
                FileEvent::Escape(Box::new([0x43, 0x12, 0x00, 0xF7]))
            ),
        ]
    );
    assert_eq!(without_defaults(&read.to_bytes()), packets);
}

/// The spec's Time Signature example: 6/8 with a click every three eighth
/// notes and eight 32nd notes to the MIDI quarter is `FF 58 04 06 03 24 08`
/// (the denominator is a power of two: 3 is an eighth, the click 36 clocks).
/// Spec: smf/time-signature#1, smf/time-signature#2, smf/time-signature#3, smf/time-signature#4
#[test]
fn the_spec_time_signature_example_is_six_eight_with_36_clocks_a_click() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x58, 0x04, 0x06, 0x03, 0x24, 0x08, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let read = read_bytes(&bytes).unwrap();
    assert_eq!(
        unspecified(&read, 0)[0].1,
        FileEvent::Meta(Meta::TimeSignature {
            numerator: 6,
            denominator_power: 3,
            clocks_per_click: 0x24,
            thirty_seconds_per_quarter: 8,
            extra: Box::default(),
        })
    );
    assert_eq!(without_defaults(&read.to_bytes()), bytes);
}

/// The spec's Key Signature: `sf` is -7 flats to 7 sharps (negative in two's
/// complement) and `mi` is 0 for major, 1 for minor; `FF 59 02 sf mi` reads
/// as that key and writes back.
/// Spec: smf/key-signature#1
#[test]
fn a_key_signature_is_seven_flats_to_seven_sharps_and_a_mode() {
    for sharps in -7i8..=7 {
        for minor in [false, true] {
            let bytes = raw_track(&[
                0x00,
                0xFF,
                0x59,
                0x02,
                sharps.to_be_bytes()[0],
                u8::from(minor),
                0x00,
                0xFF,
                0x2F,
                0x00,
            ]);
            let read = read_bytes(&bytes).unwrap();
            assert_eq!(
                unspecified(&read, 0)[0].1,
                FileEvent::Meta(Meta::KeySignature {
                    sharps,
                    minor,
                    extra: Box::default(),
                }),
                "{sharps} {minor}"
            );
            assert_eq!(
                without_defaults(&read.to_bytes()),
                bytes,
                "{sharps} {minor}"
            );
        }
    }
}

/// The spec's delta-time example: at 96 ticks to the quarter and 500000
/// microseconds to the quarter, 6144 ticks are 32000 milliseconds.
/// Spec: smf/delta-times#2, smf/delta-times#3
#[test]
fn the_spec_delta_time_example_6144_ticks_at_96_and_500000_is_32_seconds() {
    let bytes = build_smf(
        0,
        Division::Metrical(TPQ),
        &[vec![
            RawEvent::tempo(0, 500_000),
            RawEvent::note_on(6144, 0, 60, 100),
        ]],
    );
    let file = read_bytes(&bytes).unwrap();
    assert_eq!(metrical(&file).seconds_at(Tick(6144)), Seconds::new(32.0));
}

/// Meta events at one tick may come in any order: a Set Tempo then a text and
/// a text then a Set Tempo both read, in the order the file has them.
/// Spec: smf/meta-events#8
#[test]
fn meta_events_at_one_tick_read_in_the_order_the_file_has_them() {
    let tempo = RawEvent::tempo(0, 500_000);
    let text = RawEvent::new(0, vec![0xFF, 0x01, 1, b'x']);
    let first = read_bytes(file(vec![tempo, text])).unwrap();
    let kinds = |f: &MidiFile| -> Vec<bool> {
        f.tracks()[0]
            .events()
            .iter()
            .map(|(_, e)| matches!(e, FileEvent::Meta(Meta::Tempo { .. })))
            .collect()
    };
    // The Time Signature the file is given joins them.
    assert_eq!(kinds(&first), [true, false, false]);
    let tempo = RawEvent::tempo(0, 500_000);
    let text = RawEvent::new(0, vec![0xFF, 0x01, 1, b'x']);
    let second = read_bytes(file(vec![text, tempo])).unwrap();
    assert_eq!(kinds(&second), [false, true, false]);
}

/// A text event may use character codes with the high bit set: the bytes read
/// as they are and write back unchanged.
/// Spec: smf/text-event#4
#[test]
fn text_bytes_with_the_high_bit_set_read_and_write_back_unchanged() {
    let bytes = file(vec![RawEvent::new(0, vec![0xFF, 0x01, 2, 0xE9, b'a'])]);
    let read = read_bytes(&bytes).unwrap();
    assert_eq!(
        unspecified(&read, 0)[0].1,
        FileEvent::Meta(Meta::Text {
            kind: TextKind::Text,
            bytes: Box::new([0xE9, b'a']),
        })
    );
    assert_eq!(without_defaults(&read.to_bytes()), bytes);
}
