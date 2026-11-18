//! Writing a file and playing one: the `Track` builder, the timeline a clip
//! plays, the typed refusals, and the byte-stream doors.
//!
//! The expected figures are written from the specifications (SMF 1.0 for the
//! tick grid and the SysEx event, M2-104 section 7.7 for SysEx7 packets), not
//! read back from the code under test.

use crate::common::metrical;
use std::sync::Arc;

use tutti_midi_device::{ProfileId, ProfileOnRequest, ProfileTarget};
use tutti_midi_file::{
    FileEvent, Meta, MidiFile, OutOfOrder, ReadError, Refusal, SmfDivision, SmfFormat, TextKind,
    Tick, TimelineDrop, Track, TrackPart, TracksError,
};
use tutti_ump::{
    Address, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, SysexStatus,
    TicksPerQuarterNote, U7, Ump, Voice1, Voice2,
};
use tutti_units::{Beat, Bpm};

const TPQ: u16 = 480;
const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(TPQ);
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);

fn on(tick: u64, key: u8) -> (Tick, FileEvent) {
    (
        Tick(tick),
        Message::midi1(
            MidiChannel::FIRST,
            Voice1::note_on(NoteNumber::of(key), U7::of(100)),
        )
        .into(),
    )
}

fn file(tracks: Vec<Track>) -> MidiFile {
    let format = if tracks.len() == 1 {
        SmfFormat::SingleTrack
    } else {
        SmfFormat::Parallel
    };
    MidiFile::smf(format, SMF_DIVISION, tracks).unwrap()
}

// ---- Tick ------------------------------------------------------------------

/// A tick is a count: `checked_add` and `ticks_since` move it by that many
/// ticks, and a quarter note is `ticks_per_quarter` of them.
#[test]
fn a_tick_moves_by_counts_and_a_quarter_is_the_division() {
    assert_eq!(Tick(1_000).checked_add(480), Some(Tick(1_480)));
    assert_eq!(Tick(1_480).ticks_since(Tick(1_000)), Some(480));
    assert_eq!(
        Tick::quarters(3, TicksPerQuarterNote::of(96)),
        Some(Tick(288))
    );
    assert_eq!(Tick(720).beats(TicksPerQuarterNote::of(480)), Beat(1.5));
    let seven = Tick::quarters(7, DIVISION).unwrap();
    assert_eq!(seven.beats(DIVISION), Beat(7.0));
}

/// The ends of `u64` are not wrapped: an add past the last tick, a distance
/// to a later tick and a quarter count past it answer `None`.
#[test]
fn tick_arithmetic_past_the_ends_of_u64_is_none() {
    assert_eq!(Tick(u64::MAX).checked_add(0), Some(Tick(u64::MAX)));
    assert_eq!(Tick(u64::MAX).checked_add(1), None);
    assert_eq!(Tick(u64::MAX - 5).checked_add(6), None);
    assert_eq!(Tick(9).ticks_since(Tick(10)), None);
    assert_eq!(Tick(10).ticks_since(Tick(10)), Some(0));
    let two = TicksPerQuarterNote::of(2);
    assert_eq!(Tick::quarters(u64::MAX / 2, two), Some(Tick(u64::MAX - 1)));
    assert_eq!(Tick::quarters(u64::MAX / 2 + 1, two), None);
}

// ---- the builder -----------------------------------------------------------

/// A note is a Note On at its start and a Note Off at its end on its channel, and
/// the track ends where the last event does.
#[test]
fn a_note_is_an_on_at_its_start_and_an_off_at_its_end() {
    let ch = MidiChannel::of(4);
    let track = Track::new().note(Tick(100), Tick(150), ch, NoteNumber::of(60), U7::of(90));
    let at = |m| Address::first(ch) == m;
    assert_eq!(track.events().len(), 2);
    let (t0, e0) = &track.events()[0];
    let (t1, e1) = &track.events()[1];
    assert_eq!((*t0, *t1), (Tick(100), Tick(150)));
    assert!(matches!(
        e0.message(),
        Some(Message::Midi1(a, Voice1::NoteOn { note, velocity }))
            if at(*a) && note.get() == 60 && velocity.get() == 90
    ));
    assert!(matches!(
        e1.message(),
        Some(Message::Midi1(a, Voice1::NoteOff { note, .. })) if at(*a) && note.get() == 60
    ));
    assert_eq!(track.end(), Tick(150));
}

/// A note whose end is before its start is a note of no length, at the
/// start: its Note Off is never before its Note On.
#[test]
fn a_note_ending_before_it_starts_has_no_length() {
    let track = Track::new().note(
        Tick(100),
        Tick(40),
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    );
    let ticks: Vec<Tick> = track.events().iter().map(|(t, _)| *t).collect();
    assert_eq!(ticks, [Tick(100), Tick(100)]);
    assert_eq!(track.end(), Tick(100));
}

/// Notes added in any order, a chord and overlapping notes all leave the
/// events in non-decreasing tick order, which the file layer requires.
#[test]
fn notes_in_any_order_leave_the_events_in_tick_order() {
    let ch = MidiChannel::FIRST;
    let n = |k| NoteNumber::of(k);
    let track = Track::new()
        .note(Tick(480), Tick(960), ch, n(64), U7::of(100))
        .note(Tick(0), Tick(960), ch, n(60), U7::of(100)) // overlaps the first
        .note(Tick(0), Tick(480), ch, n(67), U7::of(100)); // a chord with it
    let ticks: Vec<u64> = track.events().iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 0, 480, 480, 960, 960]);
    assert_eq!(track.end(), Tick(960));
    // And a file takes it as it is.
    MidiFile::smf(SmfFormat::SingleTrack, SMF_DIVISION, vec![track]).unwrap();
}

/// `meta(tick, Meta::tempo(bpm))` is the one way to place a Set Tempo, and
/// like any meta event it is placed by tick.
#[test]
fn a_tempo_is_a_meta_event_at_its_tick() {
    let track = Track::new()
        .meta(Tick(960), Meta::text(TextKind::Marker, "B"))
        .meta(Tick(0), Meta::tempo(Bpm::of(90.0)));
    assert_eq!(
        track.events(),
        [
            (Tick(0), FileEvent::Meta(Meta::tempo(Bpm::of(90.0)))),
            (
                Tick(960),
                FileEvent::Meta(Meta::text(TextKind::Marker, "B"))
            ),
        ]
    );
    assert_eq!(track.end(), Tick(960));
}

/// `push` appends: an event before the last is refused with both ticks, and
/// the track is as it was; at the same tick is allowed.
#[test]
fn push_refuses_an_event_before_the_last() {
    let track = Track::new()
        .push(
            Tick(10),
            Message::midi1(
                MidiChannel::FIRST,
                Voice1::note_on(NoteNumber::of(60), U7::of(1)),
            ),
        )
        .unwrap()
        .push(Tick(10), Meta::tempo(Bpm::of(100.0)))
        .unwrap();
    assert_eq!(track.events().len(), 2);
    assert_eq!(track.end(), Tick(10));
    let err = track
        .clone()
        .push(Tick(9), Meta::tempo(Bpm::of(100.0)))
        .unwrap_err();
    assert_eq!(
        err,
        OutOfOrder {
            last: Tick(10),
            at: Tick(9)
        }
    );
    assert!(err.to_string().contains("tick 9"), "{err}");
}

/// Collecting pairs puts them in tick order, stable: events at one tick keep
/// the order they were given in, so a Note Off before a Note On stays so.
#[test]
fn collecting_events_sorts_them_by_tick_keeping_ties_in_order() {
    let (a, b, c) = (on(480, 1), on(0, 2), on(480, 3));
    let track: Track = [a.clone(), b.clone(), c.clone()].into_iter().collect();
    assert_eq!(track.events(), [b.clone(), a.clone(), c.clone()]);
    assert_eq!(track.end(), Tick(480));
    // Extending an existing track merges into the same order.
    let mut more = Track::new().push(b.0, b.1.clone()).unwrap();
    more.extend([c.clone(), a.clone()]);
    assert_eq!(more.events(), [b, c, a]);
    assert_eq!(more.end(), Tick(480));
}

// ---- the timeline ----------------------------------------------------------

/// The timeline of an SMF is its messages as their packets at tick over
/// division beats, in tick order across tracks, with the tempo and text meta
/// events left out (the tempo is the file's map).
#[test]
fn the_timeline_is_the_messages_at_their_beats_and_not_the_meta_events() {
    let tempo_track = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(60.0)))
        .meta(Tick(0), Meta::text(TextKind::TrackName, "tempo"));
    let notes = Track::new()
        .note(
            Tick(480),
            Tick(720),
            MidiChannel::FIRST,
            NoteNumber::of(60),
            U7::of(100),
        )
        .note(
            Tick(0),
            Tick(120),
            MidiChannel::of(1),
            NoteNumber::of(62),
            U7::of(80),
        );
    let f = file(vec![tempo_track, notes]);
    let line: Vec<(Beat, Ump)> = metrical(&f).timeline().unwrap().packets;
    let words = |ump: &Ump| ump.0[0];
    let got: Vec<(f64, u32)> = line.iter().map(|(b, u)| (b.get(), words(u))).collect();
    // M2-104 7.3: type 2, group 0, status, channel, note, velocity.
    assert_eq!(
        got,
        [
            (0.0, 0x2091_3E50),  // channel 1, note 62, velocity 80
            (0.25, 0x2081_3E40), // its off, release 64
            (1.0, 0x2090_3C64),  // channel 0, note 60, velocity 100
            (1.5, 0x2080_3C40),
        ]
    );
}

/// A SysEx event goes out as its SysEx7 packets (M2-104 section 7.7) on
/// the first Group, all at the event's beat: ten payload bytes are a Start of six
/// and an End of four. A divided message that is not ended (`F0` with no
/// closing `F7`) and one with a byte of 128 or more are listed as dropped.
#[test]
fn a_sysex_event_is_its_sysex7_packets_at_its_beat() {
    let mut sysex = vec![0xF0];
    sysex.extend(1..=10u8);
    sysex.push(0xF7);
    let track = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
        .push(Tick(960), FileEvent::SysEx(sysex[1..].into()))
        .unwrap()
        .push(Tick(960), FileEvent::SysEx(vec![1, 2, 3].into())) // divided: no F7
        .unwrap()
        .push(Tick(960), FileEvent::SysEx(vec![0x90, 0xF7].into())) // 8-bit data
        .unwrap();
    let f = file(vec![track]);
    let timeline = metrical(&f).timeline().unwrap();
    assert_eq!(timeline.packets.len(), 2);
    assert!(timeline.packets.iter().all(|(b, _)| *b == Beat(2.0)));
    assert_eq!(
        sysex7(&timeline.packets),
        [
            (Beat(2.0), SysexStatus::Start, vec![1, 2, 3, 4, 5, 6]),
            (Beat(2.0), SysexStatus::End, vec![7, 8, 9, 10]),
        ]
    );
    // The unended `F0` is dropped when the next event ends it unfinished,
    // and the 8-bit one is dropped with its bytes.
    assert_eq!(
        timeline.dropped,
        [
            (
                Beat(2.0),
                FileEvent::SysEx(vec![1, 2, 3].into()),
                TimelineDrop::Unfinished
            ),
            (
                Beat(2.0),
                FileEvent::SysEx(vec![0x90, 0xF7].into()),
                TimelineDrop::NotSevenBit
            ),
        ]
    );
}

/// The SysEx7 packets of a timeline as `(beat, status, data)`.
fn sysex7(packets: &[(Beat, Ump)]) -> Vec<(Beat, SysexStatus, Vec<u8>)> {
    packets
        .iter()
        .map(|(beat, u)| match u.decode() {
            Ok(Message::Sysex7(group, p)) => {
                assert_eq!(group, MidiGroup::FIRST);
                (*beat, p.status(), p.data().to_vec())
            }
            other => panic!("not SysEx7: {other:?}"),
        })
        .collect()
}

/// SMF 1.0 section 3.2's example: `F0 03 43 12 00`, 200 ticks, `F7 06 43 12 00
/// 43 12 00`, 100 ticks, `F7 04 43 12 00 F7` is the bytes `F0 43 12 00`, then
/// `43 12 00 43 12 00`, then `43 12 00 F7`. Each part is a SysEx7 packet of its
/// own at its own beat (M2-104 section 7.7: Start, Continue, End), and the
/// message's `F0` and `F7` are not in the packets.
#[test]
fn a_divided_sysex_is_a_start_continues_and_an_end_each_at_its_events_beat() {
    let track = Track::new()
        .push(Tick(0), FileEvent::SysEx(vec![0x43, 0x12, 0x00].into()))
        .unwrap()
        .push(
            Tick(200),
            FileEvent::Escape(vec![0x43, 0x12, 0x00, 0x43, 0x12, 0x00].into()),
        )
        .unwrap()
        .push(
            Tick(300),
            FileEvent::Escape(vec![0x43, 0x12, 0x00, 0xF7].into()),
        )
        .unwrap();
    let f = file(vec![track]);
    let timeline = metrical(&f).timeline().unwrap();
    assert!(timeline.dropped.is_empty());
    let beat = |ticks: f64| Beat(ticks / f64::from(TPQ));
    assert_eq!(
        sysex7(&timeline.packets),
        [
            (beat(0.0), SysexStatus::Start, vec![0x43, 0x12, 0x00]),
            (
                beat(200.0),
                SysexStatus::Continue,
                vec![0x43, 0x12, 0x00, 0x43, 0x12, 0x00]
            ),
            (beat(300.0), SysexStatus::End, vec![0x43, 0x12, 0x00]),
        ]
    );
}

/// A part of more than six bytes is several packets at its event's beat, and a
/// meta event between the parts does not end the message.
#[test]
fn a_divided_sysex_part_over_six_bytes_is_several_packets_and_a_meta_event_may_intervene() {
    let part: Vec<u8> = (1..=8).collect();
    let track = Track::new()
        .push(Tick(0), FileEvent::SysEx(part.clone().into()))
        .unwrap()
        .push(Tick(10), Meta::text(TextKind::Marker, "m"))
        .unwrap()
        .push(Tick(20), FileEvent::Escape(vec![9, 0xF7].into()))
        .unwrap();
    let timeline = metrical(&file(vec![track])).timeline().unwrap();
    let beat = |ticks: f64| Beat(ticks / f64::from(TPQ));
    assert_eq!(
        sysex7(&timeline.packets),
        [
            (beat(0.0), SysexStatus::Start, vec![1, 2, 3, 4, 5, 6]),
            (beat(0.0), SysexStatus::Continue, vec![7, 8]),
            (beat(20.0), SysexStatus::End, vec![9]),
        ]
    );
}

/// SMF 1.0 section 3.2: an `F7` event with no `F0` before it is an escape of
/// arbitrary bytes, which no SysEx7 packet holds, and it is reported; a divided
/// message with a transmittable event between its parts, or never ended, is
/// reported whole and its parts send nothing.
#[test]
fn a_bare_escape_and_an_unfinished_divided_message_are_dropped_and_listed() {
    let note = Message::midi1(
        MidiChannel::FIRST,
        Voice1::note_on(NoteNumber::of(60), U7::of(100)),
    );
    let escape = FileEvent::Escape(vec![0xF3, 0x01].into());
    let first = FileEvent::SysEx(vec![1, 2].into());
    let second = FileEvent::Escape(vec![3, 0xF7].into());
    let track = Track::new()
        .push(Tick(0), escape.clone())
        .unwrap()
        .push(Tick(10), first.clone())
        .unwrap()
        .push(Tick(20), note)
        .unwrap()
        .push(Tick(30), second.clone())
        .unwrap()
        .push(Tick(40), first.clone())
        .unwrap();
    let timeline = metrical(&file(vec![track])).timeline().unwrap();
    // The note is the only packet; the `F7` after it has no `F0` open.
    assert_eq!(timeline.packets.len(), 1);
    let beat = |ticks: f64| Beat(ticks / f64::from(TPQ));
    assert_eq!(
        timeline.dropped,
        [
            (beat(0.0), escape, TimelineDrop::BareEscape),
            (beat(10.0), first.clone(), TimelineDrop::Unfinished),
            (beat(30.0), second, TimelineDrop::BareEscape),
            (beat(40.0), first, TimelineDrop::Unfinished),
        ]
    );
}

/// A format 2 file has no one timeline; each of its sequences has its own, on
/// its own division.
#[test]
fn a_format_2_file_has_no_timeline_but_each_track_has() {
    let a = Track::new().note(
        Tick(480),
        Tick(960),
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    );
    let f = MidiFile::smf(SmfFormat::Sequential, SMF_DIVISION, vec![a, Track::new()]).unwrap();
    assert!(metrical(&f).timeline().is_err());
    let beats: Vec<Beat> = f.tracks()[0]
        .timeline(metrical(&f).ticks_per_quarter())
        .packets
        .into_iter()
        .map(|(b, _)| b)
        .collect();
    assert_eq!(beats, [Beat(1.0), Beat(2.0)]);
}

/// A divided message with a byte of 128 or more is dropped whole, each part
/// listed as `NotSevenBit`, and a Clip File packet no definition covers is
/// listed as `UnknownPacket`.
#[test]
fn an_eight_bit_divided_message_and_an_unknown_packet_are_listed() {
    let part = FileEvent::SysEx(vec![1, 0x90].into());
    let end = FileEvent::Escape(vec![2, 0xF7].into());
    let track = Track::new()
        .push(Tick(0), part.clone())
        .unwrap()
        .push(Tick(10), end.clone())
        .unwrap();
    let timeline = metrical(&file(vec![track])).timeline().unwrap();
    assert!(timeline.packets.is_empty());
    let beat = |ticks: f64| Beat(ticks / f64::from(TPQ));
    assert_eq!(
        timeline.dropped,
        [
            (beat(0.0), part, TimelineDrop::NotSevenBit),
            (beat(10.0), end, TimelineDrop::NotSevenBit),
        ]
    );
    // Message type 0x6 is reserved: nothing decodes it.
    let reserved = Ump([0x6001_2345, 0, 0, 0]);
    let known = Message::Midi2(
        MidiChannel::FIRST.into(),
        Voice2::ChannelPressure(tutti_ump::ControlValue::MIN),
    );
    let clip = MidiFile::from_bytes(
        &MidiFile::clip(
            DIVISION,
            Track::new()
                .push(Tick(0), FileEvent::Unknown(tutti_ump::Unknown(reserved)))
                .unwrap()
                .push(Tick(0), known)
                .unwrap(),
        )
        .unwrap()
        .to_bytes(),
    )
    .unwrap();
    let timeline = metrical(&clip).timeline().unwrap();
    assert_eq!(timeline.packets.len(), 1);
    assert_eq!(timeline.dropped.len(), 1);
    assert_eq!(timeline.dropped[0].2, TimelineDrop::UnknownPacket);
}

/// A Clip File's packets are the timeline as they are stored (a MIDI 2.0 note
/// stays MIDI 2.0), and its configuration header is not on it.
#[test]
fn a_clip_files_timeline_is_its_packets_as_stored_without_the_header() {
    let note = Message::Midi2(
        MidiChannel::FIRST.into(),
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
    );
    let track = Track::new()
        .profile_on(ProfileOnRequest {
            target: ProfileTarget::Group,
            profile: ProfileId::MPE,
        })
        .push(Tick(240), note)
        .unwrap();
    assert!(!track.header().is_empty());
    let f = MidiFile::clip(DIVISION, track).unwrap();
    let line: Vec<(Beat, Ump)> = metrical(&f).timeline().unwrap().packets;
    assert_eq!(line, [(Beat(0.5), Ump::from(note))]);
}

// ---- the tempo map ---------------------------------------------------------

/// The tempo map is shared, not copied: a clone of the file and the accessor
/// for the one timeline's track hand out the one map.
#[test]
fn the_tempo_map_is_one_shared_value() {
    let f = file(vec![Track::new().meta(Tick(0), Meta::tempo(Bpm::of(75.0)))]);
    let again = f.clone();
    assert!(Arc::ptr_eq(
        metrical(&f).tempo_map(),
        metrical(&again).tempo_map()
    ));
    assert!(Arc::ptr_eq(
        metrical(&f).tempo_map(),
        metrical(&f).tempo_maps().next().unwrap()
    ));
    assert_eq!(metrical(&f).tempo_map().tempo_at(Beat(0.0)), Bpm::of(75.0));
}

// ---- typed refusals --------------------------------------------------------

/// Each rule a track list breaks is its own variant, with the track and the
/// tick that broke it.
#[test]
fn a_refused_track_list_names_the_rule_and_where() {
    let config = |r: Result<MidiFile, TracksError>| r.expect_err("the list is refused");
    assert_eq!(
        config(MidiFile::smf(SmfFormat::Parallel, SMF_DIVISION, vec![])),
        TracksError::NoTracks
    );
    assert_eq!(
        config(MidiFile::smf(
            SmfFormat::SingleTrack,
            SMF_DIVISION,
            vec![Track::new(), Track::new()]
        )),
        TracksError::SingleTrack { tracks: 2 }
    );
    // An SMF holds the first Group only.
    let g2 = Message::midi1(
        Address::new(MidiGroup::of(1), MidiChannel::FIRST),
        Voice1::note_on(NoteNumber::of(60), U7::of(1)),
    );
    let t = Track::new().push(Tick(0), g2).unwrap();
    assert_eq!(
        config(MidiFile::smf(SmfFormat::SingleTrack, SMF_DIVISION, vec![t])),
        TracksError::Refused {
            track: 0,
            part: TrackPart::Events,
            refusal: Refusal::SmfGroup
        }
    );
}

// ---- bytes in and out ------------------------------------------------------

/// A byte slice reads back the file `to_bytes` gives.
#[test]
fn a_file_round_trips_through_a_slice() {
    let f = file(vec![
        Track::new()
            .meta(Tick(0), Meta::tempo(Bpm::of(100.0)))
            .note(
                Tick(0),
                Tick(480),
                MidiChannel::FIRST,
                NoteNumber::of(60),
                U7::of(100),
            ),
    ]);
    let out = f.to_bytes();
    assert_eq!(MidiFile::try_from(out.as_slice()).unwrap(), f);
    // Bytes of neither container are refused as the other doors refuse them.
    assert!(matches!(
        MidiFile::try_from(&b"RIFF...."[..]),
        Err(ReadError::UnknownFormat)
    ));
}

/// Two Set Tempos at one tick are both kept, in the order added, and neither is
/// refused: an SMF may hold any number of them, and the later one governs the
/// tempo map (the reader's rule, so a file written and read back agrees).
#[test]
fn two_tempos_on_one_tick_are_kept_and_the_later_governs() {
    let track = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(90.0)))
        .meta(Tick(0), Meta::tempo(Bpm::of(60.0)));
    assert_eq!(
        track.events(),
        [
            (Tick(0), FileEvent::Meta(Meta::tempo(Bpm::of(90.0)))),
            (Tick(0), FileEvent::Meta(Meta::tempo(Bpm::of(60.0)))),
        ]
    );
    let file = MidiFile::smf(
        SmfFormat::SingleTrack,
        SmfDivision::per_quarter(480),
        vec![track],
    )
    .unwrap();
    assert_eq!(
        metrical(&file).tempo_map().tempo_at(Beat(0.0)),
        Bpm::of(60.0)
    );
}
