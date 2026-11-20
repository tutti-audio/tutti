//! The Standard MIDI File half of [`MidiFile`], against bytes built from the
//! SMF 1.0 spec (`support`: an encoder and a decoder written from it, sharing
//! no code with the crate, so they are the second opinion on the bytes).
//!
//! **The lossless round trip is the test**: a file read and written holds the
//! same events, including SysEx and meta; running status and event spelling may
//! differ, so equality is on the model and on the independent decoder's
//! reading of the bytes, never on the bytes.

use crate::common::{self, metrical, sequences, track, unspecified};
use crate::common::{ToBytes, read_bytes};
use crate::support::{Division, RawEvent, build_smf};
use tutti_fixtures::{RangeContract, check_range_contract};

use tutti_midi_file::{
    FileEvent, Meta, MicrosPerQuarter, MidiFile, MidiFileKind, OtherMetaKind, PairedNote,
    ReadError, SmfDivision, SmfParseError, SmpteFractionalFrames, SmpteOffset, SysEx, TextKind,
    Tick, Timebase, TracksError,
};
use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode};
use tutti_ump::{
    Address, ControlValue, Message, MidiChannel, MidiGroup, NoteNumber, TicksPerQuarterNote, U7,
    U14, Voice1,
};
use tutti_units::{Beat, Bpm, Seconds};

const TPQ: u16 = 480;
const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(TPQ);
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);
const BEAT: u32 = TPQ as u32;

fn smf(format: u16, tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    build_smf(format, Division::Metrical(TPQ), tracks)
}

fn voice(channel: u8, voice: Voice1) -> FileEvent {
    FileEvent::Message(Message::Midi1(
        Address::first(MidiChannel::new(channel).unwrap()),
        voice,
    ))
}

fn on(channel: u8, key: u8, vel: u8) -> FileEvent {
    Message::midi1(
        MidiChannel::of(channel),
        Voice1::note_on(NoteNumber::of(key), U7::of(vel)),
    )
    .into()
}

fn off(channel: u8, key: u8, vel: u8) -> FileEvent {
    Message::midi1(
        MidiChannel::of(channel),
        Voice1::note_off(NoteNumber::of(key), U7::of(vel)),
    )
    .into()
}

fn read(bytes: &[u8]) -> MidiFile {
    read_bytes(bytes).expect("reads")
}

/// A file one of every event the SMF has, in a track.
fn everything() -> Vec<RawEvent> {
    let meta = |delta, kind: u8, data: &[u8]| {
        let mut v = vec![0xFF, kind, data.len() as u8];
        v.extend_from_slice(data);
        RawEvent::new(delta, v)
    };
    let mut escape = vec![0xF7, 3];
    escape.extend_from_slice(&[0x01, 0x02, 0x03]);
    vec![
        meta(0, 0x00, &[0, 5]),                 // sequence number
        meta(0, 0x01, b"text"),                 // text
        meta(0, 0x02, b"(c) nobody"),           // copyright
        RawEvent::track_name(0, "Lead"),        // track name
        meta(0, 0x04, b"flute"),                // instrument name
        meta(0, 0x05, b"la"),                   // lyric
        meta(0, 0x06, b"Verse"),                // marker
        meta(0, 0x07, b"door slams"),           // cue point
        meta(0, 0x08, b"patch"),                // program name
        meta(0, 0x09, b"port a"),               // device name
        meta(0, 0x20, &[3]),                    // channel prefix
        meta(0, 0x21, &[1]),                    // port
        RawEvent::tempo(0, 500_000),            // 120 BPM
        meta(0, 0x54, &[0x21, 30, 15, 10, 50]), // SMPTE offset (25 fps, 1:30:15:10.50)
        RawEvent::time_signature(0, 6, 3),      // 6/8
        meta(0, 0x59, &[0xFD, 1]),              // three flats, minor
        meta(0, 0x7F, &[0x7D, 1, 2]),           // sequencer specific
        meta(0, 0x60, &[9, 9]),                 // an unregistered type
        RawEvent::note_on(0, 0, 60, 100),
        // Running status: the same status, omitted.
        RawEvent::running_status(0, [62, 90]),
        RawEvent::new(10, [0xA0, 60, 33]),    // poly pressure
        RawEvent::new(0, [0xB1, 7, 99]),      // controller
        RawEvent::new(0, [0xC2, 40]),         // program change
        RawEvent::new(0, [0xD3, 50]),         // channel pressure
        RawEvent::new(0, [0xE4, 0x01, 0x40]), // pitch bend
        RawEvent::sysex(10, &[0x7E, 0x7F, 0x09, 0x01]),
        RawEvent::sysex(0, &[0x11; 300]), // a length needing two VLQ bytes
        RawEvent::new(0, escape),         // an F7 escape
        RawEvent::note_off(BEAT, 0, 60, 64),
        RawEvent::note_off(0, 0, 62, 0),
        meta(0, 0x06, b"end"),
    ]
}

/// **Every event kind of the SMF is read, and survives a write.**
///
/// What the first read must say (the figures are the bytes above), then
/// that writing it and reading that back is the same file, and that an
/// independent decoder reads the same events in the original and the rewrite
/// (running status made explicit by it).
/// Spec: smf/formats#4, smf/formats#5, smf/key-signature#1, smf/set-tempo#1, smf/text-event#2, smf/time-signature#1, smf/track-chunks#14, smf/track-chunks#23, smf/track-chunks#26
#[test]
fn every_event_kind_survives_a_read_and_a_write() {
    for format in [0u16, 1, 2] {
        let tracks = if format == 0 {
            vec![everything()]
        } else {
            vec![
                everything(),
                vec![
                    RawEvent::tempo(0, 500_000),
                    RawEvent::time_signature(0, 4, 2),
                    RawEvent::note_on(0, 1, 70, 80),
                ],
            ]
        };
        let bytes = smf(format, &tracks);
        let file = read(&bytes);
        assert_eq!(
            MidiFileKind::detect(bytes.clone()).unwrap(),
            Some(MidiFileKind::StandardMidiFile)
        );
        let ticks_per_quarter = match file.timebase() {
            Timebase::Metrical(view) => view.ticks_per_quarter(),
            Timebase::MetricalSequences(view) => view.ticks_per_quarter(),
            _ => panic!("a metrical file"),
        };
        assert_eq!(ticks_per_quarter, DIVISION);

        let events = &file.tracks()[0].events();
        let at = |i: usize| &events[i];
        // The four metas that open a track are its fields, and in this file
        // they are where SMF 1.0 puts them, so the read changed nothing.
        let first = &file.tracks()[0];
        assert_eq!(file.sequence_number(0), Some(5));
        assert_eq!(file.copyright(), Some(&b"(c) nobody"[..]));
        assert_eq!(first.name(), Some("Lead"));
        assert_eq!(
            file.smpte_offset(0),
            Some(
                SmpteOffset::new(
                    SmpteTimecode::new(1, 30, 15, 10, SmpteFrameRate::Fps25).unwrap(),
                    SmpteFractionalFrames::of(50),
                )
                .unwrap()
            )
        );
        assert_eq!(file.normalized(), []);
        assert_eq!(at(0).1, FileEvent::Meta(Meta::text(TextKind::Text, "text")));
        assert_eq!(at(2).1, FileEvent::Meta(Meta::text(TextKind::Lyric, "la")));
        assert_eq!(
            at(3).1,
            FileEvent::Meta(Meta::text(TextKind::Marker, "Verse"))
        );
        assert_eq!(
            at(9).1,
            FileEvent::Meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::default(),
            })
        );
        assert_eq!(
            at(10).1,
            FileEvent::Meta(Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 24,
                thirty_seconds_per_quarter: 8,
                extra: Box::default(),
            })
        );
        assert_eq!(
            at(11).1,
            FileEvent::Meta(Meta::KeySignature {
                sharps: -3,
                minor: true,
                extra: Box::default(),
            })
        );
        assert_eq!(at(14).1, on(0, 60, 100));
        // Running status read as the message it stands for.
        assert_eq!(at(15).1, on(0, 62, 90));
        assert_eq!(
            at(16),
            &(
                Tick(10),
                voice(
                    0,
                    Voice1::PolyPressure {
                        note: NoteNumber::of(60),
                        pressure: U7::of(33)
                    }
                )
            )
        );
        assert_eq!(
            at(20).1,
            voice(4, Voice1::PitchBend(U14::of(0x40 << 7 | 0x01)))
        );
        assert_eq!(
            at(21).1,
            FileEvent::SysEx(common::complete(&[0x7E, 0x7F, 0x09, 0x01]))
        );
        // The `F7` ends it and the writer adds it, so the model holds 300 bytes.
        let FileEvent::SysEx(SysEx::Complete(long)) = &at(22).1 else {
            panic!("a whole SysEx message");
        };
        assert_eq!(long.len(), 300);
        assert_eq!(at(23).1, FileEvent::Escape(vec![1, 2, 3].into()));
        assert_eq!(at(24).0, Tick(20 + u64::from(BEAT)));

        let written = file.to_bytes();
        // SMF 1.0 section 2.1: the header's format word, after the chunk type
        // "MThd" and the length 6.
        assert_eq!(&written[8..10], &format.to_be_bytes());
        assert_eq!(read(&written), file, "format {format}");
    }
}

/// The track's length is its End of Track, which may sit after the last event.
/// Spec: smf/end-of-track#2, smf/meta-events#11
#[test]
fn the_end_of_a_track_is_kept() {
    // `build_smf` appends an End of Track at delta 0; this one is a bar late.
    let mut bytes = smf(0, &[vec![RawEvent::note_on(0, 0, 60, 100)]]);
    let n = bytes.len();
    // The last event is `00 FF 2F 00`; its delta becomes 4 beats (0x8F 0x00 is
    // 1920 as a VLQ: 15*128 + 0).
    bytes[n - 4] = 0x8F;
    bytes.insert(n - 3, 0x00);
    // MTrk length is the four bytes at offset 18..22 (14 header + "MTrk").
    let len = u32::from_be_bytes(bytes[18..22].try_into().unwrap()) + 1;
    bytes[18..22].copy_from_slice(&len.to_be_bytes());

    let file = read(&bytes);
    assert_eq!(file.tracks()[0].end(), Tick(4 * u64::from(BEAT)));
    assert_eq!(
        metrical(&file).beat_at(metrical(&file).end()),
        Beat::new(4.0)
    );
    assert_eq!(read(&file.to_bytes()).tracks()[0].end(), Tick(1920));
}

/// A delta on a non-note event advances the clock: the note after a SysEx a
/// beat in is on beat 2.
/// Spec: smf/track-chunks#4
#[test]
fn a_sysex_between_notes_does_not_shift_the_beat_grid() {
    let file = read(&smf(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            RawEvent::note_off(BEAT / 2, 0, 60, 0),
            RawEvent::sysex(BEAT / 2, &[0x7D, 0x01]),
            RawEvent::note_on(BEAT, 0, 62, 100),
        ]],
    ));
    let ticks: Vec<u64> = unspecified(&file, 0).iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 240, 480, 960]);
    assert_eq!(metrical(&file).beat_at(Tick(960)), Beat::new(2.0));
}

/// **Format 2 keeps its sequences apart, each on its own tempo map.**
///
/// Sequence 0 is 120 BPM (a beat is 0.5 s) and sequence 1 is 60 BPM (a beat is
/// 1 s); a note a beat in is at 0.5 s in the first and 1 s in the second, and
/// two beats in is 1 s and 2 s. The file has no one-timeline view.
/// Spec: smf/formats#5, smf/header-chunk#8
#[test]
fn format_2_keeps_its_sequences_apart() {
    let file = read(&smf(
        2,
        &[
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::note_on(BEAT, 0, 60, 100),
            ],
            vec![
                RawEvent::tempo(0, 1_000_000),
                RawEvent::note_on(BEAT, 0, 62, 100),
            ],
        ],
    ));
    assert_eq!(&file.to_bytes()[8..10], &[0, 2]);
    assert!(
        file.timebase().metrical().is_none(),
        "no one timeline to ask"
    );
    assert_eq!(file.tracks().len(), 2);
    let sequences: Vec<_> = sequences(&file).sequences().collect();
    let tempos: Vec<Bpm> = sequences
        .iter()
        .map(|s| s.tempo_map().tempo_at(Beat::new(0.0)))
        .collect();
    assert_eq!(tempos, [Bpm::of(120.0), Bpm::of(60.0)]);
    let seconds = |tick: u64| -> Vec<Seconds> {
        sequences
            .iter()
            .map(|s| s.seconds_at(Tick(tick * u64::from(BEAT))))
            .collect()
    };
    assert_eq!(seconds(1), [Seconds::new(0.5), Seconds::new(1.0)]);
    assert_eq!(seconds(2), [Seconds::new(1.0), Seconds::new(2.0)]);
    assert_eq!(
        sequences[1].beat_at(Tick(2 * u64::from(BEAT))),
        Beat::new(2.0)
    );
    // Each sequence's events are its own track's, at its own beats.
    for sequence in &sequences {
        let beats: Vec<Beat> = sequence
            .events()
            .filter(|(_, e)| matches!(e, FileEvent::Message(_)))
            .map(|(beat, _)| beat)
            .collect();
        assert_eq!(beats, [Beat::new(1.0)]);
    }
    // The same two tracks as format 1 share one map and one timeline.
    let parallel = read(&smf(
        1,
        &[
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![RawEvent::note_on(0, 0, 62, 100)],
        ],
    ));
    // The three events and the Time Signature the file is given.
    assert_eq!(metrical(&parallel).events().count(), 4);
    assert!(parallel.timebase().metrical().is_some());
    assert_eq!(
        metrical(&parallel).tempo_map().tempo_at(Beat::new(0.0)),
        Bpm::of(120.0)
    );
}

/// **A sequence's tempo neither leaks into nor depends on another's.**
///
/// Sequence 0 sets no tempo (120 BPM, 0.5 s a beat); sequence 1 is 120 BPM
/// until beat 1 and 60 BPM after it (1 s a beat). Beat 3 is 1.5 s in sequence 0
/// and 0.5 + 2 = 2.5 s in sequence 1.
/// Spec: smf/header-chunk#8
#[test]
fn a_format_2_sequences_tempo_changes_stay_in_that_sequence() {
    let file = read(&smf(
        2,
        &[
            vec![RawEvent::note_on(0, 0, 60, 100)],
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::tempo(BEAT, 1_000_000),
                RawEvent::note_on(0, 0, 62, 100),
            ],
        ],
    ));
    let sequences: Vec<_> = sequences(&file).sequences().collect();
    let at = |beats: u64| Tick(beats * u64::from(BEAT));
    assert_eq!(
        sequences[0].tempo_map().tempo_at(Beat::new(5.0)),
        Bpm::of(120.0)
    );
    assert_eq!(sequences[0].tempo_map().points().len(), 1);
    assert_eq!(
        sequences[1].tempo_map().tempo_at(Beat::new(0.5)),
        Bpm::of(120.0)
    );
    assert_eq!(
        sequences[1].tempo_map().tempo_at(Beat::new(1.0)),
        Bpm::of(60.0)
    );
    assert_eq!(sequences[0].seconds_at(at(1)), Seconds::new(0.5));
    assert_eq!(sequences[0].seconds_at(at(3)), Seconds::new(1.5));
    assert_eq!(sequences[1].seconds_at(at(1)), Seconds::new(0.5));
    assert_eq!(sequences[1].seconds_at(at(3)), Seconds::new(2.5));
}

/// Format 1: the tracks stay apart and named, and the merged view is sorted by
/// tick with ties in track order.
/// Spec: smf/formats#4, smf/header-chunk#7
#[test]
fn merged_events_are_in_time_order_ties_in_track_order() {
    let file = read(&smf(
        1,
        &[
            vec![
                RawEvent::track_name(0, "Conductor"),
                RawEvent::note_on(0, 0, 60, 100),
                RawEvent::note_off(4 * BEAT, 0, 60, 0),
            ],
            vec![
                RawEvent::track_name(0, "Bass"),
                RawEvent::note_on(2 * BEAT, 1, 67, 100),
                RawEvent::note_off(BEAT, 1, 67, 0),
                // On the tick of track 0's Note Off: track 0 first.
                RawEvent::note_on(BEAT, 1, 69, 100),
            ],
        ],
    ));
    assert_eq!(file.tracks()[0].name(), Some("Conductor"));
    assert_eq!(file.tracks()[1].name(), Some("Bass"));
    let merged: Vec<(Beat, &FileEvent)> = metrical(&file).events().collect();
    let beats: Vec<f64> = merged.iter().map(|(b, _)| b.get()).collect();
    assert!(beats.windows(2).all(|w| w[0] <= w[1]), "{beats:?}");
    // The two events at beat 4: track 0's off, then track 1's on.
    let at4: Vec<&FileEvent> = merged
        .iter()
        .filter(|(b, _)| *b == Beat::new(4.0))
        .map(|(_, e)| *e)
        .collect();
    let (a, b) = (off(0, 60, 0), on(1, 69, 100));
    assert_eq!(at4, [&a, &b]);
}

/// **A tempo map built from the file: 120 BPM, then 60, beat to seconds exact.**
///
/// Four beats at 0.5 s then four at 1 s: beat 4 is 2 s, beat 8 is 6 s, and a
/// beat inside the second stretch is its own tempo's.
/// Spec: smf/set-tempo#1
#[test]
fn the_tempo_map_follows_the_file() {
    let file = read(&smf(
        1,
        &[vec![
            RawEvent::tempo(0, 500_000),
            RawEvent::tempo(4 * BEAT, 1_000_000),
            RawEvent::note_on(0, 0, 60, 100),
            RawEvent::note_off(4 * BEAT, 0, 60, 0), // at beat 8
        ]],
    ));
    let close = |a: Seconds, b: f64| assert!((a.get() - b).abs() < 1e-12, "{a:?} vs {b}");
    let beats = |beats: f64| Tick((beats * f64::from(BEAT)) as u64);
    close(metrical(&file).seconds_at(beats(0.0)), 0.0);
    close(metrical(&file).seconds_at(beats(1.0)), 0.5);
    close(metrical(&file).seconds_at(beats(4.0)), 2.0);
    close(metrical(&file).seconds_at(beats(5.5)), 3.5);
    close(metrical(&file).seconds_at(metrical(&file).end()), 6.0);
    assert_eq!(
        metrical(&file).tempo_map().tempo_at(Beat::new(3.99)),
        Bpm::of(120.0)
    );
    assert_eq!(
        metrical(&file).tempo_map().tempo_at(Beat::new(4.0)),
        Bpm::of(60.0)
    );
    assert_eq!(metrical(&file).beat_at(Tick(480)), Beat::new(1.0));
    // And it comes back out: the tempo events are events of the track.
    assert_eq!(
        metrical(&read(&file.to_bytes())).seconds_at(beats(8.0)),
        metrical(&file).seconds_at(beats(8.0))
    );
}

/// The map pools every track's Set Tempo: track 0 sets 60 at beat 4, track 1
/// sets 100 BPM at beat 0, so beat 0 plays at 100.
/// Spec: smf/header-chunk#7
#[test]
fn the_tempo_map_pools_every_track() {
    let file = read(&smf(
        1,
        &[
            vec![
                RawEvent::tempo(4 * BEAT, 1_000_000),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![
                RawEvent::tempo(0, 600_000),
                RawEvent::note_on(0, 1, 62, 100),
            ],
        ],
    ));
    let points = metrical(&file).tempo_map().points();
    assert_eq!(points.len(), 2);
    assert_eq!(
        (points[0].beat, points[0].tempo),
        (Beat::new(0.0), Bpm::of(100.0))
    );
    assert_eq!(
        (points[1].beat, points[1].tempo),
        (Beat::new(4.0), Bpm::of(60.0))
    );
}

/// No Set Tempo at tick 0: 120 BPM, the MIDI default, governs until the first;
/// a Set Tempo later does not govern the beats before it.
/// Spec: smf/delta-times#1, smf/formats#12
#[test]
fn a_file_with_no_tempo_plays_at_120() {
    let none = read(&smf(0, &[vec![RawEvent::note_on(0, 0, 60, 100)]]));
    assert_eq!(
        metrical(&none).tempo_map().constant_tempo(),
        Some(Bpm::of(120.0))
    );
    assert_eq!(
        metrical(&none).seconds_at(Tick(2 * u64::from(BEAT))),
        Seconds::new(1.0)
    );

    let later = read(&smf(0, &[vec![RawEvent::tempo(2 * BEAT, 1_000_000)]]));
    assert_eq!(
        metrical(&later).tempo_map().tempo_at(Beat::new(0.0)),
        Bpm::of(120.0)
    );
    assert_eq!(
        metrical(&later).tempo_map().tempo_at(Beat::new(2.0)),
        Bpm::of(60.0)
    );
}

/// Two tempos on one tick: the later in the file governs (applied in order).
#[test]
fn the_later_of_two_tempos_on_one_tick_governs() {
    let file = read(&smf(
        1,
        &[
            vec![RawEvent::tempo(0, 500_000)],
            vec![RawEvent::tempo(0, 1_000_000)],
        ],
    ));
    assert_eq!(metrical(&file).tempo_map().points().len(), 1);
    assert_eq!(
        metrical(&file).tempo_map().tempo_at(Beat::new(0.0)),
        Bpm::of(60.0)
    );
}

/// A zero microseconds-per-quarter is no valid tempo: kept as an event,
/// absent from the map. Any other value is a tempo, however absurd (1 us is 60
/// million BPM, 0xFFFFFF us about 3.58 BPM).
#[test]
fn a_zero_tempo_is_not_a_tempo_and_an_absurd_one_is_read_as_written() {
    let file = read(&smf(
        0,
        &[vec![
            RawEvent::tempo(0, 0),
            RawEvent::tempo(BEAT, 1),
            RawEvent::tempo(BEAT, 0xFF_FFFF),
        ]],
    ));
    assert_eq!(
        file.tracks()[0].events()[0].1,
        FileEvent::Meta(Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(0),
            extra: Box::default(),
        })
    );
    let points = metrical(&file).tempo_map().points();
    assert_eq!(points.len(), 3);
    assert_eq!(points[0].tempo, Bpm::of(120.0));
    assert_eq!(points[1].tempo, Bpm::of(60_000_000.0));
    assert!((points[2].tempo.get() - 60_000_000.0 / 16_777_215.0).abs() < 1e-12);
    // The zero is written back.
    assert_eq!(read(&file.to_bytes()), file);
}

/// Writing a tempo above the wire's range writes the fastest quarter, 1 us,
/// not the wire's no-valid-tempo zero.
#[test]
fn a_tempo_above_the_wire_range_writes_the_fastest_quarter() {
    assert_eq!(
        Meta::tempo(Bpm::of(1.0e9)),
        Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(1),
            extra: Box::default(),
        }
    );
    assert_eq!(
        Meta::tempo(Bpm::of(0.001)),
        Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(0xFF_FFFF),
            extra: Box::default(),
        }
    );
    // SMF 1.0 Set Tempo is microseconds per quarter: 120 BPM is a 0.5 s quarter.
    let at_120 = Meta::tempo(Bpm::of(120.0));
    assert_eq!(
        at_120,
        Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(500_000),
            extra: Box::default(),
        }
    );
    assert_eq!(at_120.bpm(), Some(Bpm::of(120.0)));
    // 174 BPM is not a whole microsecond: the wire quantises it.
    let Meta::Tempo {
        micros_per_quarter, ..
    } = Meta::tempo(Bpm::of(174.0))
    else {
        panic!("a tempo");
    };
    assert_eq!(micros_per_quarter, MicrosPerQuarter::of(344_828));
    assert!(
        !Meta::tempo(Bpm::of(174.0))
            .bpm()
            .unwrap()
            .differs_from(Bpm::of(174.0), Bpm::of(0.001))
    );
}

#[test]
fn a_zero_division_is_refused() {
    let bytes = build_smf(0, Division::Metrical(0), &[vec![]]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: None,
            offset: 0,
            cause: SmfParseError::ZeroTicksPerBeat
        })
    ));
}

// ---- truncation ----------------------------------------------------------

/// **No strict prefix of a valid file is accepted, and none panics.**
///
/// A file shorter than the whole cannot be the whole, so the answer needs no
/// oracle; a lenient reader would hand back a shorter file for most of them.
#[test]
fn no_prefix_of_a_valid_file_is_accepted() {
    let bytes = smf(
        1,
        &[
            everything(),
            vec![
                RawEvent::note_on(0, 1, 70, 80),
                RawEvent::note_off(BEAT, 1, 70, 0),
            ],
        ],
    );
    assert!(read_bytes(&bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(
            read_bytes(&bytes[..n]).is_err(),
            "the first {n} of {} bytes were accepted",
            bytes.len()
        );
    }
}

/// A chunk whose length runs past the end of the file is `Truncated`.
#[test]
fn a_chunk_cut_short_is_truncated() {
    let bytes = smf(0, &[everything()]);
    let cut = &bytes[..bytes.len() - 5];
    assert!(matches!(
        read_bytes(cut),
        Err(ReadError::Smf {
            track: None,
            offset: 14,
            cause: SmfParseError::ChunkCutOff
        })
    ));
    // Cut inside the 8-byte chunk header of the track.
    assert!(matches!(
        read_bytes(&bytes[..14 + 5]),
        Err(ReadError::Smf {
            track: None,
            offset: 14,
            cause: SmfParseError::ChunkCutOff
        })
    ));
}

/// Fewer track chunks than the header declares is `Truncated`: here the second
/// of two is missing whole, and the cut is on a chunk boundary.
/// Spec: smf/formats#3, smf/header-chunk#9
#[test]
fn a_file_missing_a_track_is_truncated() {
    let two = smf(
        1,
        &[
            vec![RawEvent::note_on(0, 0, 60, 100)],
            vec![RawEvent::note_on(0, 1, 62, 100)],
        ],
    );
    let one = smf(1, &[vec![RawEvent::note_on(0, 0, 60, 100)]]);
    let cut = &two[..one.len()];
    assert!(matches!(
        read_bytes(cut),
        Err(ReadError::Smf {
            track: None,
            offset,
            cause: SmfParseError::TracksCutOff
        }) if offset == one.len()
    ));
}

/// A track whose chunk length is right but which ends mid-event is malformed,
/// and the error names the track and the event's offset: a lenient reader
/// would keep the events before it and say nothing.
#[test]
fn a_track_that_ends_mid_event_is_a_parse_error() {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    // Replace the track body with `00 90 3C` (a Note On missing its velocity).
    bytes.truncate(14);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[0x00, 0x90, 0x3C]);
    // The header chunk is 14 bytes and the track chunk's own header 8.
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: Some(0),
            offset: 22,
            cause: SmfParseError::ChannelData
        })
    ));
}

/// A meta event whose length runs past the end of its chunk is cut off, and
/// the error names the track and where the event starts.
#[test]
fn a_meta_event_longer_than_its_chunk_is_an_event_cut_off() {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    bytes.truncate(14);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    // A Text meta event declaring 5 bytes of text and holding none.
    bytes.extend_from_slice(&[0x00, 0xFF, 0x01, 0x05]);
    let err = read_bytes(&bytes).unwrap_err();
    assert!(
        matches!(
            err,
            ReadError::Smf {
                track: Some(0),
                offset: 22,
                cause: SmfParseError::EventCutOff
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "SMF track 0 at byte 22: an event is cut off by the end of its chunk"
    );
}

/// A track chunk past the header's count is named at its own chunk.
#[test]
fn a_chunk_past_the_declared_tracks_is_named_where_it_starts() {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    let second = bytes.len();
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    assert!(matches!(
        read_bytes(&bytes),
        Err(ReadError::Smf {
            track: None,
            offset,
            cause: SmfParseError::ExtraTracks
        }) if offset == second
    ));
}

// ---- notes ---------------------------------------------------------------

fn notes_of(tracks: &[Vec<RawEvent>]) -> Vec<PairedNote> {
    read(&smf(0, tracks)).tracks()[0].notes()
}

#[test]
fn a_note_pairs_its_on_and_off_with_16_bit_velocity() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 2, 60, 100),
        RawEvent::note_off(BEAT + BEAT / 2, 2, 60, 0),
    ]]);
    assert_eq!(notes.len(), 1);
    let n = notes[0];
    assert_eq!(
        (n.group, n.channel),
        (MidiGroup::FIRST, MidiChannel::new(2).unwrap())
    );
    assert_eq!(n.note, NoteNumber::of(60));
    assert_eq!((n.start, n.end), (Tick(0), Tick(720)));
    // The 7-bit velocity upscaled by the specification's scaling (M2-104 D.3).
    assert!(
        n.velocity.get() > 100 << 9 && n.velocity.get() < 101 << 9,
        "{:?}",
        n.velocity
    );
}

#[test]
fn overlapping_same_key_notes_pair_last_on_first_off() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_on(BEAT, 0, 60, 90),
        RawEvent::note_off(BEAT, 0, 60, 0),
        RawEvent::note_off(BEAT, 0, 60, 0),
    ]]);
    // The first Note Off (tick 960) closes the second On (tick 480).
    let spans: Vec<(u64, u64)> = notes.iter().map(|n| (n.start.0, n.end.0)).collect();
    assert_eq!(spans, [(0, 1440), (480, 960)]);
}

#[test]
fn a_note_off_does_not_close_another_channels_note() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_off(BEAT, 1, 60, 0),
        RawEvent::note_off(BEAT, 0, 60, 0),
    ]]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(960));
}

#[test]
fn a_velocity_zero_note_on_closes_the_note() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_on(BEAT, 0, 60, 0),
        // Open at the end of the track: dropped.
        RawEvent::note_on(0, 0, 64, 100),
    ]]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(480));
}

/// The file layer does not translate: a velocity-0 Note On is read as the Note
/// On it is, and written back as one.
#[test]
fn a_velocity_zero_note_on_stays_a_note_on_in_the_file() {
    let file = read(&smf(0, &[vec![RawEvent::note_on(0, 0, 60, 0)]]));
    assert_eq!(unspecified(&file, 0)[0].1, on(0, 60, 0));
    assert_eq!(read(&file.to_bytes()), file);
}

// ---- building a file -----------------------------------------------------

/// The writer's framing is the spec's bytes (SMF 1.0 sections 2.1, 2.2, 3 and
/// 1.1): the header chunk, a length for each track chunk, a two-byte delta for
/// tick 1920 (`8F 00`) and the End of Track each track ends with.
/// Spec: smf/chunks#1, smf/chunks#4, smf/chunks#5, smf/chunks#7, smf/delta-times#2, smf/end-of-track#1, smf/header-chunk#1, smf/header-chunk#2, smf/header-chunk#3, smf/header-chunk#4, smf/meta-events#11, smf/set-tempo#1, smf/track-chunks#1
#[test]
fn the_writer_frames_a_file_as_the_spec_bytes() {
    let file = crate::common::format1(
        SMF_DIVISION,
        vec![
            track(vec![(0, FileEvent::Meta(Meta::tempo(Bpm::of(90.0))))]),
            track(vec![
                (0, on(0, 60, 100)),
                (4 * u64::from(BEAT), off(0, 60, 0)),
            ]),
        ],
    )
    .unwrap();
    assert_eq!(
        file.to_bytes(),
        [
            // MThd, length 6, format 1, two tracks, 480 ticks per quarter.
            0x4D, 0x54, 0x68, 0x64, 0, 0, 0, 6, 0, 1, 0, 2, 0x01, 0xE0, //
            // MTrk, length 19: Set Tempo 666 667 us (90 BPM), the default Time
            // Signature, End of Track.
            0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 19, //
            0x00, 0xFF, 0x51, 0x03, 0x0A, 0x2C, 0x2B, //
            0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08, //
            0x00, 0xFF, 0x2F, 0x00, //
            // MTrk, length 13: Note On, Note Off at delta 1920, End of Track.
            0x4D, 0x54, 0x72, 0x6B, 0, 0, 0, 13, //
            0x00, 0x90, 0x3C, 0x64, //
            0x8F, 0x00, 0x80, 0x3C, 0x00, //
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
}

/// Spec: smf/chunks#11, smf/formats#3
#[test]
fn an_empty_track_list_is_refused() {
    assert!(matches!(
        crate::common::format2(SMF_DIVISION, vec![]),
        Err(TracksError::NoTracks)
    ));
}

/// SMF 1.0 section 3.3: Set Tempo is `FF 51 03 tttttt`, 24 bits.
/// Spec: smf/set-tempo#1
#[test]
fn microseconds_per_quarter_is_0_to_16777215() {
    check_range_contract(&RangeContract {
        new: MicrosPerQuarter::new,
        get: MicrosPerQuarter::get,
        of: Some(MicrosPerQuarter::of),
        valid: &[0, 1, 500_000, 0xFF_FFFF],
        invalid: &[0x100_0000, u32::MAX],
    });
}

/// Every meta type the reader types (Sequence Number, the text events `01` to
/// `09`, Channel Prefix, End of Track, Set Tempo, SMPTE Offset, Time and Key
/// Signature, Sequencer-Specific) is refused as an `Other`, and so
/// is a type byte with its top bit set; every other type byte below 128 is
/// taken.
/// Spec: smf/track-chunks#25
#[test]
fn an_other_meta_kind_is_a_type_byte_with_no_variant() {
    let typed = [
        0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x20, 0x2F, 0x51, 0x54, 0x58, 0x59, 0x7F,
    ];
    for kind in 0..=u8::MAX {
        let expected = kind < 0x80 && !typed.contains(&kind);
        assert_eq!(OtherMetaKind::new(kind).is_some(), expected, "{kind:#04x}");
    }
}

/// A write then a read gives back an `Other` of every kind it can hold, data
/// of any length included.
/// Spec: smf/track-chunks#26
#[test]
fn an_other_meta_event_of_every_allowed_kind_writes_and_reads_back_equal() {
    let events: Vec<(Tick, FileEvent)> = (0..0x80u8)
        .filter_map(OtherMetaKind::new)
        .map(|kind| {
            let data = vec![0x7F; usize::from(kind.get() % 5)];
            (
                Tick(0),
                FileEvent::Meta(Meta::Other {
                    kind,
                    data: data.into(),
                }),
            )
        })
        .collect();
    let file = crate::common::format0(SMF_DIVISION, events.into_iter().collect()).unwrap();
    assert_eq!(read(&file.to_bytes()), file);
}

/// What an SMF cannot hold is refused rather than dropped on write: a packet,
/// another group, a tempo over 24 bits, a gap over 28 bits.
#[test]
fn what_an_smf_cannot_hold_is_refused() {
    let one = |event| crate::common::format0(SMF_DIVISION, track(vec![(0, event)]));
    let group_1 = FileEvent::Message(Message::Midi1(
        Address {
            group: MidiGroup::new(1).unwrap(),
            channel: MidiChannel::FIRST,
        },
        Voice1::ProgramChange(U7::of(1)),
    ));
    assert!(matches!(one(group_1), Err(TracksError::Refused { .. })));
    let midi2 = FileEvent::Message(Message::Midi2(
        Address::first(MidiChannel::FIRST),
        tutti_ump::Voice2::ChannelPressure(ControlValue::MIN),
    ));
    assert!(matches!(one(midi2), Err(TracksError::Refused { .. })));
    let gap = crate::common::format0(SMF_DIVISION, track(vec![(0x1000_0000, on(0, 60, 1))]));
    assert!(matches!(gap, Err(TracksError::Gap { .. })));
}

/// An event's length is a variable-length quantity of at most 28 bits (SMF 1.0
/// section 1.1): one byte past it is refused when the file is built, one byte
/// short of it is not.
/// Spec: smf/conventions#7
#[test]
fn an_event_longer_than_28_bits_is_too_large() {
    // The wire's length counts the `F7` the writer adds.
    let sysex = |len: usize| {
        let data = vec![0; len - 1];
        crate::common::format0(
            SMF_DIVISION,
            track(vec![(0, FileEvent::SysEx(common::complete(&data)))]),
        )
        .map(drop)
    };
    assert_eq!(sysex(1 << 28), Err(TracksError::TooLarge));
    assert_eq!(sysex((1 << 28) - 1), Ok(()));
}

/// The longest gap the wire holds, 28 bits, round-trips.
/// Spec: smf/conventions#7
#[test]
fn the_widest_delta_round_trips() {
    let file =
        crate::common::format0(SMF_DIVISION, track(vec![(0x0FFF_FFFF, on(0, 60, 1))])).unwrap();
    assert_eq!(read(&file.to_bytes()), file);
}

/// A file written to a path reads back from it, and a path that is not there
/// is an I/O error.
#[test]
fn a_file_reads_and_writes_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.mid");
    let file = crate::common::format0(SMF_DIVISION, track(vec![(0, on(0, 60, 1))])).unwrap();
    file.write(&path).unwrap();
    assert_eq!(MidiFile::read(&path).unwrap(), file);
    assert!(matches!(
        MidiFile::read(dir.path().join("absent.mid")),
        Err(ReadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound
    ));
}

/// Spec: smf/chunks#11
#[test]
fn bytes_of_neither_container_are_named_as_such() {
    assert!(matches!(
        read_bytes(b"RIFF....WAVE"),
        Err(ReadError::UnknownFormat)
    ));
    assert!(matches!(read_bytes(b""), Err(ReadError::UnknownFormat)));
}
