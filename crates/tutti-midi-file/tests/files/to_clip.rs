//! `MetricalView::to_clip`: a Standard MIDI File as a MIDI 2.0 Clip File, each
//! mapping checked against figures written out from the specifications (SMF
//! 1.0 section 3.3 for the meta events, M2-116 section 3.3, 7.1.1, 7.1.2 and
//! Appendix B for what replaces them, M2-104 section 7.5 for the Flex fields).
//!
//! The round trip (`the_clip_round_trips_through_its_bytes`) is a property of
//! the codec the conversion feeds; it is pinned by the clip codec's own
//! tests in `clip_model.rs`, and here it checks that the Flex and SysEx7
//! packets the conversion makes survive them.

use crate::common::{self, metrical, note, track};
use crate::common::{encoded_clip, read_bytes, read_clip};
use crate::support::{Division, RawEvent, build_smf};
use core::num::NonZeroU8;
use tutti_midi_file::{
    ClipFile, DropReason, Layout, Lead, LeadTrack, LyricChange, Meta, MicrosPerQuarter,
    OtherMetaKind, SmfDivision, SmfEvent, SmfPart, SmpteFractionalFrames, SmpteOffset,
    StandardMidiFile, TextKind, Tick, Track,
};
use tutti_midi_protocol::{SmpteFrameRate, SmpteTimecode};
use tutti_ump::{
    BarAccents, Flex, FlexAddress, FlexTempo, FlexTextError, FlexTextKind, FlexTimeSignature, Form,
    KeySignature, Letter, ManufacturerId, Message, Metronome, MidiChannel, MidiGroup, SharpsFlats,
    Stream, SysexStatus, Ump, Utility, split_stream,
};
use tutti_units::{Beat, Seconds};

const TPQ: u16 = 480;
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);

fn meta(m: Meta) -> SmfEvent {
    SmfEvent::Meta(m)
}

fn text(kind: TextKind, s: &str) -> SmfEvent {
    meta(Meta::text(kind, s))
}

fn flex(f: Flex) -> Ump {
    Message::Flex(MidiGroup::FIRST, f).into()
}

fn format0(events: Vec<(u64, SmfEvent)>) -> StandardMidiFile {
    crate::common::format0(SMF_DIVISION, track(events)).unwrap()
}

fn format0_track(track: Track) -> StandardMidiFile {
    crate::common::format0(SMF_DIVISION, track).unwrap()
}

fn format0_lead(lead: LeadTrack) -> StandardMidiFile {
    StandardMidiFile::new(SMF_DIVISION, Layout::Format0(lead)).unwrap()
}

/// The packet of an SMF event that is a message.
fn packet(event: SmfEvent) -> Ump {
    match event {
        SmfEvent::Channel { channel, message } => Ump::from(Message::midi1(channel, message)),
        _ => unreachable!("the event is a channel message"),
    }
}

/// The message of a clip packet, `None` for one no definition covers.
fn message(ump: &Ump) -> Option<Message> {
    ump.decode().ok()
}

fn events_of(file: &ClipFile) -> &[(Tick, Ump)] {
    file.sequence().events()
}

const DEFAULT_TEMPO: FlexTempo = FlexTempo::new(50_000_000);

fn signature_events(n: u8, d: u8, bb: u8, cc: u8) -> [Ump; 2] {
    [
        flex(Flex::SetTimeSignature(FlexTimeSignature {
            numerator: n,
            denominator: d,
            thirty_seconds: bb,
        })),
        // M2-104 section 7.5.5: an undivided bar's Bar Accent Part 1 is
        // the number of beats in the bar, the other two zero; a
        // Subdivision Clicks value of 0 means none.
        flex(Flex::SetMetronome(Metronome {
            clocks_per_click: cc,
            accents: BarAccents::Undivided(NonZeroU8::new(n).unwrap()),
            subdivision_1: 0,
            subdivision_2: 0,
        })),
    ]
}

/// SMF 1.0: 500000 us per quarter is 120 bpm; M2-104 section 7.5.3: the field
/// is 10 ns units per quarter, so 0.5 s is 50,000,000.
#[test]
fn a_set_tempo_is_ten_ns_units_per_quarter() {
    let smf = format0(vec![
        (
            0,
            meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::default(),
            }),
        ),
        (0, note(0, 60)),
    ]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    assert_eq!(
        events_of(&clip)[0],
        (Tick(0), flex(Flex::SetTempo(FlexTempo::new(50_000_000))))
    );
    // The widest the wire holds still fits the field: 0xFFFFFF us is 1_677_721_500.
    let slow = format0(vec![(
        0,
        meta(Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(0xFF_FFFF),
            extra: Box::default(),
        }),
    )]);
    let clip = metrical(&slow).to_clip().unwrap().clip;
    assert_eq!(
        events_of(&clip)[0].1,
        flex(Flex::SetTempo(FlexTempo::new(1_677_721_500)))
    );
}

/// SMF 1.0: `58 04 06 03 18 08` is 6/8, 24 MIDI clocks a click, 8 32nds per
/// quarter. M2-104 sections 7.5.4 and 7.5.5: the signature carries numerator,
/// denominator power and the 32nds; the metronome the clocks per click, and
/// 0 for the accents and subdivisions SMF has no field for. The one bar
/// accent part is the bar's six beats, so the parts add up to the beats.
/// Spec: m2-104/7.5.5#5, m2-104/7.5.5#9
#[test]
fn a_time_signature_is_a_signature_and_a_metronome() {
    let smf = format0(vec![
        (
            0,
            meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::default(),
            }),
        ),
        (
            0,
            meta(Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 36,
                thirty_seconds_per_quarter: 8,
                extra: Box::default(),
            }),
        ),
    ]);
    let events = metrical(&smf).to_clip().unwrap().clip;
    let [_, a, b] = events_of(&events) else {
        panic!("{:?}", events_of(&events));
    };
    let [sig, metronome] = signature_events(6, 3, 8, 36);
    assert_eq!((&a.1, &b.1), (&sig, &metronome));
    // The Metronome's accent parts add up to the Time Signature's beats.
    let (
        Some(Message::Flex(_, Flex::SetTimeSignature(time))),
        Some(Message::Flex(_, Flex::SetMetronome(click))),
    ) = (message(&a.1), message(&b.1))
    else {
        panic!("{a:?} {b:?}")
    };
    assert_eq!(click.accents.beats(), u16::from(time.numerator));
    assert_eq!(time.numerator, 6);
}

/// M2-104 section 7.5.7: sharps or flats, and the tonic's letter. 3 sharps
/// minor is F sharp minor (the relative minor of A major), so the tonic is F;
/// 0 major is C, 2 flats major is B flat, 3 flats minor is C minor, 7 sharps
/// major is C sharp.
#[test]
fn a_key_signature_is_the_sharps_and_the_tonic() {
    for (sharps, minor, tonic) in [
        (3, true, Letter::F),
        (0, false, Letter::C),
        (-2, false, Letter::B),
        (-3, true, Letter::C),
        (7, false, Letter::C),
        (0, true, Letter::A),
    ] {
        let smf = format0(vec![(
            0,
            meta(Meta::KeySignature {
                sharps,
                minor,
                extra: Box::default(),
            }),
        )]);
        let clip = metrical(&smf).to_clip().unwrap().clip;
        let key = events_of(&clip)
            .iter()
            .find_map(|(_, e)| match message(e) {
                Some(Message::Flex(_, Flex::SetKeySignature(a, k))) => Some((a, k)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            key,
            (
                FlexAddress::Group,
                KeySignature {
                    sharps_flats: SharpsFlats::new(sharps),
                    tonic: Some(tonic),
                }
            ),
            "{sharps} {minor}"
        );
    }
}

/// The Flex text of each packet, with its place in the message.
fn flex_texts(file: &ClipFile) -> Vec<(FlexTextKind, Form, Vec<u8>)> {
    events_of(file)
        .iter()
        .filter_map(|(_, e)| match message(e) {
            Some(Message::Flex(
                _,
                Flex::Text {
                    kind, form, bytes, ..
                },
            )) => Some((kind, form, bytes.to_vec())),
            _ => None,
        })
        .collect()
}

/// M2-116 Appendix B: Copyright Notice and Lyrics. M2-104 section 7.5.9: 12
/// bytes a packet, Start, Continue, End, zero padding.
#[test]
fn copyright_and_lyrics_are_flex_text_over_several_packets() {
    let notice = "(c) 2026 Someone Else"; // 21 bytes: 12 + 9
    let smf = format0_lead(
        LeadTrack::new(track(vec![(10, text(TextKind::Lyric, "la"))])).copyright(notice),
    );
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let got = flex_texts(&clip);
    let mut padded = b"Someone Else".to_vec();
    padded.truncate(12);
    assert_eq!(
        got,
        [
            (
                FlexTextKind::Copyright,
                Form::Start,
                b"(c) 2026 Som".to_vec()
            ),
            (
                FlexTextKind::Copyright,
                Form::End,
                b"eone Else\0\0\0".to_vec()
            ),
            (
                FlexTextKind::Lyrics,
                Form::Complete,
                b"la\0\0\0\0\0\0\0\0\0\0".to_vec()
            ),
        ]
    );
    let ticks: Vec<u64> = events_of(&clip)
        .iter()
        .filter(|(_, e)| matches!(message(e), Some(Message::Flex(_, Flex::Text { .. }))))
        .map(|(t, _)| t.0)
        .collect();
    assert_eq!(ticks, [0, 0, 10]);
}

/// M2-116 section 7.1.1: the first Set Tempo follows Start of Clip and uses the
/// Delta Clockstamp that precedes it. A clip with no configuration header has
/// a stamp of 0 before Start of Clip, and the Set Tempo comes right after
/// Start of Clip with none of its own.
/// Spec: m2-116/7.1.1#2
#[test]
fn the_first_set_tempo_has_the_delta_clockstamp_of_start_of_clip() {
    let smf = format0(vec![
        (
            0,
            meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(500_000),
                extra: Box::default(),
            }),
        ),
        (480, note(0, 60)),
    ]);
    let bytes = encoded_clip(&metrical(&smf).to_clip().unwrap().clip);
    let words: Vec<u32> = bytes[8..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u32::from_be_bytes(*w))
        .collect();
    let packets: Vec<Message> = split_stream(&words)
        .map(|u| u.decode().expect("a defined message"))
        .collect();
    let stamp = |p: &Message| match p {
        Message::Utility(Utility::DeltaClockstamp(t)) => Some(t.get()),
        _ => None,
    };
    let start = packets
        .iter()
        .position(|p| matches!(p, Message::Stream(Stream::StartOfClip)))
        .expect("Start of Clip");
    assert_eq!(stamp(&packets[start - 1]), Some(0));
    assert!(matches!(
        packets[start + 1],
        Message::Flex(_, Flex::SetTempo(DEFAULT_TEMPO))
    ));
}

/// M2-104 section 7.7: seven bytes are a Start of six and an End of one.
#[test]
fn a_complete_sysex_is_sysex7_packets() {
    let smf = format0(vec![(
        5,
        SmfEvent::SysEx(common::complete(&[0x7E, 1, 2, 3, 4, 5, 6])),
    )]);
    let converted = metrical(&smf).to_clip().unwrap();
    assert!(converted.dropped.is_empty());
    let packets: Vec<_> = events_of(&converted.clip)
        .iter()
        .filter_map(|(t, e)| match message(e) {
            Some(Message::Sysex7(_, p)) => Some((t.0, p.status(), p.data().to_vec())),
            _ => None,
        })
        .collect();
    assert_eq!(
        packets,
        [
            (5, SysexStatus::Start, vec![0x7E, 1, 2, 3, 4, 5]),
            (5, SysexStatus::End, vec![6]),
        ]
    );
}

#[test]
fn a_sysex_that_is_not_one_complete_message_is_reported() {
    let divided = SmfEvent::SysEx(common::start(&[0x7E, 1, 2]));
    let rest = SmfEvent::Escape(vec![3, 0xF7].into());
    // An `F0` event with an 8-bit byte is read as the escape of the bytes it sends.
    let eight_bit = SmfEvent::Escape(vec![0xF0, 0x7E, 0x80, 0xF7].into());
    let smf = read_bytes(build_smf(
        0,
        Division::Metrical(TPQ),
        &[vec![
            RawEvent::new(0, [0xF0, 3, 0x7E, 1, 2]),
            RawEvent::new(1, [0xF7, 2, 3, 0xF7]),
            RawEvent::new(1, [0xF0, 3, 0x7E, 0x80, 0xF7]),
        ]],
    ))
    .unwrap();
    let converted = metrical(&smf).to_clip().unwrap();
    let report: Vec<_> = converted
        .dropped
        .iter()
        .map(|d| (d.tick.0, d.part.clone(), d.reason))
        .collect();
    assert_eq!(
        report,
        [
            (0, SmfPart::Event(divided), DropReason::SysexNotComplete),
            (1, SmfPart::Event(rest), DropReason::NoSuccessor),
            (2, SmfPart::Event(eight_bit), DropReason::NoSuccessor),
        ]
    );
}

/// The merge rule of `events`, on the clip: by tick, ties in track order then
/// the track's own order.
#[test]
fn format_1_merges_by_tick_ties_in_track_order() {
    let a = (note(0, 60), note(0, 61)); // track 0, both at 100
    let b = note(1, 62); // track 1 at 100
    let c = note(1, 63); // track 1 at 50
    let smf = crate::common::format1(
        SMF_DIVISION,
        vec![
            track(vec![(100, a.0.clone()), (100, a.1.clone())]),
            track(vec![(50, c.clone()), (100, b.clone())]),
        ],
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let notes: Vec<(u64, Ump)> = events_of(&clip)
        .iter()
        .filter(|(_, e)| matches!(message(e), Some(Message::Midi1(..))))
        .map(|(t, e)| (t.0, *e))
        .collect();
    assert_eq!(
        notes,
        [
            (50, packet(c)),
            (100, packet(a.0)),
            (100, packet(a.1)),
            (100, packet(b))
        ]
    );
    // The end is the longest track's.
    assert_eq!(clip.end(), Tick(100));
}

/// M2-116 sections 7.1.1 and 7.1.2: the first Set Tempo, then the first Set
/// Time Signature, lead the sequence at its start, whatever order the SMF had.
/// Spec: m2-116/7.1.1#1, m2-116/7.1.2#1
#[test]
fn the_first_tempo_and_signature_lead_the_clip() {
    let smf = crate::common::format1(
        SMF_DIVISION,
        vec![
            track(vec![
                (
                    0,
                    meta(Meta::TimeSignature {
                        numerator: 3,
                        denominator_power: 2,
                        clocks_per_click: 24,
                        thirty_seconds_per_quarter: 8,
                        extra: Box::default(),
                    }),
                ),
                (
                    0,
                    meta(Meta::Tempo {
                        micros_per_quarter: MicrosPerQuarter::of(1_000_000),
                        extra: Box::default(),
                    }),
                ),
            ]),
            track(vec![(0, note(0, 60))]),
        ],
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let [sig, metronome] = signature_events(3, 2, 8, 24);
    let want = [
        flex(Flex::SetTempo(FlexTempo::new(100_000_000))),
        sig,
        metronome,
        packet(note(0, 60)),
    ];
    let got: Vec<_> = events_of(&clip).iter().map(|(_, e)| *e).collect();
    assert_eq!(got, want);
}

/// SMF 1.0 section 3.3: with no Set Tempo 120 bpm is assumed, and with no Time
/// Signature 4/4 (24 clocks, 8 32nds). The clip says it (section 7.1.1).
/// Spec: m2-116/7.1.1#1, m2-116/7.1.2#1, smf/formats#12
#[test]
fn a_file_with_no_tempo_or_signature_gets_the_smf_defaults() {
    let smf = format0(vec![
        (0, note(0, 60)),
        (
            960,
            meta(Meta::Tempo {
                micros_per_quarter: MicrosPerQuarter::of(250_000),
                extra: Box::default(),
            }),
        ),
    ]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let [sig, metronome] = signature_events(4, 2, 8, 24);
    let got: Vec<_> = events_of(&clip).iter().map(|(t, e)| (t.0, *e)).collect();
    assert_eq!(
        got,
        [
            (0, flex(Flex::SetTempo(DEFAULT_TEMPO))),
            (0, sig),
            (0, metronome),
            (0, packet(note(0, 60))),
            // The later tempo is on a 1/24-quarter position (960 is a multiple of 20), so it stays.
            (960, flex(Flex::SetTempo(FlexTempo::new(25_000_000)))),
        ]
    );
    // Two beats at the default 120 bpm (1 s), then two at 240 bpm (0.5 s).
    assert_eq!(clip.seconds_at(Tick(4 * u64::from(TPQ))), Seconds::new(1.5));
}

#[test]
fn a_tempo_at_tick_zero_is_not_defaulted() {
    let smf = format0(vec![(
        0,
        meta(Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(1_000_000),
            extra: Box::default(),
        }),
    )]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let tempos: Vec<_> = events_of(&clip)
        .iter()
        .filter_map(|(_, e)| match message(e) {
            Some(Message::Flex(_, Flex::SetTempo(t))) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tempos, [FlexTempo::new(100_000_000)]);
}

#[test]
fn the_division_and_the_end_carry_over() {
    let smf = crate::common::format1(
        SmfDivision::per_quarter(96),
        vec![
            Track::new()
                .push(Tick(10), note(0, 60))
                .unwrap()
                .ending_at(Tick(400)),
            Track::new()
                .push(Tick(10), note(1, 60))
                .unwrap()
                .ending_at(Tick(250)),
        ],
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    assert_eq!(clip.ticks_per_quarter().get(), 96);
    assert_eq!(clip.end(), Tick(400));
    assert!(clip.header().messages().is_empty());
}

/// Each event with no UMP form, with its tick and reason, in clip order.
#[test]
fn what_has_no_successor_is_reported_with_its_tick() {
    let other = |kind: u8| {
        meta(Meta::Other {
            kind: OtherMetaKind::of(kind),
            data: vec![1, 2].into(),
        })
    };
    let escape = SmfEvent::Escape(vec![0xF3, 1].into());
    let offset = SmpteOffset::new(
        SmpteTimecode::origin(SmpteFrameRate::Fps25),
        SmpteFractionalFrames::of(0),
    )
    .unwrap();
    let whole_note = meta(Meta::TimeSignature {
        numerator: 4,
        denominator_power: 0,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    });
    let events = vec![
        (0, meta(Meta::channel_prefix(MidiChannel::of(3)))),
        (5, other(0x21)), // port prefix
        (7, text(TextKind::Marker, "A")),
        (7, text(TextKind::CuePoint, "door")),
        (8, text(TextKind::InstrumentName, "piano")),
        (8, text(TextKind::ProgramName, "p")),
        (9, text(TextKind::DeviceName, "d")),
        (10, text(TextKind::Marker, "late")),
        (
            11,
            meta(Meta::SequencerSpecific {
                manufacturer: ManufacturerId::of(0x7D),
                data: vec![1, 2].into(),
            }),
        ),
        (12, escape.clone()),
        (13, whole_note.clone()),
        (
            13,
            meta(Meta::TimeSignature {
                numerator: 0,
                denominator_power: 2,
                clocks_per_click: 24,
                thirty_seconds_per_quarter: 8,
                extra: Box::default(),
            }),
        ),
        (
            14,
            meta(Meta::KeySignature {
                sharps: 8,
                minor: false,
                extra: Box::default(),
            }),
        ),
        (15, other(0x30)), // unregistered
    ];
    // The Sequence Number and the SMPTE Offset (Appendix B names a successor
    // M2-104 lacks) are the track's fields, listed first: they are at tick 0.
    let smf = format0_lead(
        LeadTrack::new(track(events.clone()))
            .numbered(1)
            .smpte_start(offset),
    );
    let converted = metrical(&smf).to_clip().unwrap();
    let reasons: Vec<(u64, DropReason)> = converted
        .dropped
        .iter()
        .map(|d| (d.tick.0, d.reason))
        .collect();
    use DropReason::*;
    assert_eq!(
        reasons,
        [
            (0, NoSuccessor),
            (0, SuccessorUndefined),
            (0, ReplacedByAddressing),
            (5, ReplacedByAddressing),
            (7, NoSuccessor),
            (7, NoSuccessor),
            (8, NoSuccessor),
            (8, NoSuccessor),
            (9, NoSuccessor),
            (10, NoSuccessor),
            (11, NoSuccessor),
            (12, NoSuccessor),
            (13, WholeNoteDenominator),
            (13, ZeroNumerator),
            (14, KeySignatureOutOfRange),
            (15, NoSuccessor),
        ]
    );
    // The report holds the event itself.
    assert_eq!(
        converted.dropped[0].part,
        SmfPart::Lead(Lead::SequenceNumber(1))
    );
    assert_eq!(
        converted.dropped[1].part,
        SmfPart::Lead(Lead::SmpteOffset(offset))
    );
    assert_eq!(converted.dropped[11].part, SmfPart::Event(escape));
    assert_eq!(converted.dropped[12].part, SmfPart::Event(whole_note));
    // The dropped signature was at tick 13, so the clip's only one is the
    // default 4/4 at tick 0 (the file declared none at its start).
    let signatures: Vec<_> = events_of(&converted.clip)
        .iter()
        .filter(|(_, e)| {
            matches!(
                message(e),
                Some(Message::Flex(_, Flex::SetTimeSignature(_)))
            )
        })
        .map(|(t, _)| t.0)
        .collect();
    assert_eq!(signatures, [0]);
}

/// SMF 1.0 section 3.3: in a format 0 track or the first track of format 1 a
/// Track Name is the sequence's name (M2-116 Appendix B: Composition Name);
/// any other is a track's, which a clip has none of.
#[test]
fn the_first_track_names_the_sequence_and_the_others_are_reported() {
    let smf = crate::common::format1(
        SMF_DIVISION,
        vec![Track::new().named("Sonata"), Track::new().named("Violin")],
    )
    .unwrap();
    let converted = metrical(&smf).to_clip().unwrap();
    assert_eq!(
        flex_texts(&converted.clip),
        [(
            FlexTextKind::CompositionName,
            Form::Complete,
            b"Sonata\0\0\0\0\0\0".to_vec()
        )]
    );
    assert_eq!(converted.dropped.len(), 1);
    assert_eq!(converted.dropped[0].track, 1);
    assert_eq!(converted.dropped[0].reason, DropReason::TrackName);
    // Format 0: its one track's name is the sequence's.
    let zero = format0_track(Track::new().named("Solo"));
    assert_eq!(
        flex_texts(&metrical(&zero).to_clip().unwrap().clip)[0].0,
        FlexTextKind::CompositionName
    );
}

/// `Converted::dropped` is by tick, then track, then the track's own order: a
/// track's dropped field (at tick 0) comes after the events of the tracks
/// before it at tick 0 and before its own.
#[test]
fn a_dropped_field_sorts_among_the_dropped_events_by_tick_then_track() {
    let marker = || text(TextKind::Marker, "m");
    let smf = crate::common::format1(
        SMF_DIVISION,
        vec![
            track(vec![(0, marker())]),
            track(vec![(0, marker())]).named("Violin"),
        ],
    )
    .unwrap();
    let converted = metrical(&smf).to_clip().unwrap();
    let order: Vec<_> = converted
        .dropped
        .iter()
        .map(|d| (d.track, d.reason))
        .collect();
    assert_eq!(
        order,
        [
            (0, DropReason::NoSuccessor),
            (1, DropReason::TrackName),
            (1, DropReason::NoSuccessor),
        ]
    );
}

/// Flex text is UTF-8 and zero-padded (M2-104 section 7.5.9); a generic text
/// event is Unknown Metadata Text.
#[test]
fn text_that_flex_cannot_carry_is_reported() {
    let latin1 = SmfEvent::Meta(Meta::Text {
        kind: TextKind::Lyric,
        bytes: vec![b'c', b'a', b'f', 0xE9].into(),
    });
    let smf = format0_lead(
        LeadTrack::new(track(vec![
            (0, latin1),
            (0, text(TextKind::Text, "remark")),
        ]))
        .copyright("a\0b"),
    );
    let converted = metrical(&smf).to_clip().unwrap();
    let reasons: Vec<_> = converted.dropped.iter().map(|d| d.reason).collect();
    assert_eq!(
        reasons,
        [
            DropReason::Text(FlexTextError::Nul.into()),
            DropReason::TextNotUtf8,
        ]
    );
    assert_eq!(
        flex_texts(&converted.clip),
        [(
            FlexTextKind::UnknownMetadata,
            Form::Complete,
            b"remark\0\0\0\0\0\0".to_vec()
        )]
    );
}

/// M2-104 section 1.8: message text is NFC-normalized UTF-8 and has no Byte
/// Order Mark. A decomposed `e` + combining acute
/// (`65 CC 81`) is written composed (`C3 A9`), and a leading BOM goes.
#[test]
fn text_is_composed_to_nfc_without_a_byte_order_mark() {
    let smf = format0_lead(
        LeadTrack::new(track(vec![(0, text(TextKind::Lyric, "cafe\u{301}"))]))
            .copyright("\u{FEFF}me"),
    );
    let converted = metrical(&smf).to_clip().unwrap();
    assert!(converted.dropped.is_empty());
    assert_eq!(
        flex_texts(&converted.clip),
        [
            (
                FlexTextKind::Copyright,
                Form::Complete,
                b"me\0\0\0\0\0\0\0\0\0\0".to_vec()
            ),
            (
                FlexTextKind::Lyrics,
                Form::Complete,
                b"caf\xC3\xA9\0\0\0\0\0\0\0".to_vec()
            ),
        ]
    );
}

/// M2-116 sections 6.4 and 7.2: no Property Exchange message in a clip
/// (MIDI-CI sub-ID#2 `0x30` to `0x3F`, M2-101 Table 4); another MIDI-CI
/// message (Discovery, `0x70`) is not PE and is carried. M2-104 section
/// 7.5.10.1: an empty Lyric would be a melisma, which SMF never said.
/// Spec: m2-116/6.4#1, m2-116/7.2#1
#[test]
fn property_exchange_and_empty_text_are_reported() {
    // F0 7E 7F 0D <sub-ID#2> ... F7, the bytes after the F0.
    let ci = |sub_id2: u8| SmfEvent::SysEx(common::complete(&[0x7E, 0x7F, 0x0D, sub_id2, 0x02]));
    let smf = format0(vec![
        (0, ci(0x34)), // Inquiry: Get Property Data
        (1, ci(0x3F)), // Notify
        (2, ci(0x70)), // Discovery: not PE
        (3, text(TextKind::Lyric, "")),
    ]);
    let converted = metrical(&smf).to_clip().unwrap();
    let reasons: Vec<(u64, DropReason)> = converted
        .dropped
        .iter()
        .map(|d| (d.tick.0, d.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            (0, DropReason::PropertyExchange),
            (1, DropReason::PropertyExchange),
            (3, DropReason::EmptyText),
        ]
    );
    let sysex_ticks: Vec<u64> = events_of(&converted.clip)
        .iter()
        .filter(|(_, e)| matches!(message(e), Some(Message::Sysex7(..))))
        .map(|(t, _)| t.0)
        .collect();
    assert_eq!(sysex_ticks, [2], "Discovery is carried");
}

/// The clip written and read back is the same clip.
#[test]
fn the_clip_round_trips_through_its_bytes() {
    let smf = StandardMidiFile::new(
        SMF_DIVISION,
        Layout::Format1 {
            lead: LeadTrack::new(track(vec![
                (
                    0,
                    meta(Meta::Tempo {
                        micros_per_quarter: MicrosPerQuarter::of(500_000),
                        extra: Box::default(),
                    }),
                ),
                (0, note(0, 60)),
                (480, note(0, 62)),
                (
                    960,
                    SmfEvent::SysEx(common::complete(&[0x7E, 1, 2, 3, 4, 5, 6, 7])),
                ),
            ]))
            .copyright("a rather long copyright notice"),
            tracks: vec![track(vec![
                (
                    0,
                    meta(Meta::KeySignature {
                        sharps: -3,
                        minor: true,
                        extra: Box::default(),
                    }),
                ),
                (480, note(1, 64)),
            ])],
        },
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let again = read_clip(encoded_clip(&clip)).unwrap();
    assert_eq!(again, clip);
}

fn tempo_event(micros: u32) -> SmfEvent {
    meta(Meta::Tempo {
        micros_per_quarter: MicrosPerQuarter::of(micros),
        extra: Box::default(),
    })
}

fn signature_meta(numerator: u8, power: u8) -> SmfEvent {
    meta(Meta::TimeSignature {
        numerator,
        denominator_power: power,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    })
}

fn at_ticks(clip: &ClipFile, wanted: fn(&Ump) -> bool) -> Vec<u64> {
    events_of(clip)
        .iter()
        .filter(|(_, e)| wanted(e))
        .map(|(t, _)| t.0)
        .collect()
}

fn is_tempo(e: &Ump) -> bool {
    matches!(message(e), Some(Message::Flex(_, Flex::SetTempo(_))))
}

fn is_signature(e: &Ump) -> bool {
    matches!(
        message(e),
        Some(Message::Flex(_, Flex::SetTimeSignature(_)))
    )
}

/// M2-104 section 7.5.3: in a file a Set Tempo sits on a 1/24-quarter-note
/// position. At 480 ticks per quarter that is every 20 ticks; 29 is nearer 20
/// than 40, 30 is the tie (taken later), and 40 is on the grid already.
/// Spec: m2-104/7.5.3#4, m2-116/7.1.1#4, m2-116/6.1.1#5
#[test]
fn a_tempo_off_the_clock_grid_moves_to_the_nearest_position() {
    let smf = format0(vec![
        (29, tempo_event(400_000)),
        (30, tempo_event(300_000)),
        (60, tempo_event(200_000)),
        (70, note(0, 60)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert_eq!(at_ticks(&done.clip, is_tempo), [0, 20, 40, 60]);
    let moves: Vec<_> = done.moved.iter().map(|m| (m.from.0, m.to.0)).collect();
    assert_eq!(moves, [(29, 20), (30, 40)]);
}

/// The same rule at a division where 1/24 quarter is not a whole tick: at 100
/// ticks per quarter the whole-tick clock positions are every 25 ticks.
/// Spec: m2-104/7.5.3#4
#[test]
fn the_clock_grid_of_an_odd_division_is_its_whole_tick_positions() {
    let smf = crate::common::format0(
        SmfDivision::per_quarter(100),
        track(vec![(30, tempo_event(400_000)), (60, note(0, 60))]),
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    assert_eq!(at_ticks(&clip, is_tempo), [0, 25]);
}

/// A tempo whose nearest grid position is past the end of the clip goes to
/// the one before it, so the clip keeps its length: 96 ticks per quarter has
/// a grid of 4 ticks, 90 rounds to 92, past an end of 90, so 88.
/// Spec: m2-104/7.5.3#4
#[test]
fn a_tempo_does_not_move_past_the_end_of_the_clip() {
    let smf = crate::common::format0(
        SmfDivision::per_quarter(96),
        Track::new().push(Tick(90), tempo_event(400_000)).unwrap(),
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    assert_eq!(at_ticks(&clip, is_tempo), [0, 88]);
}

/// M2-104 section 7.5.4: in a file a Set Time Signature sits at a bar
/// boundary. 3/4 at 480 ticks per quarter is 1440 ticks a bar, so a signature
/// at 1500 goes to 1440; 6/8 is 1440 ticks a bar too (six eighths), counted
/// from that moved signature at 1440, so a signature at 4160 goes to 4320, the
/// bar line nearer than 2880.
/// Spec: m2-104/7.5.4#3, m2-116/6.1.2#5, m2-116/7.1.2#3, m2-116/6.1.2#4
#[test]
fn a_time_signature_off_a_bar_line_moves_to_the_nearest_bar_boundary() {
    let smf = format0(vec![
        (0, signature_meta(3, 2)),
        (1500, signature_meta(6, 3)),
        (4160, signature_meta(2, 2)),
        (4400, note(0, 60)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert_eq!(at_ticks(&done.clip, is_signature), [0, 1440, 4320]);
    let moves: Vec<_> = done.moved.iter().map(|m| (m.from.0, m.to.0)).collect();
    assert_eq!(moves, [(1500, 1440), (4160, 4320)]);
}

/// Before any signature the bars are 4/4, 1920 ticks at 480 per quarter, so a
/// first signature at 1000 goes to 1920.
/// Spec: m2-104/7.5.4#3
#[test]
fn bars_before_the_first_time_signature_are_four_four() {
    let smf = format0(vec![(1000, signature_meta(3, 2)), (2000, note(0, 60))]);
    let done = metrical(&smf).to_clip().unwrap();
    // The default 4/4 at tick 0 leads, then the file's own at the next bar.
    assert_eq!(at_ticks(&done.clip, is_signature), [0, 1920]);
}

/// An event already on its grid is not moved, and nothing is reported.
/// Spec: m2-104/7.5.3#4, m2-104/7.5.4#3
#[test]
fn events_on_their_grids_are_not_moved() {
    let smf = format0(vec![
        (960, tempo_event(250_000)),
        (1920, signature_meta(3, 2)),
        (1920 + 1440, tempo_event(300_000)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert!(done.moved.is_empty());
    assert_eq!(at_ticks(&done.clip, is_tempo), [0, 960, 3360]);
}

fn smf_at(tpq: u16, events: Vec<(u64, SmfEvent)>) -> StandardMidiFile {
    crate::common::format0(SmfDivision::per_quarter(tpq), track(events)).unwrap()
}

fn flex_events(clip: &ClipFile, wanted: fn(&Ump) -> bool) -> Vec<(u64, Ump)> {
    events_of(clip)
        .iter()
        .filter(|(_, e)| wanted(e))
        .map(|(t, e)| (t.0, *e))
        .collect()
}

/// A clip holds one Set Time Signature at a position (M2-104 section 7.5.4,
/// M2-116 section 6.1.2). At 96 ticks per quarter a 4/4 bar is 384 ticks:
/// signatures at 200 and 300 both round to the bar line at 384, the later
/// (2/4) wins, the earlier (3/4) is reported superseded, and nothing panics
/// counting a bar from a position past the second one's own tick.
/// Spec: m2-104/7.5.4#3, m2-116/6.1.2#3
#[test]
fn two_time_signatures_that_snap_to_one_bar_line_leave_the_later() {
    let smf = smf_at(
        96,
        vec![
            (200, signature_meta(3, 2)),
            (300, signature_meta(2, 2)),
            (400, note(0, 60)),
        ],
    );
    let done = metrical(&smf).to_clip().unwrap();
    let [sig, metronome] = signature_events(2, 2, 8, 24);
    // The default 4/4 at tick 0, then the later signature at 384.
    let got = flex_events(&done.clip, |e| {
        is_signature(e) || matches!(message(e), Some(Message::Flex(_, Flex::SetMetronome(_))))
    });
    let [default_sig, default_metronome] = signature_events(4, 2, 8, 24);
    assert_eq!(
        got,
        [
            (0, default_sig),
            (0, default_metronome),
            (384, sig),
            (384, metronome)
        ]
    );
    let dropped: Vec<_> = done.dropped.iter().map(|d| (d.tick.0, d.reason)).collect();
    assert_eq!(dropped, [(Tick(200).0, DropReason::Superseded)]);
    let moves: Vec<_> = done.moved.iter().map(|m| (m.from.0, m.to.0)).collect();
    assert_eq!(moves, [(300, 384)]);
}

/// Two tempos that snap to one 1/24-quarter position (every 20 ticks at 480
/// per quarter; 22 and 28 both go to 20) leave the later; the earlier is
/// reported superseded. M2-104 section 7.5.3 gives a tempo a position, one
/// message there.
/// Spec: m2-104/7.5.3#4, m2-116/6.1.1#3
#[test]
fn two_tempos_that_snap_to_one_position_leave_the_later() {
    let smf = format0(vec![
        (22, tempo_event(400_000)),
        (28, tempo_event(300_000)),
        (60, note(0, 60)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert_eq!(
        flex_events(&done.clip, is_tempo),
        [
            (0, flex(Flex::SetTempo(DEFAULT_TEMPO))),
            (20, flex(Flex::SetTempo(FlexTempo::new(30_000_000))))
        ]
    );
    let dropped: Vec<_> = done.dropped.iter().map(|d| (d.tick.0, d.reason)).collect();
    assert_eq!(dropped, [(22, DropReason::Superseded)]);
}

/// A Set Tempo of 0 us has no Flex form (the field 0 is none, M2-104 section
/// 7.5.3): it is dropped as `ZeroTempo`, it is not the clip's first tempo (the
/// default 120 BPM stands at tick 0, M2-116 section 7.1.1) and it does not
/// supersede the real tempo it lands on.
/// Spec: m2-104/7.5.3#4, m2-116/7.1.1#1
#[test]
fn a_zero_tempo_is_dropped_and_is_not_the_first_tempo() {
    let smf = format0(vec![
        (0, tempo_event(0)),
        (480, tempo_event(250_000)),
        (480, tempo_event(0)),
        (960, note(0, 60)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert_eq!(
        flex_events(&done.clip, is_tempo),
        [
            (0, flex(Flex::SetTempo(DEFAULT_TEMPO))),
            (480, flex(Flex::SetTempo(FlexTempo::new(25_000_000)))),
        ]
    );
    let dropped: Vec<_> = done.dropped.iter().map(|d| (d.tick.0, d.reason)).collect();
    assert_eq!(
        dropped,
        [(0, DropReason::ZeroTempo), (480, DropReason::ZeroTempo)]
    );
}

/// A tempo at tick 5 of 480 per quarter snaps to tick 0, where the clip
/// already has the file's own tempo: no default joins it, so the header holds
/// one tempo (M2-116 section 6.1.1) and the clip survives its bytes.
/// Spec: m2-116/6.1.1#3, m2-116/7.1.1#1
#[test]
fn a_tempo_that_snaps_to_tick_zero_is_the_only_one_there() {
    let smf = format0(vec![(5, tempo_event(1_000_000)), (50, note(0, 60))]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let again = read_clip(encoded_clip(&clip)).unwrap();
    assert_eq!(again, clip);
    assert_eq!(
        flex_events(&again, is_tempo),
        [(0, flex(Flex::SetTempo(FlexTempo::new(100_000_000))))]
    );
}

/// The same for a Time Signature at tick 5, snapped to the bar line at 0.
/// Spec: m2-116/6.1.2#3, m2-116/7.1.2#1
#[test]
fn a_signature_that_snaps_to_tick_zero_is_the_only_one_there() {
    let smf = format0(vec![(5, signature_meta(3, 2)), (50, note(0, 60))]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let again = read_clip(encoded_clip(&clip)).unwrap();
    assert_eq!(again, clip);
    let [sig, _] = signature_events(3, 2, 8, 24);
    assert_eq!(flex_events(&again, is_signature), [(0, sig)]);
}

/// A tempo and a Time Signature at the last tick a file can reach in one
/// gap (0x0FFF_FFFF, the SMF delta limit) land on a grid position at or before
/// the clip's end and within a step of where they were. (The arithmetic at
/// the top of `u64` is pinned in `src/file/to_clip/tests.rs`.)
/// Spec: m2-104/7.5.3#4, m2-104/7.5.4#3
#[test]
fn ticks_at_the_largest_gap_stay_on_their_grids() {
    let at = 0x0FFF_FFFF - 3;
    let smf = format0(vec![(at, tempo_event(400_000)), (at, signature_meta(3, 2))]);
    let clip = metrical(&smf).to_clip().unwrap().clip;
    let tempo = flex_events(&clip, is_tempo).pop().unwrap().0;
    let signature = flex_events(&clip, is_signature).pop().unwrap().0;
    // 480 per quarter: tempo grid every 20 ticks, and bar lines of the
    // default 4/4 (1920 ticks) before the file's first signature.
    assert!(tempo <= at && at - tempo < 20 && tempo.is_multiple_of(20));
    assert!(signature <= at && at - signature < 1920 && signature.is_multiple_of(1920));
}

/// A Time Signature that replaces another counts its bars from the signature
/// before the replaced one. At 96 ticks per quarter, 1/4 from tick 0 has a bar
/// of 96 ticks, so 4/4 at 200 sits at 192 (the bar line nearest) and has bars
/// of 384. 3/4 at 250 is within half of those bars of 192, which would put it
/// there, replacing the 4/4; counted on the 1/4 bars of 96 it is nearest 288.
/// The 4/4 is reported superseded and the 3/4 sits at 288.
/// Spec: m2-104/7.5.4#3, m2-116/6.1.2#3
#[test]
fn a_replacing_signature_counts_its_bars_from_the_one_before_the_replaced() {
    let smf = smf_at(
        96,
        vec![
            (0, signature_meta(1, 2)),
            (200, signature_meta(4, 2)),
            (250, signature_meta(3, 2)),
            (400, note(0, 60)),
        ],
    );
    let done = metrical(&smf).to_clip().unwrap();
    let ticks = at_ticks(&done.clip, is_signature);
    assert_eq!(ticks, [0, 288]);
    let [three_four, _] = signature_events(3, 2, 8, 24);
    assert_eq!(
        flex_events(&done.clip, is_signature).pop(),
        Some((288, three_four))
    );
    let dropped: Vec<_> = done.dropped.iter().map(|d| (d.tick.0, d.reason)).collect();
    assert_eq!(dropped, [(200, DropReason::Superseded)]);
    let moves: Vec<_> = done.moved.iter().map(|m| (m.from.0, m.to.0)).collect();
    assert_eq!(moves, [(250, 288)]);
}

/// The Lyric Data texts of a clip, one string a message: M2-104 section
/// 7.5.10 puts a syllable in one message, so the short ones here are one
/// Complete packet each.
fn lyrics_of(file: &ClipFile) -> Vec<(u64, String)> {
    events_of(file)
        .iter()
        .filter_map(|(tick, e)| match message(e) {
            Some(Message::Flex(
                _,
                Flex::Text {
                    kind: FlexTextKind::Lyrics,
                    bytes,
                    ..
                },
            )) => {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                Some((tick.0, String::from_utf8(bytes[..end].to_vec()).unwrap()))
            }
            _ => None,
        })
        .collect()
}

fn lyrics_of_smf(lyrics: &[&str]) -> Vec<(u64, String)> {
    let events = lyrics
        .iter()
        .zip(0u64..)
        .map(|(l, i)| (i * 10, text(TextKind::Lyric, l)))
        .collect();
    lyrics_of(&metrical(&format0(events)).to_clip().unwrap().clip)
}

fn lyric_texts(lyrics: &[&str]) -> Vec<String> {
    lyrics_of_smf(lyrics).into_iter().map(|l| l.1).collect()
}

/// M2-104 section 7.5.10: a hyphen is only for a hyphenated word, and a
/// syllable with no space, CR or LF at its end is followed by the rest of its
/// word, so the karaoke hyphen that says the same is not carried. A hyphen
/// inside a word stays.
/// Spec: m2-104/7.5.10#12
#[test]
fn a_karaoke_hyphen_ends_no_syllable_in_the_clip() {
    assert_eq!(lyric_texts(&["Hel-", "lo "]), ["Hel", "lo "]);
    assert_eq!(lyric_texts(&["sixty-four "]), ["sixty-four "]);
    assert_eq!(lyric_texts(&["Hel- "]), ["Hel "]);
    // Every trailing hyphen is the mark, so a genuine hyphen there is lost.
    assert_eq!(lyric_texts(&["Hel--", "lo"]), ["Hel", "lo"]);
    assert_eq!(lyric_texts(&["a-- "]), ["a "]);
    assert_eq!(lyric_texts(&["sixty-", "four"]), ["sixty", "four"]);
}

/// A lyric of only hyphens has no syllable and is reported, never dropped
/// silently; a text past the 384 bytes of a message and one with a NUL are
/// reported for what they are.
#[test]
fn a_lyric_that_cannot_be_a_message_is_reported() {
    let smf = format0_lead(
        LeadTrack::new(track(vec![
            (1, text(TextKind::Lyric, "-")),
            (2, text(TextKind::Lyric, "--")),
            (3, text(TextKind::Lyric, &"x".repeat(383))),
            (4, text(TextKind::Lyric, "a\0b")),
        ]))
        .copyright(&"x".repeat(385)),
    );
    let reasons: Vec<DropReason> = metrical(&smf)
        .to_clip()
        .unwrap()
        .dropped
        .iter()
        .map(|d| d.reason)
        .collect();
    assert_eq!(
        reasons,
        [
            DropReason::Text(FlexTextError::TooLong.into()),
            DropReason::LyricWithoutSyllable,
            DropReason::LyricWithoutSyllable,
            DropReason::Text(FlexTextError::TooLong.into()),
            DropReason::Text(FlexTextError::Nul.into()),
        ]
    );
}

/// M2-104 section 7.5.10: a space at the end of the text ends a word, a CR
/// a line, a CR LF a paragraph. In SMF karaoke the break is at the start of
/// the next event (`/` a line, `\` a paragraph, a space a word); the clip
/// puts it on the syllable it ends.
#[test]
fn karaoke_breaks_end_the_syllable_before_them() {
    assert_eq!(
        lyric_texts(&["Hel", "lo", "/world", "\\Next", " word"]),
        ["Hel", "lo\r", "world\r\n", "Next ", "word"]
    );
    // A break event of its own, and a space at the start of a syllable.
    assert_eq!(
        lyric_texts(&["Hel", "lo", "/", "Go"]),
        ["Hel", "lo\r", "Go"]
    );
    assert_eq!(lyric_texts(&["Hel", " lo"]), ["Hel ", "lo"]);
}

/// What reading a lyric as karaoke changes of its text is reported by event: a
/// leading `/` or `\` that became a syllable end, and hyphens taken off a
/// syllable. Text that is carried as it is, spaces and breaks the spec names
/// included, is not.
#[test]
fn a_lyric_whose_text_changes_is_listed_as_altered() {
    let smf = format0(vec![
        (0, text(TextKind::Lyric, "Hel-")),
        (10, text(TextKind::Lyric, "lo")),
        (20, text(TextKind::Lyric, "/\\world-")),
        (30, text(TextKind::Lyric, "plain ")),
        (40, text(TextKind::Lyric, "-")),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    let altered: Vec<(u64, LyricChange)> =
        done.altered.iter().map(|a| (a.tick.0, a.change)).collect();
    assert_eq!(
        altered,
        [
            (0, LyricChange::HyphenStripped),
            (20, LyricChange::BreakMark),
            (20, LyricChange::HyphenStripped),
        ]
    );
    assert_eq!(done.altered[1].event, text(TextKind::Lyric, "/\\world-"));
    // The lyric of only a hyphen is dropped, not altered.
    assert_eq!(done.dropped.len(), 1);
}

/// M2-104 section 7.5.10: a message is one syllable, so an event of several
/// words is a message each at its tick, the last keeping its own end.
#[test]
fn a_lyric_of_several_words_is_a_message_each_at_its_tick() {
    assert_eq!(
        lyrics_of_smf(&["one two three\r\n"]),
        [
            (0, "one ".to_owned()),
            (0, "two ".to_owned()),
            (0, "three\r\n".to_owned())
        ]
    );
}

/// A lyric with nothing to say that has nothing before it to end is reported.
#[test]
fn a_lyric_of_only_a_break_with_no_syllable_before_is_reported() {
    let smf = format0(vec![
        (0, text(TextKind::Lyric, "/")),
        (5, text(TextKind::Lyric, "-")),
    ]);
    let converted = metrical(&smf).to_clip().unwrap();
    let reasons: Vec<DropReason> = converted.dropped.iter().map(|d| d.reason).collect();
    assert_eq!(
        reasons,
        [
            DropReason::LyricWithoutSyllable,
            DropReason::LyricWithoutSyllable
        ]
    );
}

/// M2-104 section 7.5.10: a syllable is placed at the time its sung note
/// begins. An SMF lyric is at that time, so the clip's Lyric Data message is
/// at the tick of its source event, however many messages the event becomes.
/// Spec: m2-104/7.5.10#2
#[test]
fn a_lyric_keeps_the_tick_of_its_event() {
    let smf = format0(vec![
        (7, text(TextKind::Lyric, "Hel-")),
        (130, text(TextKind::Lyric, "lo ")),
        (480, text(TextKind::Lyric, "big wide")),
        (481, text(TextKind::Lyric, "/world")),
    ]);
    assert_eq!(
        lyrics_of(&metrical(&smf).to_clip().unwrap().clip),
        [
            (7, "Hel".to_owned()),
            (130, "lo ".to_owned()),
            (480, "big ".to_owned()),
            (480, "wide\r".to_owned()),
            (481, "world".to_owned()),
        ]
    );
}

/// M2-116 section 7.1.3: a pickup bar has a Set Time Signature that gives its
/// true length, and the bar after it starts with a new Set Time Signature. A
/// quarter-note pickup (1/4, 480 ticks) then 4/4: the first bar starts at the
/// start of the clip, at tick 0 (section 7.1), the second at tick 480 where its
/// signature is, on the pickup's own bar line, so nothing moves.
/// Spec: m2-116/7.1.3#1, m2-116/7.1.3#2, m2-116/7.1#2
#[test]
fn a_pickup_bar_has_its_own_signature_and_the_next_bar_a_new_one() {
    let smf = format0(vec![
        (0, signature_meta(1, 2)),
        (0, note(0, 60)),
        (480, signature_meta(4, 2)),
        (480, note(0, 62)),
    ]);
    let done = metrical(&smf).to_clip().unwrap();
    assert!(done.moved.is_empty());
    assert_eq!(at_ticks(&done.clip, is_signature), [0, 480]);
    let [pickup, _] = signature_events(1, 2, 8, 24);
    let [bar, _] = signature_events(4, 2, 8, 24);
    let signatures: Vec<_> = flex_events(&done.clip, is_signature)
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    assert_eq!(signatures, [pickup, bar]);
    // The pickup is one beat: the second bar begins a quarter note in.
    assert_eq!(
        Tick(480).beats(done.clip.ticks_per_quarter()),
        Beat::new(1.0)
    );
}

/// Without a signature for the pickup, a 4/4 bar is counted from the start of
/// the clip, and a signature a quarter in is off its bar line: it moves to the
/// nearest one, the clip's start, and is listed.
#[test]
fn a_signature_that_does_not_declare_the_pickup_is_off_the_bar_line() {
    let smf = format0(vec![(480, signature_meta(3, 2)), (480, note(0, 60))]);
    let done = metrical(&smf).to_clip().unwrap();
    let moves: Vec<_> = done.moved.iter().map(|m| (m.from.0, m.to.0)).collect();
    assert_eq!(moves, [(480, 0)]);
}
