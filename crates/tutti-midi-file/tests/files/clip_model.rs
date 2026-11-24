//! The MIDI 2.0 Clip File half of [`MidiFile`], against streams of UMP words
//! laid out by hand from M2-116 (the structure rules are about *order*, which
//! only a hand-built stream can get wrong on purpose).

use crate::common;
use crate::common::{encoded, encoded_clip, read_bytes, read_clip};

use tutti_fixtures::midi;
use tutti_midi_ci::{NotSetProfileOn, ProfileChannels, ProfileId, ProfileOnRequest, ProfileTarget};
use tutti_midi_file::{
    ClipBuildError, ClipFile, ClipFileError, ClipHeader, ClipPart, ClipReadError, ClipSequence,
    FileEvent, MidiFile, MidiFileKind, ReadError, SmfDivision, Tick,
};
use tutti_music::{Meter, TimeSignature};
use tutti_ump::ControlValue;
use tutti_ump::{
    Address, DeltaTicks, Flex, FlexTempo, JrTicks, Message, MidiChannel, MidiGroup, NoteNumber,
    NoteVelocity, Stream, TicksPerQuarterNote, U7, Ump, Utility, Voice1,
};
use tutti_units::{BarNumber, Beat, Bpm, Seconds};

const TPQ: u16 = 96;
const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(TPQ);
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(TPQ);
const MAGIC: &[u8; 8] = b"SMF2CLIP";

fn utility(u: Utility) -> Ump {
    Message::Utility(u).into()
}
fn dcs(ticks: u32) -> Ump {
    utility(Utility::DeltaClockstamp(DeltaTicks::masked(ticks)))
}
/// A Ticks Per Quarter Note packet, a reserved zero included: a sender cannot
/// name zero, so the packet is built from its words.
fn dctpq(tpq: u16) -> Ump {
    Ump([0x0030_0000 | u32::from(tpq), 0, 0, 0])
}
fn start() -> Ump {
    Message::Stream(Stream::StartOfClip).into()
}
fn end() -> Ump {
    Message::Stream(Stream::EndOfClip).into()
}
fn on(note: u8, vel: u16) -> Ump {
    midi::note_on(MidiChannel::FIRST, note, vel)
}
fn off(note: u8) -> Ump {
    midi::note_off(MidiChannel::FIRST, note, 0)
}
fn set_tempo(bpm: f64) -> Ump {
    Message::Flex(
        MidiGroup::FIRST,
        Flex::SetTempo(FlexTempo::new((6.0e9 / bpm) as u32)),
    )
    .into()
}

/// The bytes of a clip made of exactly `parts`, in order.
fn clip(parts: &[Ump]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for ump in parts {
        for w in ump.words() {
            out.extend_from_slice(&w.to_be_bytes());
        }
    }
    out
}

/// A valid clip: DCTPQ, Start of Clip, a note a beat long, End of Clip a beat
/// after it.
fn valid() -> Vec<Ump> {
    vec![
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        on(60, 0xABCD),
        dcs(u32::from(TPQ)),
        off(60),
        dcs(u32::from(TPQ)),
        end(),
    ]
}

/// What reading a clip made of `parts` refused: the index of the packet whose
/// first word is the word the error names, and the rule broken.
fn clip_error(parts: &[Ump]) -> (usize, ClipFileError) {
    match read_clip(clip(parts)) {
        Err(ClipReadError::Parse { word, cause }) => {
            let mut at = 0;
            let packet = parts
                .iter()
                .position(|ump| {
                    let found = at == word;
                    at += ump.words().len();
                    found
                })
                // A word past the last packet is the end of the stream.
                .or((at == word).then_some(parts.len()))
                .unwrap_or_else(|| panic!("word {word} is inside a packet"));
            (packet, cause)
        }
        other => panic!("expected a clip error, got {other:?}"),
    }
}

/// The sequence of `events`, each `(tick, packet)`.
fn sequence(events: Vec<(u64, Ump)>) -> ClipSequence {
    events.into_iter().map(|(t, e)| (Tick(t), e)).collect()
}

/// Spec: m2-116/3.2.2#1, m2-116/hdr/1, m2-116/hdr/2, m2-116/hdr/3, m2-116/hdr/4, m2-116/hdr/5, m2-116/hdr/6, m2-116/hdr/7, m2-116/hdr/8, m2-116/5#2
#[test]
fn a_clip_reads_at_absolute_ticks() {
    let file = read_clip(clip(&valid())).unwrap();
    assert_eq!(
        MidiFileKind::detect(encoded_clip(&file)).unwrap(),
        Some(MidiFileKind::ClipFile)
    );
    assert_eq!(file.ticks_per_quarter(), DIVISION);
    let events = &file.sequence().events();
    assert_eq!(events.len(), 2, "structure is not an event");
    assert_eq!(events[0].0, Tick(0));
    assert_eq!(events[1].0, Tick(96));
    assert_eq!(events[0].1, on(60, 0xABCD));
    assert_eq!(events[1].0.beats(file.ticks_per_quarter()), Beat::new(1.0));
    // The clip's length is the delta before End of Clip.
    assert_eq!(file.end().beats(file.ticks_per_quarter()), Beat::new(2.0));
    // And a clip's notes keep all 16 bits.
    let notes: Vec<_> = file.sequence().notes();
    assert_eq!(notes.len(), 1);
    assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(96)));
    assert_eq!(notes[0].velocity, NoteVelocity::new(0xABCD));
}

/// **The clip's length is the Delta Clockstamp before End of Clip**, so a clip
/// that ends a bar after its last note is a bar long, and says so after a
/// round trip.
/// Spec: m2-116/7.3#1
#[test]
fn the_clip_length_is_the_delta_before_end_of_clip() {
    let mut parts = valid();
    let n = parts.len();
    parts[n - 2] = dcs(4 * u32::from(TPQ));
    let file = read_clip(clip(&parts)).unwrap();
    assert_eq!(file.end(), Tick(5 * u64::from(TPQ)));
    let again = read_clip(encoded_clip(&file)).unwrap();
    assert_eq!(again, file);
}

/// The lossless round trip: every packet, the Flex messages, a packet no
/// definition covers, and a gap past the 20-bit DCS field (restarted by NOOPs,
/// M2-116 section 3.2.2) all come back.
#[test]
fn a_gap_past_the_dcs_field_round_trips() {
    let gap: u32 = 3 * 0x000F_FFFF + 17; // three restarts and a remainder
    let parts = [
        dcs(0),
        dctpq(480),
        dcs(0),
        start(),
        dcs(0),
        set_tempo(120.0),
        dcs(0),
        on(60, 1),
        // A DCS is 20 bits: this gap is NOOP restarts, then a count.
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(17),
        off(60),
        dcs(0),
        end(),
    ];
    let file = read_clip(clip(&parts)).unwrap();
    assert_eq!(file.sequence().events()[2].0, Tick(u64::from(gap)));
    let written = encoded_clip(&file);
    assert_eq!(read_clip(&written).unwrap(), file);
}

#[test]
fn a_packet_no_definition_covers_survives() {
    // Message type 0x6 is reserved: a 32-bit packet nothing decodes.
    let reserved = Ump([0x6001_2345, 0, 0, 0]);
    let mut parts = valid();
    parts.insert(6, reserved);
    parts.insert(6, dcs(0));
    let file = read_clip(clip(&parts)).unwrap();
    let kept = file.sequence().events().iter().any(|(_, e)| *e == reserved);
    assert!(kept, "{:?}", file.sequence().events());
    assert_eq!(read_clip(encoded_clip(&file)).unwrap(), file);
}

/// **The tempo map is the Set Tempo messages** (Flex Data, M2-116 section
/// 7.1.1): 120 BPM then 60 from beat 4, beat to seconds exact, and a clip with
/// none plays at the default 120.
/// Spec: m2-116/6.1.1#4
#[test]
fn the_tempo_map_is_the_set_tempo_messages() {
    let q = u32::from(TPQ);
    let file = read_clip(clip(&[
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        set_tempo(120.0),
        dcs(4 * q),
        set_tempo(60.0),
        dcs(4 * q),
        end(),
    ]))
    .unwrap();
    let close = |a: Seconds, b: f64| assert!((a.get() - b).abs() < 1e-9, "{a:?} vs {b}");
    close(file.seconds_at(Tick(4 * u64::from(q))), 2.0);
    close(file.seconds_at(Tick(8 * u64::from(q))), 6.0);
    assert_eq!(file.tempo_map().tempo_at(Beat::new(5.0)), Bpm::of(60.0));
    // The messages are events, kept.
    assert_eq!(file.sequence().events().len(), 2);

    let bare = read_clip(clip(&valid())).unwrap();
    assert_eq!(bare.tempo_map().constant_tempo(), Some(Bpm::of(120.0)));
}

/// **The meter map is the Set Time Signature messages** (Flex Data, M2-104
/// section 7.5.4; M2-116 section 6.1.2 lets the sequence hold several): 3/4 from
/// the start, then 6/8 at beat 3, the downbeat of bar 2. Bars of 3 quarters
/// are the same length in both signatures, so bar 2 starts at beat 3.
/// Spec: m2-116/6.1.2#4
#[test]
fn the_meter_map_is_the_set_time_signature_messages() {
    let q = u32::from(TPQ);
    let file = read_clip(clip(&[
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        signature(3, 2),
        dcs(3 * q),
        signature(6, 3),
        dcs(3 * q),
        end(),
    ]))
    .unwrap();
    let view = &file;
    let map = view.meter_map();
    assert_eq!(map.at(Beat::new(0.0)), TimeSignature::of(3, 4));
    assert_eq!(map.at(Beat::new(2.9)), TimeSignature::of(3, 4));
    assert_eq!(map.at(Beat::new(3.0)), TimeSignature::of(6, 8));
    assert_eq!(map.bar_start(BarNumber::new(2)), Beat::new(3.0));
    assert!(view.refused_time_signatures().is_empty());

    let bare = read_clip(clip(&valid())).unwrap();
    assert_eq!(bare.meter_map().at(Beat::new(0.0)), TimeSignature::of(4, 4));
}

/// **A header Set Time Signature is the meter from beat 0** (M2-116 section
/// 6.1.2), as the header's Set Tempo is the tempo from tick 0; one the sequence
/// states at tick 0 replaces it.
/// Spec: m2-116/6.1.2#1
#[test]
fn a_header_set_time_signature_is_the_meter_from_beat_zero() {
    let header = [dcs(0), set_tempo(100.0), dcs(0), time_signature()];
    let held = read_clip(clip(&with_header(&header))).unwrap();
    assert_eq!(held.meter_map().at(Beat::new(0.0)), TimeSignature::of(3, 4));

    let mut parts = vec![dcs(0), dctpq(TPQ)];
    parts.extend(header);
    parts.extend([dcs(0), start(), dcs(0), signature(6, 3), dcs(0), end()]);
    let replaced = read_clip(clip(&parts)).unwrap();
    let map = replaced.meter_map();
    assert_eq!(map.changes().len(), 1);
    assert_eq!(map.at(Beat::new(0.0)), TimeSignature::of(6, 8));
}

/// A Flex denominator field of 0 is non-standard (M2-104 section 7.5.4): the
/// message makes no meter change and is listed as the wire states it.
#[test]
fn a_flex_time_signature_denominator_of_zero_is_refused() {
    let file = read_clip(clip(&[
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        signature(3, 0),
        dcs(0),
        end(),
    ]))
    .unwrap();
    let view = &file;
    assert_eq!(view.meter_map().at(Beat::new(0.0)), TimeSignature::of(4, 4));
    let refused = view.refused_time_signatures();
    assert_eq!(refused.len(), 1);
    assert_eq!(
        (
            refused[0].beat,
            refused[0].numerator,
            refused[0].denominator_power
        ),
        (Beat::new(0.0), 3, 0)
    );
}

// ---- structure (M2-116 sections 6 and 7): one error variant per rule ------

/// Spec: m2-116/3.2.1#1, m2-116/6#5
#[test]
fn a_dctpq_after_a_message_is_refused() {
    // Start of Clip, then the DCTPQ: the DCTPQ is not first.
    let parts = [dcs(0), start(), dcs(0), dctpq(TPQ), dcs(0), end()];
    assert_eq!(clip_error(&parts), (0, ClipFileError::DctpqNotFirst));
}

/// Spec: m2-116/3.2.1#1
#[test]
fn a_second_dctpq_is_refused() {
    let mut parts = valid();
    parts.splice(2..2, [dcs(0), dctpq(TPQ)]);
    // The second DCTPQ, behind its Delta Clockstamp.
    assert_eq!(clip_error(&parts), (2, ClipFileError::DuplicateDctpq));
}

/// Spec: m2-116/7#3
#[test]
fn a_second_start_of_clip_is_refused() {
    let mut parts = valid();
    parts.splice(4..4, [dcs(0), start()]);
    assert_eq!(clip_error(&parts), (4, ClipFileError::DuplicateStartOfClip));
}

/// Spec: m2-116/7.3#2
#[test]
fn data_after_end_of_clip_is_refused() {
    let mut parts = valid();
    parts.extend([dcs(0), on(61, 1)]);
    // The Delta Clockstamp after the ten packets of the clip.
    assert_eq!(clip_error(&parts), (10, ClipFileError::TrailingData));
}

/// Spec: m2-116/3.2.1#1, m2-116/6#5
#[test]
fn a_clip_without_a_dctpq_is_refused() {
    let parts = [dcs(0), start(), dcs(0), end()];
    assert_eq!(clip_error(&parts), (0, ClipFileError::MissingDctpq));
}

/// The ticks per quarter note run from 1 to 65535, and 0 is reserved
/// (M2-104 section 7.2.3.1).
/// Spec: m2-104/7.2.3.1#1
#[test]
fn a_dctpq_is_1_to_65535_and_zero_is_refused() {
    let mut parts = valid();
    parts[1] = dctpq(0);
    assert_eq!(clip_error(&parts), (0, ClipFileError::ZeroDctpq));
    for ticks in [1, u16::MAX] {
        parts[1] = dctpq(ticks);
        let file = read_clip(clip(&parts)).unwrap();
        assert_eq!(file.ticks_per_quarter().get(), ticks);
    }
}

/// M2-104 section 7.2.3.1: an application may use a wall clock time base, a
/// Set Tempo of 60 beats per minute and a DCTPQ equal to the ticks a second.
/// With 1000 ticks per quarter note at 60 bpm a tick is a millisecond, so a note
/// at tick 2500 sounds at 2.5 seconds.
/// Spec: m2-104/7.2.3.1#2
#[test]
fn a_wall_clock_clip_has_a_millisecond_tick() {
    let parts = [
        dcs(0),
        dctpq(1000),
        dcs(0),
        start(),
        dcs(0),
        set_tempo(60.0),
        dcs(2500),
        on(60, 0x8000),
        dcs(500),
        off(60),
        dcs(0),
        end(),
    ];
    let file = read_clip(clip(&parts)).unwrap();
    let events = &file.sequence().events();
    let note = events
        .iter()
        .find(|(_, e)| matches!(e.decode(), Ok(Message::Midi2(..))))
        .unwrap();
    assert_eq!(note.0, Tick(2500));
    let seconds = file.seconds_at(note.0).get();
    assert!((seconds - 2.5).abs() < 1e-9, "{seconds}");
}

/// Spec: m2-116/7#3, m2-116/7.1#1
#[test]
fn a_clip_without_start_of_clip_is_refused() {
    let parts = [dcs(0), dctpq(TPQ), dcs(0), end()];
    assert_eq!(clip_error(&parts), (2, ClipFileError::MissingStartOfClip));
}

/// Spec: m2-116/7#4, m2-116/7.3#1
#[test]
fn a_clip_without_end_of_clip_is_refused() {
    let mut parts = valid();
    parts.truncate(parts.len() - 2);
    // Where the eight packets end.
    assert_eq!(clip_error(&parts), (8, ClipFileError::MissingEndOfClip));
}

/// Timing and padding are not structure: a Delta Clockstamp, a Null and JR
/// messages may come before the DCTPQ and Start of Clip.
#[test]
fn clockstamps_and_jr_messages_do_not_count_as_content() {
    let mut parts = vec![
        utility(Utility::Noop),
        utility(Utility::JrClock(JrTicks::new(7))),
        dcs(5),
    ];
    parts.extend(valid());
    assert!(read_clip(clip(&parts)).is_ok());
}

/// Spec: m2-116/5#1
#[test]
fn the_magic_alignment_and_a_cut_packet_are_refused() {
    let good = clip(&valid());
    assert!(matches!(
        MidiFile::from_bytes(b"SMF2CLI"),
        Err(ReadError::UnknownFormat)
    ));
    // A Clip File's own reader names the magic it did not find.
    assert_eq!(
        ClipFile::from_bytes(b"SMF2CLI"),
        Err(ClipReadError::Parse {
            word: 0,
            cause: ClipFileError::BadMagic,
        })
    );
    assert_eq!(
        ClipFile::from_bytes(b"MThd\0\0\0\x06\0\0\0\x01\0\x60"),
        Err(ClipReadError::Parse {
            word: 0,
            cause: ClipFileError::BadMagic,
        })
    );
    let mut odd = good.clone();
    odd.push(0);
    assert!(matches!(
        read_clip(&odd),
        // The word the stray byte starts.
        Err(ClipReadError::Parse {
            word,
            cause: ClipFileError::Unaligned,
        }) if word == (good.len() - 8) / 4
    ));
    // End of Clip is a 128-bit message: cut it to one word.
    let cut = &good[..good.len() - 12];
    assert!(matches!(
        read_clip(cut),
        // The one word of the cut packet is past the whole packets.
        Err(ClipReadError::Parse {
            word,
            cause: ClipFileError::MessageCutOff,
        }) if word == (cut.len() - 8) / 4 - 1
    ));
}

/// **No strict prefix of a valid clip is itself a clip, and none panics.**
#[test]
fn no_prefix_of_a_valid_clip_is_accepted() {
    let bytes = clip(&valid());
    assert!(read_clip(&bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(
            read_clip(&bytes[..n]).is_err(),
            "the first {n} of {} bytes were accepted",
            bytes.len()
        );
    }
}

// ---- building a clip ------------------------------------------------------

#[test]
fn what_a_clip_cannot_hold_is_refused() {
    let one = |event| ClipFile::new(DIVISION, ClipHeader::new(), sequence(vec![(0, event)]));
    // The file's own structure is not an event.
    for structure in [
        Message::Stream(Stream::StartOfClip),
        Message::Stream(Stream::EndOfClip),
        Message::Utility(Utility::Noop),
        Message::Utility(Utility::TicksPerQuarterNote(TicksPerQuarterNote::of(1))),
    ] {
        assert!(matches!(
            one(structure.into()),
            Err(ClipBuildError::Refused {
                part: ClipPart::Sequence,
                ..
            })
        ));
    }
}

/// M2-116 section 3.2.2: a Delta Clockstamp holds 20 bits and a Null message
/// restarts the count, so a gap of any length is written and read back. Past
/// a `u32` it is 4096 restarts.
#[test]
fn a_gap_past_a_u32_round_trips() {
    let at = u64::from(u32::MAX) + 1;
    let file = ClipFile::new(
        DIVISION,
        ClipHeader::new(),
        ClipSequence::new()
            .push(Tick(at), on(60, 1))
            .unwrap()
            .ending_at(Tick(at + 7)),
    )
    .unwrap();
    let bytes = encoded_clip(&file);
    // 4096 restarts of two words each, and the Delta Clockstamps around them.
    assert!(bytes.len() > 4096 * 8);
    assert_eq!(read_clip(&bytes).unwrap(), file);
}

/// A gap whose Null restarts would not fit 4 GB is refused when the clip is
/// built, not written into memory.
#[test]
fn a_gap_too_long_to_write_is_too_large() {
    assert_eq!(
        ClipFile::new(
            DIVISION,
            ClipHeader::new(),
            ClipSequence::new().ending_at(Tick(u64::MAX))
        ),
        Err(ClipBuildError::TooLarge)
    );
}

// ---- notes ----------------------------------------------------------------

fn notes_of(events: Vec<(u64, Ump)>) -> Vec<tutti_midi_file::PairedNote> {
    ClipFile::new(DIVISION, ClipHeader::new(), sequence(events))
        .unwrap()
        .sequence()
        .notes()
}

#[test]
fn a_note_off_only_closes_its_own_group() {
    let g1 = MidiGroup::new(1).unwrap();
    let notes = notes_of(vec![
        (0, on(60, 100)),
        (
            10,
            midi::note_off(Address::new(g1, MidiChannel::FIRST), 60, 0),
        ),
        (20, off(60)),
    ]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(20));
}

/// A MIDI 2.0 Note On at velocity 0 is a note (M2-104 section 7.4.2), and
/// neither starts nor ends one here.
#[test]
fn a_midi2_note_on_at_velocity_zero_is_neither_start_nor_end() {
    let notes = notes_of(vec![(0, on(60, 100)), (10, on(60, 0)), (20, off(60))]);
    assert_eq!(notes.len(), 1);
    assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(20)));
}

/// An SMF and a Clip File holding the same MIDI 1.0 messages pair the same
/// notes: the file layer keeps the vocabulary one.
#[test]
fn an_smf_and_a_clip_of_the_same_messages_pair_the_same_notes() {
    let message = |on: bool| {
        let (note, velocity) = (NoteNumber::of(64), U7::of(90));
        Message::Midi1(
            Address::first(MidiChannel::FIRST),
            if on {
                Voice1::NoteOn { note, velocity }
            } else {
                Voice1::NoteOff { note, velocity }
            },
        )
    };
    let smf = crate::common::format0(
        SMF_DIVISION,
        common::track(vec![(0, message(true).into()), (96, message(false).into())]),
    )
    .unwrap();
    let clip = ClipFile::new(
        DIVISION,
        ClipHeader::new(),
        sequence(vec![(0, message(true).into()), (96, message(false).into())]),
    )
    .unwrap();
    let a = read_bytes(encoded(&smf)).unwrap();
    let b = read_clip(encoded_clip(&clip)).unwrap();
    assert_eq!(a.tracks()[0].notes(), b.sequence().notes());
    // The file's Set Tempo and Time Signature come first.
    assert_eq!(
        a.tracks()[0].events()[2].1,
        FileEvent::Message(b.sequence().events()[0].1.decode().unwrap())
    );
}

// ---- the configuration header (M2-116 sections 6 to 6.1.2) ----------------

fn sysex7(status: tutti_ump::SysexStatus, data: &[u8]) -> Ump {
    Message::Sysex7(
        MidiGroup::FIRST,
        tutti_ump::Sysex7Packet::new(status, data).unwrap(),
    )
    .into()
}

/// A Set Profile On (M2-101 section 7.8) as the packets it is cut into: Universal
/// SysEx `7E <device> 0D 22 <version> <source> <destination> <Profile Id>
/// <channels>`, both MUIDs Broadcast (M2-116 section 6.2). The channels bytes
/// are left out of a Version 1 message.
fn profile_payload(device: u8, version: u8, profile: [u8; 5], channels: &[u8]) -> Vec<Ump> {
    let mut payload = vec![0x7E, device, 0x0D, 0x22, version];
    payload.extend([0x7F; 8]);
    payload.extend(profile);
    payload.extend(channels);
    tutti_ump::Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
        .unwrap()
        .collect()
}

fn set_profile_on_of(id: [u8; 5]) -> Vec<Ump> {
    profile_payload(0x7E, 2, id, &[0, 0])
}

/// The General MIDI 2 Function Block Profile (M2-118 section 3.4).
const GM2: [u8; 5] = [0x7E, 0x00, 0x00, 0x01, 0x01];
/// The MPE Profile (M2-120 section 3.2).
const MPE: [u8; 5] = [0x7E, 0x31, 0x00, 0x01, 0x01];

fn set_profile_on() -> Vec<Ump> {
    set_profile_on_of(GM2)
}

/// `parts` one after another.
fn then(parts: &[&[Ump]]) -> Vec<Ump> {
    parts.concat()
}

fn program_change() -> Ump {
    Message::Midi1(
        Address::first(MidiChannel::FIRST),
        Voice1::ProgramChange(U7::of(5)),
    )
    .into()
}

/// A Flex Data Set Time Signature (M2-104 section 7.5.4): the denominator
/// field is the power of two, 2 for a quarter and 3 for an eighth.
fn signature(numerator: u8, denominator: u8) -> Ump {
    Message::Flex(
        MidiGroup::FIRST,
        Flex::SetTimeSignature(tutti_ump::FlexTimeSignature {
            numerator,
            denominator,
            thirty_seconds: 8,
        }),
    )
    .into()
}

/// 3/4 as a Flex Data Set Time Signature.
fn time_signature() -> Ump {
    signature(3, 2)
}

/// A clip whose header after the DCTPQ is `header` (each behind a DCS of 0
/// unless it is a `dcs`), then Start of Clip, a note and End of Clip.
fn with_header(header: &[Ump]) -> Vec<Ump> {
    let mut parts = vec![dcs(0), dctpq(TPQ)];
    parts.extend_from_slice(header);
    parts.extend([
        dcs(0),
        start(),
        dcs(0),
        on(60, 1),
        dcs(10),
        off(60),
        dcs(0),
        end(),
    ]);
    parts
}

/// Section 6: Set Profile On messages come first, carry no DCS, and the DCTPQ
/// follows them. Kept in the header, written back before the DCTPQ.
/// Spec: m2-116/hdr/1, m2-116/hdr/2, m2-116/hdr/3, m2-116/hdr/4, m2-116/hdr/5, m2-116/hdr/6, m2-116/hdr/7, m2-116/hdr/8, m2-116/5#2, m2-116/5#3, m2-116/6#1, m2-116/6#3, m2-116/6#4, m2-116/6#5, m2-116/6#6, m2-116/3.2.1#1, m2-116/3.2.1#2, m2-116/6.2#1
#[test]
fn set_profile_on_before_the_dctpq_is_accepted() {
    let messages = then(&[&set_profile_on(), &set_profile_on_of(MPE)]);
    let mut parts = messages.clone();
    parts.extend(with_header(&[]));
    let file = read_clip(clip(&parts)).unwrap();
    let header = file.header().messages();
    assert_eq!(header.len(), messages.len());
    assert!(header.iter().all(|(t, _)| *t == Tick(0)));
    // The write puts them before the DCTPQ again, with no DCS between.
    let written = encoded_clip(&file);
    let mut expected = MAGIC.to_vec();
    for ump in then(&[&messages, &[dcs(0), dctpq(TPQ)]]) {
        for w in ump.words() {
            expected.extend_from_slice(&w.to_be_bytes());
        }
    }
    assert!(written.starts_with(&expected));
    assert_eq!(read_clip(&written).unwrap(), file);
}

/// Anything else before the DCTPQ is not allowed: a note, or a SysEx that is
/// not a Set Profile On.
/// Spec: m2-116/3.2.1#1, m2-116/6#5
#[test]
fn a_message_before_the_dctpq_is_refused() {
    let mut note_first = vec![dcs(0), on(60, 1)];
    note_first.extend(with_header(&[]));
    assert_eq!(clip_error(&note_first), (0, ClipFileError::DctpqNotFirst));
    let other_sysex = sysex7(tutti_ump::SysexStatus::Complete, &[0x7E, 0x7F, 0x06, 0x01]);
    let mut parts = vec![other_sysex];
    parts.extend(with_header(&[]));
    assert_eq!(clip_error(&parts), (0, ClipFileError::DctpqNotFirst));
}

/// Section 6: the header may hold other messages that configure a receiver,
/// each behind its own DCS. They are in the model, in order with their delta
/// times, are not events of the sequence, and write back.
/// Spec: m2-116/6#7, m2-116/6.1#1, m2-116/6.3#1, m2-116/2#1
#[test]
fn a_header_with_program_change_cc_and_sysex_round_trips() {
    let control = Ump::from(Message::Midi2(
        Address::first(MidiChannel::FIRST),
        tutti_ump::Voice2::ControlChange {
            controller: tutti_ump::CCNumber::of(7),
            value: ControlValue::CENTER,
        },
    ));
    let sysex = sysex7(tutti_ump::SysexStatus::Complete, &[0x7D, 1, 2, 3]);
    let file = read_clip(clip(&with_header(&[
        dcs(0),
        program_change(),
        dcs(10),
        control,
        dcs(5),
        sysex,
    ])))
    .unwrap();
    let ticks: Vec<u64> = file.header().messages().iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 10, 15]);
    assert_eq!(file.header().messages()[2].1, sysex);
    // Not on the sequence's timeline, not sounding at bar 1.
    assert_eq!(file.sequence().events().len(), 2);
    assert_eq!(file.timeline().packets.len(), 2);
    assert_eq!(read_clip(encoded_clip(&file)).unwrap(), file);
}

/// Spec: m2-116/6.1#3, m2-116/6.1.1#3
#[test]
fn two_header_set_tempos_are_refused() {
    let parts = with_header(&[dcs(0), set_tempo(100.0), dcs(0), set_tempo(90.0)]);
    assert_eq!(clip_error(&parts), (4, ClipFileError::DuplicateHeaderTempo));
}

/// Section 6.1.1: the Set Tempo is the first message after the DCTPQ,
/// at a delta of 0 (or sharing the DCTPQ's DCS, which is also 0).
/// Spec: m2-116/6.1.1#1, m2-116/6.1.1#2
#[test]
fn a_header_set_tempo_not_first_or_not_at_delta_zero_is_misplaced() {
    let after_other = with_header(&[dcs(0), program_change(), dcs(0), set_tempo(100.0)]);
    assert_eq!(
        clip_error(&after_other),
        (4, ClipFileError::MisplacedHeaderTempo)
    );
    let late = with_header(&[dcs(7), set_tempo(100.0)]);
    assert_eq!(clip_error(&late), (2, ClipFileError::MisplacedHeaderTempo));
    // Sharing the DCTPQ's DCS (no DCS of its own) is first at delta 0.
    let sharing = with_header(&[set_tempo(100.0)]);
    assert!(read_clip(clip(&sharing)).is_ok());
}

/// Spec: m2-116/6.1#3, m2-116/6.1.2#3
#[test]
fn two_header_time_signatures_are_refused() {
    let parts = with_header(&[dcs(0), time_signature(), dcs(0), time_signature()]);
    assert_eq!(
        clip_error(&parts),
        (4, ClipFileError::DuplicateHeaderTimeSignature)
    );
}

/// Section 6.1.2: the Set Time Signature follows the Set Tempo immediately (or
/// is first when there is no Set Tempo).
/// Spec: m2-116/6.1#2, m2-116/6.1.2#1
#[test]
fn a_header_time_signature_must_follow_the_set_tempo_immediately() {
    let behind_other = with_header(&[
        dcs(0),
        set_tempo(100.0),
        dcs(0),
        program_change(),
        dcs(0),
        time_signature(),
    ]);
    assert_eq!(
        clip_error(&behind_other),
        (6, ClipFileError::MisplacedHeaderTimeSignature)
    );
    let before_tempo = with_header(&[dcs(0), time_signature(), dcs(0), set_tempo(100.0)]);
    // The tempo is then not first.
    assert_eq!(
        clip_error(&before_tempo),
        (4, ClipFileError::MisplacedHeaderTempo)
    );
    let after_other_no_tempo = with_header(&[dcs(0), program_change(), dcs(0), time_signature()]);
    assert_eq!(
        clip_error(&after_other_no_tempo),
        (4, ClipFileError::MisplacedHeaderTimeSignature)
    );
    for ok in [
        with_header(&[dcs(0), set_tempo(100.0), dcs(0), time_signature()]),
        with_header(&[dcs(0), time_signature()]),
    ] {
        assert!(read_clip(clip(&ok)).is_ok());
    }
}

/// **The header's Set Tempo is the tempo from the start of the sequence**: the
/// header's clock runs before Start of Clip, so its tempo is at sequence tick
/// 0, and a Set Tempo in the sequence is an ordinary change after it.
/// Spec: m2-116/6.1.1#1, m2-116/6.1.1#4
#[test]
fn the_header_set_tempo_is_the_tempo_from_the_start_of_the_sequence() {
    let q = u32::from(TPQ);
    let mut parts = vec![
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        set_tempo(100.0),
        dcs(0),
        start(),
    ];
    parts.extend([dcs(q), set_tempo(50.0), dcs(q), on(60, 1), dcs(0), end()]);
    let file = read_clip(clip(&parts)).unwrap();
    let map = file.tempo_map();
    assert_eq!(map.tempo_at(Beat::new(0.5)), Bpm::of(100.0));
    assert_eq!(map.tempo_at(Beat::new(1.5)), Bpm::of(50.0));
    // One beat at 100 BPM (0.6 s) and one at 50 (1.2 s).
    let close = |a: Seconds, b: f64| assert!((a.get() - b).abs() < 1e-9, "{a:?} vs {b}");
    close(file.seconds_at(Tick(2 * u64::from(q))), 1.8);
    assert_eq!(file.header().messages().len(), 1);
}

/// Section 3.2.2: a gap past 1,048,575 ticks is written as a DCS of 1,048,575
/// followed by a Null message, once per restart, then the remainder's DCS.
/// Spec: m2-116/3.2.2#4, m2-104/7.2.3.2#3
#[test]
fn a_long_gap_is_written_as_dcs_then_a_null() {
    let gap: u32 = 2 * 0x000F_FFFF + 3;
    let sequence = sequence(vec![(0, on(60, 1)), (u64::from(gap), off(60))]);
    let bytes = encoded_clip(&ClipFile::new(DIVISION, ClipHeader::new(), sequence).unwrap());
    let words: Vec<u32> = bytes[8..]
        .chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    let (max, noop) = (dcs(0x000F_FFFF).words()[0], 0u32);
    let expect = [max, noop, max, noop, dcs(3).words()[0]];
    assert!(
        words.windows(5).any(|w| w == expect),
        "no DCS, Null, DCS, Null, DCS run in {words:08x?}"
    );
}

// ---- M2-116 conformance of the reader and the writer -----------

/// The packets of a written clip after the file header, in order.
fn packets_of(bytes: &[u8]) -> Vec<Ump> {
    let words: Vec<u32> = bytes[8..]
        .chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    tutti_ump::split_stream(&words).collect()
}

fn is_dcs(ump: Ump) -> bool {
    matches!(
        ump.decode(),
        Ok(Message::Utility(Utility::DeltaClockstamp(_)))
    )
}

/// A MIDI-CI message `7E 7F 0D <sub_id2> ...` as one System Exclusive packet.
fn ci(sub_id2: u8, status: tutti_ump::SysexStatus) -> Ump {
    sysex7(status, &[0x7E, 0x7F, 0x0D, sub_id2, 0, 0])
}

/// Section 3.2.1 and section 6: the DCTPQ has a preceding Delta
/// Clockstamp (DCS) with its tick count set to 0.
/// Spec: m2-116/3.2.1#2, m2-116/6#6
#[test]
fn the_dctpq_has_a_zero_delta_clockstamp_before_it() {
    let tail = [dcs(0), start(), dcs(0), end()];
    let with = |before: &[Ump]| -> Vec<Ump> {
        let mut parts = before.to_vec();
        parts.push(dctpq(TPQ));
        parts.extend_from_slice(&tail);
        parts
    };
    assert!(read_clip(clip(&with(&[dcs(0)]))).is_ok());
    assert_eq!(clip_error(&with(&[])), (0, ClipFileError::DctpqDelta));
    assert_eq!(clip_error(&with(&[dcs(5)])), (0, ClipFileError::DctpqDelta));
    // It follows the Set Profile On messages, and its DCS is the one after them.
    assert!(read_clip(clip(&with(&then(&[&set_profile_on(), &[dcs(0)]])))).is_ok());
    assert_eq!(
        clip_error(&with(&then(&[&set_profile_on(), &[dcs(2)]]))),
        (set_profile_on().len(), ClipFileError::DctpqDelta)
    );
}

/// Section 6: a Set Profile On message has no Delta
/// Clockstamp (DCS) before it.
/// Spec: m2-116/6#4
#[test]
fn a_set_profile_on_has_no_delta_clockstamp_before_it() {
    let after = [dcs(0), dctpq(TPQ), dcs(0), start(), dcs(0), end()];
    let with = |before: &[Ump]| -> Vec<Ump> {
        let mut parts = before.to_vec();
        parts.extend_from_slice(&after);
        parts
    };
    assert!(read_clip(clip(&with(&set_profile_on()))).is_ok());
    assert_eq!(
        clip_error(&with(&then(&[&[dcs(0)], &set_profile_on()]))),
        (1, ClipFileError::ProfileOnDelta)
    );
    // Nor before a later packet of one message.
    let parts = set_profile_on();
    let cut = then(&[&parts[..1], &[dcs(0)], &parts[1..]]);
    assert_eq!(clip_error(&with(&cut)), (0, ClipFileError::ProfileOnDelta));
}

/// Section 6.1.2: the header's Set Time Signature uses the Delta
/// Clockstamp that precedes the DCTPQ or has a preceding one of 0 ticks, as the
/// Set Tempo does (section 6.1.1).
/// Spec: m2-116/6.1.2#2
#[test]
fn the_header_time_signature_is_at_delta_zero() {
    for (header, packet) in [
        (vec![dcs(5), time_signature()], 2),
        (vec![dcs(0), set_tempo(100.0), dcs(5), time_signature()], 4),
    ] {
        assert_eq!(
            clip_error(&with_header(&header)),
            (packet, ClipFileError::MisplacedHeaderTimeSignature),
            "{header:?}"
        );
    }
}

/// Section 6.4: Property Data for PE Resources is not included in a Clip File.
/// Section 7.2: the sequence data holds any UMP message but Property
/// Exchange. Property Exchange is MIDI-CI
/// sub-ID#2 `0x30` to `0x3F` (M2-101 Table 5), so its neighbours are fine.
/// Spec: m2-116/6.4#1, m2-116/7.2#1
#[test]
fn property_exchange_is_refused_everywhere() {
    use tutti_ump::SysexStatus::{Complete, Start};
    for sub_id2 in [0x30, 0x34, 0x3F] {
        for status in [Complete, Start] {
            let pe = ci(sub_id2, status);
            // In the sequence data...
            let in_sequence = [
                dcs(0),
                dctpq(TPQ),
                dcs(0),
                start(),
                dcs(0),
                pe,
                dcs(0),
                end(),
            ];
            assert_eq!(
                clip_error(&in_sequence),
                (4, ClipFileError::PropertyExchange),
                "{sub_id2:#x}"
            );
            // ...and in the header.
            assert_eq!(
                clip_error(&with_header(&[dcs(0), pe])),
                (2, ClipFileError::PropertyExchange)
            );
            // The writer's gate refuses it too.
            assert_eq!(
                ClipFile::new(DIVISION, ClipHeader::new(), sequence(vec![(0, pe)])),
                Err(ClipBuildError::Format(ClipFileError::PropertyExchange)),
                "{sub_id2:#x}"
            );
        }
    }
    // Profile Configuration (0x2F) and Process Inquiry (0x40) are not it.
    for sub_id2 in [0x22, 0x2F, 0x40] {
        let other = ci(sub_id2, Complete);
        let parts = [
            dcs(0),
            dctpq(TPQ),
            dcs(0),
            start(),
            dcs(0),
            other,
            dcs(0),
            end(),
        ];
        assert!(read_clip(clip(&parts)).is_ok(), "{sub_id2:#x}");
    }
}

/// Section 6.1: the Delta Clockstamps up to and including the one for the
/// Start of Clip add up to the time allocated to the Clip Configuration
/// Header. The last
/// gap is kept, read and written.
#[test]
fn the_header_keeps_its_duration() {
    // A header message at 4, then 26 more ticks to Start of Clip: it ends at 30.
    let parts = [
        dcs(0),
        dctpq(TPQ),
        dcs(4),
        program_change(),
        dcs(26),
        start(),
        dcs(0),
        end(),
    ];
    let file = read_clip(clip(&parts)).unwrap();
    let header = file.header();
    assert_eq!(header.messages().len(), 1);
    assert_eq!(header.messages()[0].0, Tick(4));
    assert_eq!(header.end(), Tick(30));
    // Written back with the same Delta Clockstamp before Start of Clip.
    let bytes = encoded_clip(&file);
    let written = packets_of(&bytes);
    let at = written.iter().position(|&u| u == start()).unwrap();
    assert_eq!(written[at - 1], dcs(26));
    assert_eq!(read_clip(&bytes).unwrap(), file);
    // A header with no message has its duration too.
    let empty = [dcs(0), dctpq(TPQ), dcs(12), start(), dcs(0), end()];
    let file = read_clip(clip(&empty)).unwrap();
    assert_eq!(file.header().end(), Tick(12));
    assert_eq!(read_clip(encoded_clip(&file)).unwrap(), file);
    // None, as in `valid`, is zero.
    let file = read_clip(clip(&valid())).unwrap();
    assert_eq!(file.header().end(), Tick(0));
}

fn text_packets(kind: tutti_ump::FlexTextKind, text: &str) -> Vec<(Tick, Ump)> {
    use tutti_ump::{
        FlexAddress, FlexTextKind, LyricKind, PlainTextKind, Syllable, SyllableBody, SyllableEnd,
        Text, flex_lyric_packets, flex_text_packets,
    };
    let (group, address) = (MidiGroup::FIRST, FlexAddress::Group);
    let lyric = |kind| {
        let syllable = Syllable {
            body: SyllableBody::try_from(text).unwrap(),
            end: SyllableEnd::None,
        };
        flex_lyric_packets(group, address, kind, &syllable).collect::<Vec<_>>()
    };
    let plain = |kind| {
        let text = Text::try_from(text).unwrap();
        flex_text_packets(group, address, kind, &text).collect::<Vec<_>>()
    };
    let umps = match kind {
        FlexTextKind::Lyrics => lyric(LyricKind::Lyrics),
        FlexTextKind::Ruby => lyric(LyricKind::Ruby),
        FlexTextKind::Composer => plain(PlainTextKind::Composer),
        FlexTextKind::Arranger => plain(PlainTextKind::Arranger),
        other => panic!("{other:?}"),
    };
    umps.into_iter().map(|ump| (Tick(10), ump)).collect()
}

/// M2-104 sections 7.5.10 and 7.5.12: in a Clip File all parts of a Lyric
/// Data Message are placed at the same time, sharing one Delta Clockstamp, and
/// M2-116 section 3.2.2: a Delta Clockstamp sets the time of the messages that
/// follow it until the next one. The parts of a Lyric or Ruby message are written behind one DCS;
/// the parts of any other message, and separate messages, keep their own.
/// Spec: m2-116/3.2.2#1, m2-104/7.5.10#5, m2-104/7.5.12#5
#[test]
fn the_parts_of_a_lyric_or_ruby_message_share_one_delta_clockstamp() {
    use tutti_ump::FlexTextKind::{Composer, Lyrics, Ruby};
    // 20 bytes: a Start and an End packet (12 bytes each).
    let syllable = "abcdefghijklmnopqrst";
    for (kind, shared) in [(Lyrics, true), (Ruby, true), (Composer, false)] {
        let events = text_packets(kind, syllable);
        assert_eq!(events.len(), 2);
        let bytes = ClipFile::new(
            DIVISION,
            ClipHeader::new(),
            events.iter().copied().collect::<ClipSequence>(),
        )
        .unwrap();
        let bytes = encoded_clip(&bytes);
        let written = packets_of(&bytes);
        let first = written.iter().position(|&u| u == events[0].1).unwrap();
        assert_eq!(
            is_dcs(written[first + 1]),
            !shared,
            "{kind:?}: a DCS between the parts"
        );
        assert!(
            is_dcs(written[first - 1]),
            "{kind:?}: a DCS before the first"
        );
        // Either way it reads back as the same events at the same ticks.
        let back = read_clip(&bytes).unwrap();
        assert_eq!(back.sequence().events(), events, "{kind:?}");
    }
    // Two separate Lyric messages at one tick are two messages: each its own DCS.
    let mut two = text_packets(Lyrics, "one");
    two.extend(text_packets(Lyrics, "two"));
    let bytes = ClipFile::new(
        DIVISION,
        ClipHeader::new(),
        two.iter().copied().collect::<ClipSequence>(),
    )
    .unwrap();
    let bytes = encoded_clip(&bytes);
    let written = packets_of(&bytes);
    let first = written.iter().position(|&u| u == two[0].1).unwrap();
    assert!(is_dcs(written[first + 1]));
    assert_eq!(read_clip(&bytes).unwrap().sequence().events(), two);
}

/// A Set Profile On message is complete before the DCTPQ: a Start with no End,
/// then the DCS of the DCTPQ, is a Delta Clockstamp inside the message.
#[test]
fn an_unfinished_set_profile_on_is_refused() {
    let start_only = sysex7(
        tutti_ump::SysexStatus::Start,
        &[0x7E, 0x7F, 0x0D, 0x22, 1, 2],
    );
    let mut parts = vec![start_only];
    parts.extend(with_header(&[]));
    assert_eq!(clip_error(&parts), (0, ClipFileError::ProfileOnDelta));
}

/// Section 7.3: nothing follows End of Clip, not even half a packet.
/// Spec: m2-116/7.3#2
#[test]
fn half_a_packet_after_end_of_clip_is_trailing_data() {
    let mut bytes = clip(&valid());
    // The first word of a 64-bit Note On.
    bytes.extend_from_slice(&on(60, 1).words()[0].to_be_bytes());
    assert!(matches!(
        read_clip(&bytes),
        // The first word of the Note On, after the clip's words.
        Err(ClipReadError::Parse {
            word,
            cause: ClipFileError::TrailingData,
        }) if word == (clip(&valid()).len() - 8) / 4
    ));
}

/// Every message of a written clip but a Set Profile On has a Delta Clockstamp
/// right before it (M2-116 sections 3.2.2, 6 and 7), Start of Clip and End of
/// Clip (M2-104 sections 7.1.10 and 7.1.11) among them, and the header ends at
/// the Delta Clockstamp of Start of Clip: the header messages are before it and
/// no sequence event is.
/// Spec: m2-116/6#2, m2-116/6#8, m2-116/6#9, m2-116/7#1, m2-116/7#2, m2-104/7.1.10#1, m2-104/7.1.11#1
#[test]
fn a_written_clip_stamps_every_message_and_ends_the_header_at_start_of_clip() {
    let sysex = sysex7(tutti_ump::SysexStatus::Complete, &[0x7D, 1, 2, 3]);
    let file = read_clip(clip(&with_header(&[
        dcs(0),
        program_change(),
        dcs(10),
        sysex,
    ])))
    .unwrap();
    let packets = packets_of(&encoded_clip(&file));
    let position = |want: Ump| packets.iter().position(|p| *p == want).unwrap();
    let (s, e) = (position(start()), position(end()));
    assert!(is_dcs(packets[s - 1]), "Start of Clip has a DCS before it");
    assert!(is_dcs(packets[e - 1]), "End of Clip has a DCS before it");
    assert_eq!(e, packets.len() - 1, "End of Clip is the last message");
    for (i, p) in packets.iter().enumerate().skip(1) {
        assert!(
            is_dcs(*p) || is_dcs(packets[i - 1]),
            "message {i} of {packets:08x?} has no DCS before it"
        );
    }
    let header = &packets[..s - 1];
    assert!(header.contains(&program_change()) && header.contains(&sysex));
    assert!(header.iter().all(|p| p.note_key_event().is_none()));
    assert!(packets[s..].iter().any(|p| p.note_key_event().is_some()));
}

/// Events that follow one Delta Clockstamp are at one tick and keep the order
/// they were stored in, which the file written back keeps too.
/// Spec: m2-116/3.2.2#2, m2-116/3.2.2#3, m2-104/7.2.3.2#1, m2-104/7.2.3.2#2
#[test]
fn simultaneous_events_share_a_delta_clockstamp_in_presentation_order() {
    let at_zero = [on(60, 1), on(64, 2), off(60), on(67, 3)];
    let mut parts = vec![dcs(0), dctpq(TPQ), dcs(0), start(), dcs(0)];
    parts.extend(at_zero);
    parts.extend([dcs(u32::from(TPQ)), off(64), dcs(0), end()]);
    let file = read_clip(clip(&parts)).unwrap();
    let events = &file.sequence().events();
    assert_eq!(events.len(), 5);
    let ticks: Vec<Tick> = events.iter().map(|e| e.0).collect();
    assert_eq!(ticks, [Tick(0), Tick(0), Tick(0), Tick(0), Tick(96)]);
    for (event, want) in events.iter().zip(at_zero) {
        assert_eq!(event.1, want);
    }
    let back = read_clip(encoded_clip(&file)).unwrap();
    assert_eq!(back.sequence().events(), *events);
}

/// M2-104 section 7.5.2: within a Group, between the Start and the End of a
/// Flex Data message a sender puts only Continue packets of that message and
/// System Real Time messages, so a clip with anything else of that Group
/// there, a Continue or End with no Start, or a message with no End, is
/// refused when it is built, at the tick of the offending message; a message
/// of another Group, and a System Real Time message of any, is no
/// interruption.
/// Spec: m2-104/7.5.2#2, m2-104/7.5.2#3
#[test]
fn a_flex_message_is_not_interrupted_in_a_clip() {
    use tutti_ump::FlexTextKind::{Arranger, Composer};
    use tutti_ump::System;
    // 25 bytes: a Start, a Continue and an End, at ticks 10, 20 and 30.
    let text = "0123456789ab0123456789abX";
    let at = |tick: u64, (_, event): (Tick, Ump)| (Tick(tick), event);
    let parts = |kind| -> Vec<(Tick, Ump)> {
        text_packets(kind, text)
            .into_iter()
            .zip([10, 20, 30])
            .map(|(e, tick)| at(tick, e))
            .collect()
    };
    let build = |events: Vec<(Tick, Ump)>| {
        ClipFile::new(
            DIVISION,
            ClipHeader::new(),
            events
                .into_iter()
                .collect::<ClipSequence>()
                .ending_at(Tick(100)),
        )
    };
    let interrupted = |events| match build(events) {
        Err(ClipBuildError::TextInterrupted {
            part: ClipPart::Sequence,
            at,
        }) => Some(at),
        Ok(_) => None,
        Err(other) => panic!("{other:?}"),
    };
    let composer = parts(Composer);
    let note = (Tick(15), on(60, 1));
    let real_time = |system, tick| {
        (
            Tick(tick),
            Ump::from(Message::System(MidiGroup::FIRST, system)),
        )
    };
    let with = |extra: (Tick, Ump)| {
        let mut events = composer.clone();
        events.insert(1, extra);
        events
    };

    assert_eq!(interrupted(composer.clone()), None);
    // A note of the Group, between Start and Continue or Continue and End.
    assert_eq!(interrupted(with(note)), Some(Tick(15)));
    let mut between_continue_and_end = composer.clone();
    between_continue_and_end.insert(2, (Tick(25), note.1));
    assert_eq!(interrupted(between_continue_and_end), Some(Tick(25)));
    // Every System Real Time message is allowed, of any Group.
    for system in [
        System::TimingClock,
        System::Start,
        System::Continue,
        System::Stop,
        System::ActiveSensing,
        System::Reset,
    ] {
        assert_eq!(interrupted(with(real_time(system, 15))), None, "{system:?}");
    }
    let mut other_group_clock = real_time(System::TimingClock, 15);
    other_group_clock.1 = Message::System(MidiGroup::of(9), System::TimingClock).into();
    assert_eq!(interrupted(with(other_group_clock)), None);
    // A System Common message is not real time.
    assert_eq!(
        interrupted(with(real_time(System::TuneRequest, 15))),
        Some(Tick(15))
    );
    // A note of another Group, and a message of another Group, are not in
    // the way.
    let mut other_note = note;
    other_note.1 = Message::Midi1(
        Address {
            group: MidiGroup::of(4),
            channel: MidiChannel::of(0),
        },
        tutti_ump::Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(1),
        },
    )
    .into();
    assert_eq!(interrupted(with(other_note)), None);
    let mut elsewhere = with(note);
    elsewhere[1].1 = Message::Flex(
        MidiGroup::of(1),
        Flex::Text {
            address: tutti_ump::FlexAddress::Group,
            kind: Arranger,
            form: tutti_ump::Form::Complete,
            bytes: *b"other group!",
        },
    )
    .into();
    assert_eq!(interrupted(elsewhere), None);
    // A Complete message of the Group, or a Start of another text, interrupts.
    let mut complete = with((Tick(15), composer[2].1));
    complete[1].1 = Message::Flex(
        MidiGroup::FIRST,
        Flex::Text {
            address: tutti_ump::FlexAddress::Group,
            kind: Arranger,
            form: tutti_ump::Form::Complete,
            bytes: *b"interrupting",
        },
    )
    .into();
    assert_eq!(interrupted(complete), Some(Tick(15)));
    // A message that never ends is reported at the last event; a Continue or
    // End with no Start, or of another text, at its own tick.
    assert_eq!(interrupted(composer[..2].to_vec()), Some(Tick(20)));
    assert_eq!(interrupted(composer[1..].to_vec()), Some(Tick(20)));
    assert_eq!(interrupted(composer[2..].to_vec()), Some(Tick(30)));
    let mut other_kind = composer.clone();
    other_kind[1] = at(20, parts(Arranger).remove(1));
    assert_eq!(interrupted(other_kind), Some(Tick(20)));
}

/// The header of a clip made of `leading`, the DCTPQ and nothing else, read.
fn read_with_leading(leading: &[Ump]) -> Result<ClipFile, ClipReadError> {
    let mut parts = leading.to_vec();
    parts.extend(with_header(&[]));
    read_clip(clip(&parts))
}

/// Section 6.2: the header's Set Profile On names the Profile the sequence
/// data is meant for; a file read gives it typed, in order, with the address it
/// is for.
/// Spec: m2-116/6.2#2
#[test]
fn the_headers_profile_is_read_typed() {
    let file = read_with_leading(&then(&[
        &set_profile_on(),
        &profile_payload(0x02, 2, MPE, &[0x0F, 0x00]),
        &profile_payload(0x7F, 1, GM2, &[]),
    ]))
    .unwrap();
    assert_eq!(
        file.header().profiles(),
        vec![
            ProfileOnRequest {
                target: ProfileTarget::Group,
                profile: ProfileId::GM2_FUNCTION_BLOCK
            },
            ProfileOnRequest {
                target: ProfileTarget::Channel {
                    channel: MidiChannel::of(2),
                    channels: ProfileChannels::of(15)
                },
                profile: ProfileId::MPE
            },
            ProfileOnRequest {
                target: ProfileTarget::FunctionBlock,
                profile: ProfileId::GM2_FUNCTION_BLOCK
            },
        ]
    );
    // The write keeps them.
    let back = read_clip(encoded_clip(&file)).unwrap();
    assert_eq!(back.header().profiles(), file.header().profiles());
}

/// A header whose Set Profile On is Message Format Version 1, which has no
/// Number of Channels field, loads and asks for 0 Channels.
/// Spec: m2-116/6.2#2
#[test]
fn a_version_1_set_profile_on_loads_with_no_channels() {
    let file = read_with_leading(&profile_payload(0x03, 1, MPE, &[])).unwrap();
    assert_eq!(
        file.header().profiles(),
        vec![ProfileOnRequest {
            target: ProfileTarget::Channel {
                channel: MidiChannel::of(3),
                channels: ProfileChannels::of(0)
            },
            profile: ProfileId::MPE
        }]
    );
}

/// A header with no Set Profile On has no Profile.
/// Spec: m2-116/6.2#2
#[test]
fn a_header_without_a_set_profile_on_has_no_profile() {
    let file = read_with_leading(&[]).unwrap();
    assert!(file.header().profiles().is_empty());
}

/// A Set Profile On that is not one is refused when the file is read, and the
/// refusal is the codec's own error: here a Group address asking for Channels.
/// Spec: m2-116/6.2#2
#[test]
fn a_malformed_set_profile_on_is_refused_by_its_fault() {
    let bad = profile_payload(0x7E, 2, MPE, &[3, 0]);
    assert!(matches!(
        read_with_leading(&bad),
        Err(ClipReadError::Build(ClipBuildError::Format(
            ClipFileError::ProfileOn(NotSetProfileOn::ChannelsForBlock(3))
        )))
    ));
}

/// Section 6.2: data meant for a Profile is tagged with a Set Profile On, with
/// that Profile Id, at the beginning of the header, and section 6 puts it before
/// the DCTPQ with no Delta Clockstamp. The message is the Table 24 layout, which
/// the codec's tests pin; here it is the packets, before everything else in the
/// header.
/// Spec: m2-116/6.2#3, m2-116/6#3
#[test]
fn a_profile_is_written_at_the_start_of_the_header() {
    let on = ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    };
    let with_program = read_clip(clip(&with_header(&[dcs(0), program_change()]))).unwrap();
    let header = with_program.header().clone().profile_on(on);
    assert_eq!(header.profiles(), vec![on]);
    assert_eq!(header.messages().len(), set_profile_on_of(MPE).len() + 1);
    let file = ClipFile::new(DIVISION, header, with_program.sequence().clone()).unwrap();
    let bytes = encoded_clip(&file);
    let want = clip(&then(&[
        &set_profile_on_of(MPE),
        &[dcs(0), dctpq(TPQ), dcs(0)],
    ]));
    assert!(bytes.starts_with(&want));
    assert_eq!(read_clip(&bytes).unwrap(), file);
    // A second Profile goes after the first and before the other messages.
    let second = ProfileOnRequest {
        target: ProfileTarget::FunctionBlock,
        profile: ProfileId::GM2_FUNCTION_BLOCK,
    };
    let both = file.header().clone().profile_on(second);
    assert_eq!(both.profiles(), vec![on, second]);
    assert_eq!(both.messages().last().unwrap().1, program_change());
}

/// The events at tick 0 of a clip of `header_end`, as the file writes them.
fn written_after_start(events: Vec<Ump>, header_end: u32) -> (Vec<Ump>, ClipFile) {
    let mut parts = vec![dcs(0), dctpq(TPQ), dcs(header_end), start()];
    for event in events {
        parts.extend([dcs(0), event]);
    }
    parts.extend([dcs(0), end()]);
    let file = read_clip(clip(&parts)).unwrap();
    let packets = packets_of(&encoded_clip(&file));
    let s = packets.iter().position(|p| *p == start()).unwrap();
    (packets[s - 1..].to_vec(), file)
}

/// Sections 7.1.1 and 7.1.2: the first Set Tempo and the first Set Time
/// Signature follow Start of Clip at its tick, the first bar's, using the
/// Delta Clockstamp that precedes Start of Clip: a header that asks for 5
/// ticks has a Delta Clockstamp of 5 before Start of Clip and none between it,
/// the Set Tempo and the Set Time Signature, and the sequence still reads
/// them at tick 0, where the first bar starts. What follows has its own.
/// Spec: m2-116/7.1.1#2, m2-116/7.1.1#3, m2-116/7.1.2#2, m2-116/7.1#2, m2-116/3.2.2#1, m2-104/7.1.10#2
#[test]
fn the_first_tempo_and_signature_use_the_delta_clockstamp_before_start_of_clip() {
    let (packets, file) =
        written_after_start(vec![set_tempo(100.0), time_signature(), on(60, 1)], 5);
    assert_eq!(
        packets[..6],
        [
            dcs(5),
            start(),
            set_tempo(100.0),
            time_signature(),
            dcs(0),
            on(60, 1)
        ]
    );
    let events = &file.sequence().events();
    assert!(events.iter().all(|(tick, _)| *tick == Tick(0)));
    assert_eq!(file.header().end(), Tick(5));
    assert_eq!(read_clip(encoded_clip(&file)).unwrap(), file);
}

/// With no Set Tempo the Set Time Signature is the one that follows Start of
/// Clip; one that is not first at tick 0 has its own Delta Clockstamp.
/// Spec: m2-116/7.1.2#2
#[test]
fn a_signature_with_no_tempo_before_it_uses_the_delta_clockstamp_before_start_of_clip() {
    let (packets, _) = written_after_start(vec![time_signature(), on(60, 1)], 5);
    assert_eq!(packets[..4], [dcs(5), start(), time_signature(), dcs(0)]);
    let (packets, _) = written_after_start(vec![on(60, 1), set_tempo(100.0)], 5);
    assert_eq!(
        packets[..6],
        [dcs(5), start(), dcs(0), on(60, 1), dcs(0), set_tempo(100.0)]
    );
}

/// A file whose first Set Tempo has a Delta Clockstamp of 0 of its own, where
/// this crate used to write one, still reads: the tempo is at tick 0, and the
/// header gap is the one before Start of Clip.
/// Spec: m2-116/7.1.1#2
#[test]
fn a_set_tempo_with_its_own_zero_delta_clockstamp_reads_at_tick_zero() {
    let parts = [
        dcs(0),
        dctpq(TPQ),
        dcs(5),
        start(),
        dcs(0),
        set_tempo(100.0),
        dcs(0),
        on(60, 1),
        dcs(0),
        end(),
    ];
    let file = read_clip(clip(&parts)).unwrap();
    let ticks: Vec<_> = file.sequence().events().iter().map(|e| e.0).collect();
    assert_eq!(ticks, [Tick(0), Tick(0)]);
    assert_eq!(file.header().end(), Tick(5));
}

/// A packet no definition covers has no message to play: the timeline lists it
/// apart from the packets, and drops nothing.
/// Spec: m2-116/7#3
#[test]
fn a_packet_no_definition_covers_is_listed_not_played() {
    // Message type 0x6 is reserved: nothing decodes it.
    let reserved = Ump([0x6001_2345, 0, 0, 0]);
    let known: Ump = Message::Midi2(
        MidiChannel::FIRST.into(),
        tutti_ump::Voice2::ChannelPressure(ControlValue::MIN),
    )
    .into();
    let clip = ClipFile::new(
        DIVISION,
        ClipHeader::new(),
        ClipSequence::new()
            .push(Tick(0), reserved)
            .unwrap()
            .push(Tick(96), known)
            .unwrap(),
    )
    .unwrap();
    let timeline = read_clip(encoded_clip(&clip)).unwrap().timeline();
    assert_eq!(timeline.packets, [(Beat::new(1.0), known)]);
    assert_eq!(
        timeline.unknown,
        [(Beat::new(0.0), tutti_ump::Unknown(reserved))]
    );
}

/// A Clip File's packets are the timeline as they are stored (a MIDI 2.0 note
/// stays MIDI 2.0), and its configuration header is not on it.
/// Spec: m2-116/6#3
#[test]
fn a_clip_files_timeline_is_its_packets_as_stored_without_the_header() {
    let note: Ump = Message::Midi2(
        MidiChannel::FIRST.into(),
        tutti_ump::Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
    )
    .into();
    let header = ClipHeader::new().profile_on(ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    });
    assert!(!header.messages().is_empty());
    let sequence = ClipSequence::new().push(Tick(48), note).unwrap();
    let file = ClipFile::new(DIVISION, header, sequence).unwrap();
    assert_eq!(file.timeline().packets, [(Beat::new(0.5), note)]);
}
