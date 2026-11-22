use super::*;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_midi_protocol::channel_voice;

use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue};
use tutti_ump::RegisteredPerNoteController;
use tutti_ump::{
    Address, BendValue, CCNumber, ControlValue, FlexTempo, Message, MidiChannel, MidiGroup, NoteId,
    NoteNumber, NoteVelocity, PerNoteManagement, Pitch, Sysex7Packets, SysexStatus, System, U7,
    U14, Ump, Voice2,
};
use tutti_units::{Bipolar, Samples};
use tutti_vst3_ffi::EventList;

/// A packet from its words, zero-padded to four, at offset 0.
fn raw(words: &[u32]) -> UmpAt {
    let mut packet = [0; 4];
    packet[..words.len()].copy_from_slice(words);
    (Offset::ZERO, Ump(packet))
}

/// The `noteId` the spec gives `(channel, note)`: `channel * 128 + note`
/// (`tutti_ump::NoteId`), read from the spec's type rather than from
/// `plugin_note_id`, the code under test.
fn spec_note_id(channel: u8, note: u8) -> i32 {
    i32::try_from(NoteId::from_channel_note(MidiChannel::of(channel), NoteNumber::of(note)).raw())
        .unwrap()
}

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: usize) -> Offset {
    Offset::of(Samples::new(k), Samples::new(TEST_BLOCK))
}

/// Encodes one event on a fresh stream (every per-note bend range at the
/// MPE default).
fn encode(event: &UmpAt) -> Option<Event> {
    MidiEncoder::default().encode(event).first().copied()
}

/// Encodes the per-note `event` on a stream whose note on the event's own
/// Group, channel and key is already sounding (a per-note controller of a number
/// with no note is held and sends nothing: M2-104 Appendix C.3). Returns the
/// `noteId` that note was started with and the event made.
fn encode_live(event: &UmpAt) -> (i32, Option<Event>) {
    let mut encoder = MidiEncoder::default();
    let (address, note) = match event.1.decode() {
        Ok(Message::Midi2(
            address,
            Voice2::PolyPressure { note, .. }
            | Voice2::PerNotePitchBend { note, .. }
            | Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. },
        ))
        | Ok(Message::Midi1(address, tutti_ump::Voice1::PolyPressure { note, .. })) => {
            (address, note)
        }
        _ => return (-1, encoder.encode(event).first().copied()),
    };
    let on = Message::Midi2(address, Voice2::note_on(note, NoteVelocity::CENTER));
    let started = match encoder.encode(&(event.0, Ump::from(on))).first() {
        Some(Event {
            payload: Payload::NoteOn(on),
            ..
        }) => on.noteId,
        other => panic!("the note did not start: {other:?}"),
    };
    (started, encoder.encode(event).first().copied())
}

#[test]
fn inbound_generic_data_cc_promotes_to_cv2() {
    // A plugin's output `Data` event of three MIDI-1 bytes (a controller on
    // channel 1) decodes to MIDI-2 Channel Voice 2, not CV1: the engine sees one
    // vocabulary regardless of source (the same widening the hardware input
    // path makes).
    let vst3 = Event::at(0, Payload::Data(Data::inline(0, [0xB1, 74, 100])));
    let back = vst3.to_midi(&mut OutputStream::new()).expect("cc decodes");
    match back.ump.decode().expect("valid UMP") {
        Message::Midi2(address, Voice2::ControlChange { controller, .. }) => {
            assert_eq!(address.channel.get(), 1);
            assert_eq!(controller.get(), 74);
        }
        other => panic!("expected CV2 ControlChange, got {other:?}"),
    }
}

#[test]
fn inbound_system_message_passes_through() {
    // A System real-time message (Timing Clock 0xF8) in a plugin's output has no
    // CV form and must pass through unchanged, not be dropped or widened.
    let vst3 = Event::at(0, Payload::Data(Data::inline(0, [0xF8, 0, 0])));
    let back = vst3
        .to_midi(&mut OutputStream::new())
        .expect("clock decodes");
    assert!(
        matches!(
            back.ump.decode(),
            Ok(Message::System(_, System::TimingClock))
        ),
        "timing clock should stay a System Real-Time message"
    );
}

#[test]
fn note_on_lands_in_note_on_variant() {
    let event = (at(5), midi::note_on(MidiChannel::of(3), 60, 0x8000));
    let vst3 = encode(&event).expect("NoteOn should convert");
    match &vst3.payload {
        Payload::NoteOn(e) => {
            assert_eq!(e.channel, 3);
            assert_eq!(e.pitch, 60);
            assert_eq!(vst3.sample_offset(), 5);
            assert!(e.velocity > 0.0, "expected non-zero velocity");
        }
        _ => panic!("expected NoteOn variant"),
    }

    let back = vst3.to_midi(&mut OutputStream::new()).expect("round-trip");
    assert!(matches!(
        channel_voice(back.ump),
        Some((_, Voice2::NoteOn { note, .. })) if note == NoteNumber::of(60)
    ));
    assert_eq!(back.offset, 5);
}

#[test]
fn note_off_lands_in_note_off_variant() {
    const RELEASE: u16 = 0x4000;
    let event = (at(10), midi::note_off(MidiChannel::FIRST, 72, RELEASE));
    let vst3 = encode(&event).expect("NoteOff should convert");
    match &vst3.payload {
        Payload::NoteOff(e) => {
            assert_eq!(e.pitch, 72);
            assert_eq!(vst3.sample_offset(), 10);
            assert!(
                e.velocity > 0.0,
                "release velocity was dropped on the way to the plugin — \
                 release-layer instruments get the minimum on every note-off"
            );
        }
        _ => panic!("expected NoteOff variant"),
    }

    let back = vst3.to_midi(&mut OutputStream::new()).expect("round-trip");
    assert!(back.ump.is_note_off());
    let Some((_, Voice2::NoteOff { note, velocity, .. })) = channel_voice(back.ump) else {
        panic!("a note-off");
    };
    assert_eq!(note, NoteNumber::of(72));
    assert!(
        velocity.to_u7().get() > 0,
        "release velocity was dropped on the way back from the plugin"
    );
}

/// A frame offset past `i32::MAX` must saturate, not wrap negative.
///
/// An `Offset` is a `u32` and VST3's `sampleOffset` is `i32`, so
/// a bare `as i32` turns a large offset into a negative one, which the plugin
/// indexes its own buffers with, out of bounds, where nothing here can catch it.
#[test]
fn a_huge_frame_offset_saturates_rather_than_going_negative() {
    let last = usize::try_from(u32::MAX).unwrap();
    let event = (
        // A block of 2^32 frames, so that its last offset is `u32::MAX`.
        Offset::of(Samples::new(last), Samples::new(last + 1)),
        midi::note_on(MidiChannel::FIRST, 60, 0x8000),
    );
    let vst3 = encode(&event).expect("NoteOn should convert");
    let offset = vst3.sample_offset();
    assert!(
        offset >= 0,
        "sample_offset went negative ({offset}) — the plugin will index \
         its buffers out of bounds"
    );
    assert_eq!(offset, i32::MAX, "an unrepresentable offset must saturate");
}

#[test]
fn poly_pressure_lands_in_poly_pressure_variant() {
    let event = (
        at(0),
        midi::poly_pressure(
            MidiChannel::of(1),
            60,
            ControlValue::from(U7::of(100)).get(),
        ),
    );
    let (id, vst3) = encode_live(&event);
    let vst3 = vst3.expect("PolyPressure should convert");
    let Payload::PolyPressure(p) = vst3.payload else {
        panic!("expected poly pressure");
    };
    assert_eq!(
        p.noteId, id,
        "the pressure is for the note that is sounding"
    );
    let back = vst3.to_midi(&mut OutputStream::new()).expect("round-trip");
    assert!(matches!(
        channel_voice(back.ump),
        Some((_, Voice2::PolyPressure { note, .. })) if note == NoteNumber::of(60)
    ));
}

/// VST3's `DataEvent` has one type, `kMidiSysEx` (ivstevents.h), so a MIDI 1.0
/// channel message is never sent in one: a controller, pitch bend and program
/// change that no `IMidiMapping` parameter took have no VST3 event. Each is
/// dropped whole and counted.
#[test]
fn unmapped_channel_messages_are_dropped_and_counted_never_sent_as_data() {
    let mut encoder = MidiEncoder::default();
    let events = [
        midi::cc(
            MidiChannel::of(2),
            CCNumber::BRIGHTNESS,
            ControlValue::from(U7::of(100)).get(),
        ),
        midi::pitch_bend(MidiChannel::FIRST, BendValue::from(U14::of(8192)).get()),
        midi::program_change(MidiChannel::of(9), 42, None),
    ];
    for (i, ump) in events.into_iter().enumerate() {
        let made = encoder.encode(&(Offset::ZERO, ump)).to_vec();
        assert!(made.is_empty(), "{ump:?} reached the plugin as {made:?}");
        assert_eq!(encoder.unsendable_dropped(), i as u64 + 1);
    }
}

/// The note-expression `typeId` table, asserted against the **absolute
/// numeric ids from `ivstnoteexpression.h`** — deliberately hardcoded here
/// rather than read back from `expression_type_id`, because a test
/// that compares the table to itself passes with a wrong table.
///
/// `kVolumeTypeID = 0`, `kPanTypeID = 1`, `kTuningTypeID = 2`,
/// `kVibratoTypeID = 3`, `kExpressionTypeID = 4`, `kBrightnessTypeID = 5`.
#[test]
fn note_expression_type_ids_match_the_vst3_spec() {
    assert_eq!(expression_type_id(NoteExpressionType::Volume), Some(0));
    assert_eq!(expression_type_id(NoteExpressionType::Pan), Some(1));
    assert_eq!(expression_type_id(NoteExpressionType::Tuning), Some(2));
    assert_eq!(expression_type_id(NoteExpressionType::Vibrato), Some(3));
    assert_eq!(
        expression_type_id(NoteExpressionType::Expression),
        Some(4),
        "kExpressionTypeID is 4 — NOT Brightness"
    );
    assert_eq!(
        expression_type_id(NoteExpressionType::Brightness),
        Some(5),
        "kBrightnessTypeID is 5"
    );
    // VST3 has no standard note-expression id for poly pressure; it rides
    // the kPolyPressureEvent path instead.
    assert_eq!(expression_type_id(NoteExpressionType::Pressure), None);
}

/// The decoder must mirror the same absolute ids, so an incoming 5 is
/// Brightness (not dropped) and an incoming 4 is Expression (not
/// mislabelled Brightness).
#[test]
fn expression_type_from_id_matches_the_vst3_spec() {
    assert_eq!(expression_type_from_id(0), Some(NoteExpressionType::Volume));
    assert_eq!(expression_type_from_id(1), Some(NoteExpressionType::Pan));
    assert_eq!(expression_type_from_id(2), Some(NoteExpressionType::Tuning));
    assert_eq!(
        expression_type_from_id(3),
        Some(NoteExpressionType::Vibrato)
    );
    assert_eq!(
        expression_type_from_id(4),
        Some(NoteExpressionType::Expression)
    );
    assert_eq!(
        expression_type_from_id(5),
        Some(NoteExpressionType::Brightness)
    );
    // kTextTypeID (6) / kPhonemeTypeID (7) carry text, not a value.
    assert_eq!(expression_type_from_id(6), None);
    assert_eq!(expression_type_from_id(7), None);
}

/// Every VST3-encodable dimension survives a to-id → from-id round trip.
/// Combined with the two absolute-id tests above, this pins both halves.
#[test]
fn note_expression_type_round_trips_for_every_encodable_dimension() {
    for ty in [
        NoteExpressionType::Volume,
        NoteExpressionType::Pan,
        NoteExpressionType::Tuning,
        NoteExpressionType::Vibrato,
        NoteExpressionType::Expression,
        NoteExpressionType::Brightness,
    ] {
        let id = expression_type_id(ty).expect("VST3-encodable");
        assert_eq!(expression_type_from_id(id), Some(ty), "{ty:?} @ id {id}");
    }
}

/// A plugin-defined `typeId` survives instead of being dropped.
///
/// VST3 reserves `kCustomStart` (100000) upward for dimensions a plugin
/// declares through `INoteExpressionController`. Decoding those to `None`
/// has the caller discard the event, so a plugin whose expressiveness is
/// entirely custom looks silent.
#[test]
fn a_plugin_defined_type_id_is_carried_not_dropped() {
    for id in [100_000, 100_001, 8, 12345, u32::MAX] {
        assert_eq!(
            expression_type_from_id(id),
            Some(NoteExpressionType::Custom(id)),
            "custom id {id} must survive decoding"
        );
    }
}

/// A custom id round-trips unchanged — it came from a VST3 plugin, so it
/// goes back out as the same number.
#[test]
fn a_custom_type_id_round_trips_unchanged() {
    for id in [100_000, 8, 999] {
        let ty = expression_type_from_id(id).expect("carried");
        assert_eq!(expression_type_id(ty), Some(id));
    }
}

/// The text slots stay out of the value path in BOTH directions.
///
/// `kTextTypeID` / `kPhonemeTypeID` ride `NoteExpressionTextEvent` and carry
/// a string. Admitting them as `Custom` would let the encoder emit a value
/// event on a text id — a malformed event the plugin would misread.
#[test]
fn the_text_type_ids_never_enter_the_value_path() {
    for id in [6, 7] {
        assert_eq!(
            expression_type_from_id(id),
            None,
            "id {id} carries text, not a value"
        );
        assert_eq!(
            expression_type_id(NoteExpressionType::Custom(id)),
            None,
            "a Custom({id}) must not be encodable as a value event"
        );
    }
}

/// A custom dimension survives staging into an event and reading back —
/// the full path the decoder feeds, not just the id map.
#[test]
fn a_custom_dimension_stages_and_reads_back() {
    let expr = NoteExpressionValue {
        sample_offset: 12,
        note_id: 3,
        expression_type: NoteExpressionType::Custom(100_042),
        value: 0.75,
    };
    let staged = expr.to_event().expect("custom id is encodable");
    match &staged.payload {
        Payload::ExpressionValue(e) => assert_eq!(e.typeId, 100_042),
        other => panic!("expected NoteExpression, got {other:?}"),
    }
    let Payload::ExpressionValue(e) = &staged.payload else {
        unreachable!("matched above")
    };
    assert_eq!((e.value, e.noteId), (0.75, 3));
    assert_eq!(staged.sample_offset(), 12);
}

/// `Expression` is a real VST3 dimension (id 4) and must survive staging
/// into an event and reading back out. The failure mode this guards is
/// Brightness consuming id 4, leaving Expression decoding to `None`.
#[test]
fn expression_dimension_stages_and_reads_back() {
    let expr = NoteExpressionValue {
        sample_offset: 3,
        note_id: spec_note_id(2, 64),
        expression_type: NoteExpressionType::Expression,
        value: 0.25,
    };
    let ev = expr
        .to_event()
        .expect("Expression is VST3-encodable (id 4)");
    match &ev.payload {
        Payload::ExpressionValue(e) => assert_eq!(e.typeId, 4),
        other => panic!("expected NoteExpression, got {other:?}"),
    }
    let Payload::ExpressionValue(e) = &ev.payload else {
        unreachable!("matched above")
    };
    assert_eq!(e.noteId, expr.note_id);
    assert_eq!(ev.sample_offset(), 3);
}

/// A non-finite expression value is dropped, not passed through and not
/// clamped to an invented number.
///
/// `f64::clamp` is not a guard for this: it returns NaN for a NaN input,
/// so a clamp-only fix would still hand the plugin a NaN.
#[test]
fn a_non_finite_expression_value_is_dropped() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: bad,
        };
        assert!(
            expr.to_event().is_none(),
            "{bad} is not a normalized value and must not reach the plugin"
        );
    }
}

/// A finite value outside `[0, 1]` is clamped rather than dropped.
///
/// Dropping would be the worse failure: the dimension would silently hold
/// whatever the plugin last saw, so a runaway modulator reads as a stuck
/// expression rather than as a clipped one.
#[test]
fn an_out_of_range_expression_value_is_clamped_not_dropped() {
    for (input, want) in [(1.25_f64, 1.0_f64), (-0.5, 0.0), (f64::MAX, 1.0)] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: input,
        };
        let staged = expr.to_event().expect("a finite value is always encodable");
        let Payload::ExpressionValue(e) = &staged.payload else {
            panic!("expected NoteExpression, got {:?}", staged.payload)
        };
        assert_eq!(e.value, want, "{input} must clamp to {want}");
    }
}

/// An in-range value passes through bit-exact.
///
/// Pins that the guard is a no-op for every producer inside this crate —
/// the MIDI paths all arrive pre-normalized, so a clamp that perturbed
/// them would go unseen by the two tests above.
#[test]
fn an_in_range_expression_value_is_untouched() {
    for value in [0.0_f64, f64::MIN_POSITIVE, 0.5, 0.7327, 1.0] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value,
        };
        let staged = expr.to_event().expect("in-range is encodable");
        let Payload::ExpressionValue(e) = &staged.payload else {
            panic!("expected NoteExpression, got {:?}", staged.payload)
        };
        assert_eq!(e.value, value, "{value} is already normalized");
    }
}

/// A note expression names its voice by a note id; one that is not the host's
/// `channel * 128 + note` (a plugin's own id space) names no `(channel, note)`
/// and is no MIDI event.
#[test]
fn a_note_expression_of_a_plugins_own_note_id_is_not_a_midi_event() {
    let expr = NoteExpressionValue {
        sample_offset: 0,
        note_id: 5000,
        expression_type: NoteExpressionType::Tuning,
        value: 0.5,
    };
    let vst3 = expr.to_event().expect("Tuning is VST3-encodable");
    assert!(vst3.to_midi(&mut OutputStream::new()).is_none());
}

#[test]
fn data_event_with_truncated_size_fails_gracefully() {
    let data = Data::new(0, [0xB0]).expect("fits inline");
    assert!(
        Event::at(0, Payload::Data(data))
            .to_midi(&mut OutputStream::new())
            .is_none()
    );
}

/// A Note On has a `noteId` of its own, minted above every `(channel, key)`
/// pair's (`NoteId::MAX_CHANNEL_NOTE`); two notes on one key have two (M2-104
/// section 7.4.5: a Detach ends one and the next is another); and a Note Off
/// names no note (`-1`), which the plugin matches by channel and pitch.
#[test]
fn a_note_on_has_an_id_of_its_own_and_its_note_off_names_none() {
    let mut encoder = MidiEncoder::default();
    let mut note_on = |channel: u8, note: u8| match encoder
        .encode(&(
            Offset::ZERO,
            midi::note_on(MidiChannel::of(channel), note, 0x8000),
        ))
        .first()
        .map(|e| e.payload)
    {
        Some(Payload::NoteOn(on)) => on.noteId,
        other => panic!("expected a Note On, got {other:?}"),
    };
    let first = note_on(5, 67);
    let again = note_on(5, 67);
    let other_key = note_on(5, 68);
    for id in [first, again, other_key] {
        assert!(
            u32::try_from(id).unwrap() > tutti_ump::NoteId::MAX_CHANNEL_NOTE,
            "id {id} names a (channel, key) pair or a wildcard"
        );
    }
    assert_ne!(first, again, "two notes of one key");
    assert_ne!(first, other_key);

    let off = encoder
        .encode(&(Offset::ZERO, midi::note_off(MidiChannel::of(5), 67, 0)))
        .first()
        .map(|e| e.payload);
    match off {
        Some(Payload::NoteOff(off)) => {
            assert_eq!((off.channel, off.pitch, off.noteId), (5, 67, -1));
        }
        other => panic!("expected a Note Off, got {other:?}"),
    }
}

#[test]
fn per_note_pitch_bend_becomes_tuning_expression() {
    // Center bend on note 67, channel 5 → Tuning expression at value 0.5,
    // bound to the noteId of the note sounding on (5, 67).
    let event = (
        at(12),
        midi::per_note_pitch_bend(MidiChannel::of(5), 67, BendValue::from(U14::of(8192)).get()),
    );
    let (id, vst3) = encode_live(&event);
    let vst3 = vst3.expect("per-note bend should map");
    let Payload::ExpressionValue(expr) = &vst3.payload else {
        panic!("is a note expression, got {:?}", vst3.payload)
    };
    assert_eq!(expr.typeId, 2, "VST3 `kTuningTypeID`");
    assert_eq!(expr.noteId, id, "the tuning is the sounding note's");
    assert_eq!(vst3.sample_offset(), 12);
    assert_eq!(expr.value, 0.5, "center bend → 0.5");
    // It is NOT a MIDI channel-voice event.
    assert!(vst3.to_midi(&mut OutputStream::new()).is_none());
}

/// A per-note pitch bend is told to the plugin as the semitones it
/// means: the bend over its channel's per-note range (±48 until the
/// stream sets one with RPN #00/07), on VST3's ±120-semitone tuning
/// scale. A quarter bend at 48 is 12 semitones, 0.5 + 12/240; after a
/// sensitivity of 24 on channel 3, 6 semitones there and 12 elsewhere.
#[test]
fn a_per_note_bend_is_tuned_over_its_channels_range() {
    use tutti_midi_protocol::PitchBendSensitivity;

    let bend = |channel: u8, amount: f64| {
        (
            Offset::ZERO,
            midi::per_note_pitch_bend(
                MidiChannel::of(channel),
                60,
                BendValue::from_bipolar(Bipolar::of(amount)).get(),
            ),
        )
    };
    let tuning = |encoder: &mut MidiEncoder, e: &UmpAt| match encoder.encode(e).first().copied() {
        Some(Event {
            payload: Payload::ExpressionValue(x),
            ..
        }) => {
            assert_eq!(x.typeId, 2, "kTuningTypeID");
            x.value
        }
        other => panic!("expected a tuning expression, got {other:?}"),
    };

    let mut encoder = MidiEncoder::default();
    // A note sounds on each channel: a bend with no note is held, not sent.
    for channel in [3, 4] {
        let _ = encoder.encode(&(
            Offset::ZERO,
            midi::note_on(MidiChannel::of(channel), 60, 0x8000),
        ));
    }
    assert_eq!(tuning(&mut encoder, &bend(3, 0.25)), 0.5 + 12.0 / 240.0);
    assert_eq!(tuning(&mut encoder, &bend(3, -1.0)), 0.5 - 48.0 / 240.0);

    let sensitivity = (
        Offset::ZERO,
        midi::registered(
            MidiChannel::of(3),
            tutti_ump::Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::from_semitones(U7::of(24)).to_rpn_bits(),
        ),
    );
    // The sensitivity is consumed (no RPN reaches the plugin), and the bend the
    // sounding note holds is told again at its new size (M2-104 section
    // 7.4.13): 6 semitones of the last one, -1.0 over 24.
    let after = encoder.encode(&sensitivity);
    assert!(
        matches!(
            after,
            [Event {
                payload: Payload::ExpressionValue(_),
                ..
            }]
        ),
        "{after:?}"
    );
    let Payload::ExpressionValue(resent) = after[0].payload else {
        unreachable!()
    };
    assert_eq!(resent.value, 0.5 - 24.0 / 240.0);
    assert_eq!(tuning(&mut encoder, &bend(3, 0.25)), 0.5 + 6.0 / 240.0);
    assert_eq!(tuning(&mut encoder, &bend(4, 0.25)), 0.5 + 12.0 / 240.0);
}

#[test]
fn per_note_controller_maps_known_indices_only() {
    // CC 74 (brightness) is a known dimension → Brightness expression.
    let known = (
        Offset::ZERO,
        Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::AssignablePerNote {
                note: NoteNumber::of(60),
                controller: 74,
                value: ControlValue::from(U7::of(100)),
            },
        )),
    );
    let (id, known) = encode_live(&known);
    match known {
        Some(Event {
            payload: Payload::ExpressionValue(e),
            ..
        }) => {
            // Asserted against the SPEC's absolute id (kBrightnessTypeID = 5),
            // not against `expression_type_id` — comparing the table
            // to itself is what let the off-by-one at index 4 ship.
            assert_eq!(e.typeId, 5, "CC74 → kBrightnessTypeID");
            assert_eq!(e.noteId, id);
        }
        other => panic!("expected Brightness note expression, got {other:?}"),
    }
    // An index with no VST3 expression counterpart is dropped.
    let unknown = (
        Offset::ZERO,
        Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::AssignablePerNote {
                note: NoteNumber::of(60),
                controller: 33,
                value: ControlValue::from(U7::of(100)),
            },
        )),
    );
    assert!(encode_live(&unknown).1.is_none());
}

#[test]
fn single_packet_sysex_round_trips_through_data_event() {
    // A short SysEx (identity request) → VST3 Data event with the SysEx
    // subtype and 0xF0 … 0xF7 framing, then back to the same UMP payload.
    let payload = [0x7E, 0x7F, 0x06, 0x01];
    let single = Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
        .unwrap()
        .next()
        .unwrap();
    let event = (at(7), single);
    let vst3 = encode(&event).expect("SysEx should map to Data");
    match &vst3.payload {
        Payload::Data(e) => {
            assert_eq!(e.data_type(), K_DATA_TYPE_MIDI_SYSEX);
            assert_eq!(e.size() as usize, payload.len() + 2);
            assert_eq!(e.payload()[0], 0xF0);
            assert_eq!(e.payload()[1..1 + payload.len()], payload);
            assert_eq!(e.payload()[1 + payload.len()], 0xF7);
            assert_eq!(vst3.sample_offset(), 7);
        }
        _ => panic!("expected Data variant for SysEx"),
    }

    let back = vst3
        .to_midi(&mut OutputStream::new())
        .expect("SysEx Data round-trips");
    let Ok(Message::Sysex7(_, packet)) = back.ump.decode() else {
        panic!("decodes as SysEx7");
    };
    assert_eq!(packet.status(), SysexStatus::Complete);
    assert_eq!(packet.data(), &payload);
    assert_eq!(back.offset, 7);
}

/// **No packet of a multi-packet SysEx converts on its own**: a lone
/// `START` would hand the plugin an unterminated message. The event list
/// reassembles them (`stage`'s SysEx tests pin that).
#[test]
fn a_sysex_fragment_does_not_convert_alone() {
    let mut frags = Vec::new();
    frags.extend(
        tutti_ump::Sysex7Packets::from_payload(MidiGroup::FIRST, &[1, 2, 3, 4, 5, 6, 7, 8])
            .expect("a 7-bit payload"),
    );
    assert!(frags.len() > 1);
    for frag in &frags {
        assert!(
            encode(&(Offset::ZERO, *frag)).is_none(),
            "fragment {frag:?} converted alone"
        );
    }
}

#[test]
fn note_on_velocity_round_trips_at_full_width() {
    // A MIDI-2 note-on carrying a value not representable in 7 bits keeps
    // its width through the f32 VST3 struct on the way back out, rather than
    // collapsing to a 7-bit grid point.
    let velocity_u16 = 0x9123; // not a multiple of the 7-bit step
    let event = (
        Offset::ZERO,
        midi::note_on(MidiChannel::FIRST, 60, velocity_u16),
    );
    let vst3 = encode(&event).expect("converts");
    let back = vst3.to_midi(&mut OutputStream::new()).expect("round-trips");
    match back.ump.decode().expect("decodes") {
        Message::Midi2(_, Voice2::NoteOn { velocity: raw, .. }) => {
            assert_eq!(raw, NoteVelocity::new(velocity_u16));
        }
        _ => panic!("expected NoteOn"),
    }
}

// ── int / chord / scale / text / legacy-cc-out ───────────────────────────

fn legacy_cc(control: u8, channel: i8, value: i8, value2: i8) -> LegacyCcOut {
    LegacyCcOut(vst3::Steinberg::Vst::LegacyMIDICCOutEvent {
        controlNumber: control,
        channel,
        value,
        value2,
    })
}

#[test]
fn note_expression_int_round_trips_with_its_sign() {
    let value = NoteExpressionIntValue {
        sample_offset: 9,
        note_id: spec_note_id(1, 50),
        type_id: 3,
        value: -1234,
    };
    let event = value.to_event().expect("always encodable");
    let Payload::ExpressionInt(raw) = event.payload else {
        panic!("not an int expression");
    };
    assert_eq!(raw.value, (-1234i64).cast_unsigned(), "the same 64 bits");
    assert_eq!(raw.noteId, spec_note_id(1, 50));
    assert_eq!((raw.typeId, event.sample_offset()), (3, 9));
    assert!(
        event.to_midi(&mut OutputStream::new()).is_none(),
        "not a MIDI message"
    );
}

/// A chord, scale and text value stage into a list, intern their text, and read
/// back from the event and the arena.
#[test]
fn harmony_values_round_trip_through_a_list() {
    let chord = Utf16Chord {
        sample_offset: 12,
        root: 60,
        bass_note: 48,
        mask: 0b100_1001_0001,
        text: event_text("Cmaj7"),
    };
    let scale = Utf16Scale {
        sample_offset: 0,
        root: 62,
        mask: 0x5ab5,
        text: event_text("D Dorian"),
    };
    let text = Utf16Text {
        sample_offset: 4,
        note_id: spec_note_id(2, 64),
        type_id: 7,
        text: event_text("staccato"),
    };

    let list = EventList::input();
    let mut stage = list.begin();
    for event in [
        chord.to_event(&mut stage),
        scale.to_event(&mut stage),
        text.to_event(&mut stage),
    ] {
        assert!(stage.push(event));
    }
    drop(stage);

    list.with_events(|staged| {
        let by_kind = |f: fn(&Payload) -> bool| {
            staged
                .events
                .iter()
                .find(|e| f(&e.payload))
                .expect("staged")
        };
        let chord_event = by_kind(|p| matches!(p, Payload::Chord(_)));
        let Payload::Chord(back) = &chord_event.payload else {
            unreachable!("found by kind")
        };
        assert_eq!((back.root, back.bass_note, back.mask), (60, 48, chord.mask));
        assert_eq!(chord_event.sample_offset(), 12);
        assert_eq!(back.text.slice(staged.text), Some(chord.text.as_slice()));

        let Payload::Scale(back) = &by_kind(|p| matches!(p, Payload::Scale(_))).payload else {
            unreachable!("found by kind")
        };
        assert_eq!((back.root, back.mask), (62, 0x5ab5));
        assert_eq!(back.text.slice(staged.text), Some(scale.text.as_slice()));

        let Payload::ExpressionText(back) =
            &by_kind(|p| matches!(p, Payload::ExpressionText(_))).payload
        else {
            unreachable!("found by kind")
        };
        assert_eq!((back.note_id, back.type_id), (spec_note_id(2, 64), 7));
        assert_eq!(back.text.slice(staged.text), Some(text.text.as_slice()));
        assert!(
            by_kind(|p| matches!(p, Payload::Chord(_)))
                .to_midi(&mut OutputStream::new())
                .is_none()
        );
    });
}

/// A plugin-emitted legacy-MIDI-CC-out event decodes to the CC / pitch-bend
/// / poly-pressure MIDI message it stands for.
#[test]
fn legacy_cc_out_decodes_to_midi() {
    use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kCtrlPolyPressure, kPitchBend};

    // Plain CC 74 = 100 on channel 3.
    let cc = Event::at(5, Payload::LegacyCcOut(legacy_cc(74, 3, 100, 0)));
    let m = cc.to_midi(&mut OutputStream::new()).expect("CC decodes");
    assert_eq!(m.offset, 5);
    match m.ump.decode().expect("UMP") {
        Message::Midi2(address, Voice2::ControlChange { controller, value }) => {
            assert_eq!((address.channel.get(), controller.get()), (3, 74));
            // M2-115 section 3.3 upscales 100 to `0xC924_9249`.
            assert_eq!(value, ControlValue::new(0xC924_9249));
        }
        other => panic!("expected CC, got {other:?}"),
    }

    // Pitch bend: 14-bit from (value=LSB, value2=MSB). Center = 0,0x40.
    let pb = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kPitchBend as u8, 0, 0, 0x40)),
    );
    let pb_m = pb.to_midi(&mut OutputStream::new()).unwrap();
    match pb_m.ump.decode().expect("UMP") {
        Message::Midi2(_, Voice2::PitchBend(bend)) => {
            assert_eq!(bend.bipolar(), Bipolar::ZERO)
        }
        other => panic!("expected PitchBend, got {other:?}"),
    }

    // Poly pressure: value = note, value2 = pressure, upscaled from 7 bits.
    let pp = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kCtrlPolyPressure as u8, 3, 60, 0x21)),
    );
    let pp_m = pp.to_midi(&mut OutputStream::new()).unwrap();
    assert_eq!(
        pp_m.ump.decode(),
        Ok(Message::Midi2(
            Address {
                group: MidiGroup::FIRST,
                channel: MidiChannel::of(3)
            },
            Voice2::PolyPressure {
                note: NoteNumber::new(60).unwrap(),
                value: ControlValue::new(0x4200_0000)
            }
        ))
    );

    // Aftertouch (channel pressure).
    let at = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kAfterTouch as u8, 1, 127, 0)),
    );
    let at_m = at.to_midi(&mut OutputStream::new()).unwrap();
    match at_m.ump.decode().expect("UMP") {
        Message::Midi2(address, Voice2::ChannelPressure(_)) => {
            assert_eq!(address.channel.get(), 1)
        }
        other => panic!("expected ChannelPressure, got {other:?}"),
    }
}

/// `LegacyMIDICCOutEvent`'s controller numbers above the CCs
/// (ivstmidicontrollers.h): 130 is Program Change, 132 to 140 the System
/// messages (Cable Select, 135, has none), with `value2` the MSB of Song
/// Position.
#[test]
fn legacy_cc_out_numbers_above_the_controllers_are_program_change_and_system() {
    let group = MidiGroup::of(2);
    let decode = |control: u8, value: i8, value2: i8| {
        let mut event = Event::at(
            3,
            Payload::LegacyCcOut(legacy_cc(control, 5, value, value2)),
        );
        event.header.bus_index = 2;
        event
            .to_midi(&mut OutputStream::new())
            .map(|m| m.ump.decode())
    };
    assert_eq!(
        decode(130, 42, 0),
        Some(Ok(Message::Midi2(
            Address {
                group,
                channel: MidiChannel::of(5)
            },
            Voice2::ProgramChange {
                program: U7::new(42).unwrap(),
                bank: None
            }
        )))
    );
    let system = |system| Some(Ok(Message::System(group, system)));
    assert_eq!(
        decode(132, 0x35, 0),
        system(System::TimeCode(U7::new(0x35).unwrap()))
    );
    assert_eq!(
        decode(133, 9, 0),
        system(System::SongSelect(U7::new(9).unwrap()))
    );
    // Song Position: value is the LSB, value2 the MSB.
    assert_eq!(
        decode(134, 0x34, 0x24),
        system(System::SongPosition(U14::new(0x24 << 7 | 0x34).unwrap()))
    );
    assert_eq!(decode(135, 1, 0), None, "Cable Select has no MIDI message");
    assert_eq!(decode(136, 0, 0), system(System::TuneRequest));
    assert_eq!(decode(137, 0, 0), system(System::Start));
    assert_eq!(decode(138, 0, 0), system(System::Continue));
    assert_eq!(decode(139, 0, 0), system(System::Stop));
    assert_eq!(decode(140, 0, 0), system(System::ActiveSensing));
    assert_eq!(decode(141, 0, 0), None, "past the last number");
}

/// An output event's `busIndex` is the event output bus it left on, the UMP
/// Group of its message: a note, a legacy controller and a raw MIDI 1.0 data
/// event keep it, and a bus past the 16 Groups drops the event.
#[test]
fn an_output_events_bus_index_is_its_group() {
    let on_bus = |bus: i32, mut event: Event| {
        event.header.bus_index = bus;
        event.to_midi(&mut OutputStream::new())
    };
    let group_of = |midi: Option<tutti_plugin_types::WireMidi>| match midi.unwrap().ump.decode() {
        Ok(Message::Midi2(address, _)) => address.group,
        Ok(Message::System(group, _)) => group,
        other => panic!("not a message with a Group: {other:?}"),
    };
    let note = Event::at(0, Payload::NoteOn(NoteOn::new(1, 60, 0.5, -1)));
    assert_eq!(group_of(on_bus(7, note)), MidiGroup::of(7));
    let cc = Event::at(0, Payload::LegacyCcOut(legacy_cc(74, 3, 100, 0)));
    assert_eq!(group_of(on_bus(3, cc)), MidiGroup::of(3));
    let raw = Event::at(0, Payload::Data(Data::inline(0, [0xB1, 74, 100])));
    assert_eq!(group_of(on_bus(15, raw)), MidiGroup::of(15));
    let clock = Event::at(0, Payload::Data(Data::inline(0, [0xF8, 0, 0])));
    assert_eq!(group_of(on_bus(1, clock)), MidiGroup::of(1));
    assert!(
        on_bus(
            16,
            Event::at(0, Payload::NoteOn(NoteOn::new(1, 60, 0.5, -1)))
        )
        .is_none()
    );
    assert!(
        on_bus(
            -1,
            Event::at(0, Payload::NoteOn(NoteOn::new(1, 60, 0.5, -1)))
        )
        .is_none()
    );
}

/// Chord / scale / text / int events are not channel-voice MIDI.
#[test]
fn harmony_events_are_not_midi() {
    let chord = Event::at(
        0,
        Payload::Chord(Chord {
            root: 60,
            bass_note: 60,
            mask: 0,
            text: TextRef::default(),
        }),
    );
    assert!(chord.to_midi(&mut OutputStream::new()).is_none());
}

// ── reading through `tutti_ump::Message` ─────────────────────────────────

mod message {
    use super::*;
    use tutti_ump::{
        Address, Attribute, Bank, Flex, NoteNumber, Sysex7Packet, Sysex8StreamIds, SysexStatus,
        Utility, Voice1,
    };

    fn address(channel: u8) -> Address {
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::of(channel),
        }
    }

    fn note(n: u8) -> NoteNumber {
        NoteNumber::new(n).unwrap()
    }

    fn u7(v: u8) -> U7 {
        U7::new(v).unwrap()
    }

    fn packet(message: Message) -> UmpAt {
        (at(7), Ump::from(message))
    }

    /// A MIDI 1.0 Note On at velocity 0 translates to a Note Off at velocity
    /// 0x8000 (M2-104 v1.1.2 D.3.1), which VST3 carries as the release
    /// velocity `0x8000 / 0xFFFF`, about 0.5. It was a release velocity of 0.
    #[test]
    fn a_midi1_note_on_at_velocity_zero_is_a_note_off_at_one_half() {
        let event = packet(Message::Midi1(
            address(2),
            Voice1::NoteOn {
                note: note(60),
                velocity: u7(0),
            },
        ));
        let vst3 = encode(&event).expect("a note");
        let Payload::NoteOff(off) = vst3.payload else {
            panic!("expected a Note Off, got {vst3:?}");
        };
        assert_eq!((off.channel, off.pitch), (2, 60));
        assert_eq!(off.velocity, (32768.0_f64 / 65535.0) as f32);
    }

    /// A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104 §7.4.2: it
    /// "does not function as a Note Off"), at the smallest velocity above 0,
    /// 1/65535 (D.2's 0 -> 1), since a VST3 plugin reads a note-on at 0 as a
    /// release.
    #[test]
    fn a_midi2_note_on_at_velocity_zero_is_a_note_on_at_the_least_velocity() {
        let event = packet(Message::Midi2(
            address(2),
            Voice2::NoteOn {
                note: note(60),
                velocity: NoteVelocity::MIN,
                attribute: Attribute::None,
            },
        ));
        let vst3 = encode(&event).expect("a note");
        let Payload::NoteOn(on) = vst3.payload else {
            panic!("expected a Note On, got {vst3:?}");
        };
        assert_eq!((on.channel, on.pitch), (2, 60));
        assert_eq!(on.velocity, 1.0 / 65535.0);
    }

    /// A reserved attribute type (M2-104 Table 8, §7.4.14: reserved for senders,
    /// no receiver rule) does not stop the note, which plays; a Pitch attribute is read but not applied, so the Note On's
    /// `tuning` stays 0.
    #[test]
    fn a_reserved_attribute_type_does_not_stop_the_note() {
        for attribute in [
            midi::reserved_attribute(0x04, 0xABCD),
            midi::reserved_attribute(0xFF, 0),
            Attribute::Pitch(Pitch::from_q7_9(0x3C00)),
            Attribute::Manufacturer(0x1234),
        ] {
            let event = packet(Message::Midi2(
                address(1),
                Voice2::NoteOn {
                    note: note(60),
                    velocity: NoteVelocity::CENTER,
                    attribute,
                },
            ));
            let vst3 = encode(&event).unwrap_or_else(|| panic!("{attribute:?} stopped the note"));
            let Payload::NoteOn(on) = vst3.payload else {
                panic!("expected a Note On, got {vst3:?}");
            };
            assert_eq!(
                (on.channel, on.pitch, on.tuning),
                (1, 60, 0.0),
                "{attribute:?}"
            );
            assert_eq!(on.velocity, 32768.0 / 65535.0);
        }
    }

    /// This host is a sink for what VST3 has no event for. The SDK's `Event`
    /// has no raw UMP event and its `Data` event has the one subtype
    /// `kMidiSysEx`, so an `Unknown` packet cannot be forwarded: it is `None`,
    /// not an error, and so is every kind with neither a VST3 event nor a
    /// MIDI 1.0 form.
    #[test]
    fn what_vst3_has_no_event_for_is_a_sink() {
        let sinks = [
            // Reserved message type 0x6, and a Channel Voice 2 status 0x7 that
            // is undefined: `decode` cannot frame them.
            raw(&[0x6000_1234]),
            raw(&[0x4070_0000, 0]),
            packet(Message::Utility(Utility::Noop)),
            packet(Message::Utility(Utility::JrClock(9))),
            // An Endpoint Discovery (stream) and a SysEx 8 packet (Data128).
            raw(&[0xF000_0101, 0x1F, 0, 0]),
            raw(&[0x5001_0000, 0, 0, 0]),
            packet(Message::Flex(
                MidiGroup::FIRST,
                Flex::SetTempo(FlexTempo::new(50_000_000)),
            )),
            packet(Message::Midi2(
                address(0),
                Voice2::PerNoteManagement {
                    note: note(60),
                    management: PerNoteManagement::Detach,
                },
            )),
            // The start of a multi-packet SysEx stands for nothing alone.
            packet(Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(SysexStatus::Start, &[1, 2, 3]).unwrap(),
            )),
        ];
        // Only the first two are packets `decode` cannot frame; the rest are
        // defined messages.
        for (i, event) in sinks.iter().enumerate() {
            assert_eq!(
                event.1.decode().is_err(),
                i < 2,
                "{event:?} decodes as the wrong kind"
            );
        }
        for event in sinks {
            assert!(encode(&event).is_none(), "{event:?} reached the plugin");
        }
    }

    /// Each System message has no VST3 event (`DataEvent` is `kMidiSysEx`
    /// alone): it is dropped and counted.
    #[test]
    fn a_system_message_is_dropped_and_counted() {
        let cases = [
            System::TimeCode(u7(0x35)),
            System::SongPosition(U14::new(0x1234).unwrap()),
            System::SongSelect(u7(9)),
            System::TuneRequest,
            System::TimingClock,
            System::Start,
            System::Continue,
            System::Stop,
            System::ActiveSensing,
            System::Reset,
        ];
        let mut encoder = MidiEncoder::default();
        for (i, system) in cases.into_iter().enumerate() {
            let event = packet(Message::System(MidiGroup::FIRST, system));
            assert!(encoder.encode(&event).is_empty(), "{system:?} was sent");
            assert_eq!(encoder.unsendable_dropped(), i as u64 + 1, "{system:?}");
        }
    }

    /// A controller, program, pressure and bend, MIDI 1.0 or 2.0, and a banked
    /// program change or an RPN, have no VST3 event: each is dropped whole and
    /// counted, and none is a `Data` event.
    #[test]
    fn controllers_have_no_vst3_event_and_are_counted() {
        let ch = address(3);
        let messages = [
            Message::Midi1(
                ch,
                Voice1::ControlChange {
                    controller: CCNumber::new(74).unwrap(),
                    value: u7(100),
                },
            ),
            Message::Midi1(ch, Voice1::PitchBend(U14::new(0x2001).unwrap())),
            Message::Midi1(ch, Voice1::ChannelPressure(u7(5))),
            Message::Midi1(ch, Voice1::ProgramChange(u7(9))),
            Message::Midi2(
                ch,
                Voice2::ControlChange {
                    controller: CCNumber::new(74).unwrap(),
                    value: ControlValue::MAX,
                },
            ),
            Message::Midi2(ch, Voice2::PitchBend(BendValue::CENTER)),
            Message::Midi2(ch, Voice2::ChannelPressure(ControlValue::CENTER)),
            Message::Midi2(
                ch,
                Voice2::ProgramChange {
                    program: u7(9),
                    bank: Some(Bank {
                        msb: u7(1),
                        lsb: u7(5),
                    }),
                },
            ),
        ];
        let mut encoder = MidiEncoder::default();
        for (i, message) in messages.into_iter().enumerate() {
            let made = encoder.encode(&packet(message)).to_vec();
            assert!(
                made.is_empty(),
                "{message:?} reached the plugin as {made:?}"
            );
            assert_eq!(encoder.unsendable_dropped(), i as u64 + 1, "{message:?}");
        }
    }

    /// Packets written out as M2-104 words (MIDI 1.0 and 2.0 forms of a
    /// controller, program, pressure and bend) are the messages above: none
    /// reaches the plugin.
    #[test]
    fn spec_words_for_channel_messages_are_dropped() {
        let mut encoder = MidiEncoder::default();
        let words: [&[u32]; 8] = [
            &[0x20C3_0900],
            &[0x20D3_4500],
            &[0x20B3_4A64],
            &[0x20E3_0140],
            &[0x40C3_0000, 0x0900_0000],
            &[0x40D3_0000, 0x8800_0000],
            &[0x40B3_4A00, 0xC800_0000],
            &[0x40E3_0000, 0x0504_0000],
        ];
        for (i, w) in words.into_iter().enumerate() {
            assert!(encoder.encode(&raw(w)).is_empty(), "{w:08x?} was sent");
            assert_eq!(encoder.unsendable_dropped(), i as u64 + 1, "{w:08x?}");
        }
    }

    /// Per-note pressure and controllers written out as words. The type ids
    /// are the SDK's (volume 0, pan 1, brightness 5), not the host's table.
    #[test]
    fn spec_words_give_the_exact_note_events() {
        // Every packet is for channel 3, key 60, which is sounding.
        let event = |words: &[u32]| encode_live(&raw(words)).1.expect("an event");
        let expression_type = |words: &[u32]| {
            let (id, made) = encode_live(&raw(words));
            match made.expect("an event").payload {
                Payload::ExpressionValue(e) => (e.typeId, e.noteId == id),
                other => panic!("expected an expression, got {other:?}"),
            }
        };
        // Registered Per-Note Controller (status 0): note 60, controller in
        // bits 7-0 of word 0.
        assert_eq!(expression_type(&[0x4003_3C07, 0x8000_0000]), (0, true));
        assert_eq!(expression_type(&[0x4003_3C0A, 0x8000_0000]), (1, true));
        assert_eq!(expression_type(&[0x4003_3C4A, 0x8000_0000]), (5, true));
        assert!(encode_live(&raw(&[0x4003_3C01, 0x8000_0000])).1.is_none());
        // Assignable Per-Note Controller (status 1).
        assert_eq!(expression_type(&[0x4013_3C07, 0x8000_0000]), (0, true));
        assert_eq!(expression_type(&[0x4013_3C0A, 0x8000_0000]), (1, true));
        assert_eq!(expression_type(&[0x4013_3C4A, 0x8000_0000]), (5, true));
        // Poly Pressure (status A): MIDI 2.0 carries 32 bits; MIDI 1.0 0x21
        // widens to 0x4200_0000.
        let Payload::PolyPressure(p) = event(&[0x40A3_3C00, 0x4000_0000]).payload else {
            panic!("expected poly pressure");
        };
        assert_eq!((p.channel, p.pitch), (3, 60));
        assert_eq!(
            p.pressure,
            (f64::from(0x4000_0000_u32) / f64::from(u32::MAX)) as f32
        );
        let Payload::PolyPressure(p) = event(&[0x20A3_3C21]).payload else {
            panic!("expected poly pressure");
        };
        assert_eq!(
            p.pressure,
            (f64::from(0x4200_0000_u32) / f64::from(u32::MAX)) as f32
        );
    }

    /// A MIDI 1.0 message a plugin sends as raw bytes is translated on the way
    /// in: a Note On at velocity 0 is a Note Off at 0x8000, and a system
    /// message is forwarded unchanged.
    #[test]
    fn raw_midi1_from_a_plugin_is_widened() {
        let on = Event::at(4, Payload::Data(Data::inline(0, [0x92, 60, 0])));
        let back = on.to_midi(&mut OutputStream::new()).expect("decodes");
        assert_eq!(back.offset, 4);
        assert_eq!(
            back.ump.decode(),
            Ok(Message::Midi2(
                address(2),
                Voice2::NoteOff {
                    note: note(60),
                    velocity: NoteVelocity::CENTER,
                    attribute: Attribute::None,
                }
            ))
        );
        let clock = Event::at(0, Payload::Data(Data::inline(0, [0xF8, 0, 0])))
            .to_midi(&mut OutputStream::new());
        assert_eq!(
            clock.map(|m| m.ump.decode()),
            Some(Ok(Message::System(MidiGroup::FIRST, System::TimingClock)))
        );
    }

    /// **Every kind of message is read without allocating.** Each packet kind
    /// goes through the encoder, the bank split and the output decode inside
    /// `assert_no_alloc`, and each packet is checked to decode to the kind it
    /// stands for, so a malformed packet cannot stand in for a kind. The VST3
    /// host has no worker pool: the calling thread is the only participant.
    #[test]
    fn every_kind_of_message_is_read_without_allocating() {
        let ch = address(4);
        let messages = [
            Message::Utility(Utility::Noop),
            Message::System(MidiGroup::FIRST, System::SongPosition(U14::MAX)),
            Message::System(MidiGroup::FIRST, System::TimingClock),
            Message::Midi1(
                ch,
                Voice1::NoteOn {
                    note: note(60),
                    velocity: u7(0),
                },
            ),
            Message::Midi1(
                ch,
                Voice1::PolyPressure {
                    note: note(60),
                    pressure: u7(9),
                },
            ),
            Message::Midi1(ch, Voice1::PitchBend(U14::MAX)),
            Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(SysexStatus::Complete, &[1, 2, 3]).unwrap(),
            ),
            Message::Midi2(
                ch,
                Voice2::NoteOn {
                    note: note(60),
                    velocity: NoteVelocity::MAX,
                    attribute: midi::reserved_attribute(9, 1),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::PerNotePitchBend {
                    note: note(60),
                    value: BendValue::new(0x9000_0000),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::AssignablePerNote {
                    note: note(60),
                    controller: 74,
                    value: ControlValue::new(7),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::ProgramChange {
                    program: u7(1),
                    bank: Some(Bank {
                        msb: u7(1),
                        lsb: u7(2),
                    }),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::ControlChange {
                    controller: CCNumber::new(1).unwrap(),
                    value: ControlValue::new(1),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::Registered {
                    bank: u7(0),
                    index: u7(7),
                    value: ControlValue::new(0x1000_0000),
                },
            ),
            Message::Flex(MidiGroup::FIRST, Flex::SetTempo(FlexTempo::new(50_000_000))),
            Sysex8StreamIds::undiscovered()
                .open()
                .unwrap()
                .packets(MidiGroup::FIRST, &[1, 2, 3])
                .next()
                .unwrap()
                .decode()
                .unwrap(),
        ];
        let mut events = Vec::new();
        for message in messages {
            let event = packet(message);
            assert_eq!(event.1.decode(), Ok(message), "{message:?}");
            events.push(event);
        }
        // A Stream message, spelled out: an Endpoint Discovery.
        let discovery = raw(&[0xF000_0101, 0x1F, 0, 0]);
        assert!(matches!(discovery.1.decode(), Ok(Message::Stream(_))));
        events.push(discovery);
        // A packet `decode` cannot frame.
        let unknown = raw(&[0x6000_0000]);
        assert!(unknown.1.decode().is_err());
        events.push(unknown);

        // The plugin-output side: raw MIDI 1.0 bytes in a Data event.
        let raw: Vec<Event> = [
            [0x90, 60, 0],
            [0x91, 60, 100],
            [0xB2, 7, 99],
            [0xE3, 1, 64],
            [0xF2, 2, 1],
            [0xF8, 0, 0],
        ]
        .into_iter()
        .map(|bytes| Event::at(1, Payload::Data(Data::inline(0, bytes))))
        .collect();
        for event in &raw {
            assert!(
                event.to_midi(&mut OutputStream::new()).is_some(),
                "{event:?} did not decode"
            );
        }
        let legacy = Event::at(2, Payload::LegacyCcOut(legacy_cc(74, 3, 100, 0)));
        let mut encoder = MidiEncoder::default();
        assert_no_alloc::assert_no_alloc(|| {
            for event in &events {
                let _ = encoder.encode(event);
            }
            for event in &raw {
                let _ = event.to_midi(&mut OutputStream::new());
            }
            let _ = legacy.to_midi(&mut OutputStream::new());
        });
    }

    /// **A plugin's malformed MIDI bytes are dropped without allocating.** A
    /// stray `0xF7`, a truncated Note On, an undefined status, a data byte with
    /// its top bit set each read as nothing, inside
    /// `assert_no_alloc`; a good message beside them still reads.
    #[test]
    fn a_stray_status_from_a_plugin_allocates_nothing() {
        let hostile: Vec<Event> = [
            &[0xF7, 1, 2][..],
            &[0x90, 60],
            &[0x90, 0x80, 1],
            &[0xF4, 1, 2],
            &[0xB0, 7],
        ]
        .into_iter()
        .map(|bytes| {
            Event::at(
                0,
                Payload::Data(Data::new(0, bytes.iter().copied()).unwrap()),
            )
        })
        .collect();
        let good = Event::at(3, Payload::Data(Data::inline(0, [0xB0, 7, 99])));
        assert_no_alloc::assert_no_alloc(|| {
            for event in &hostile {
                assert!(
                    event.to_midi(&mut OutputStream::new()).is_none(),
                    "{event:?} read as a message"
                );
            }
            assert!(good.to_midi(&mut OutputStream::new()).is_some());
        });
    }

    /// **A SysEx a plugin emits as a Data event is a `SysexMessage`**: one
    /// framed `0xF0 .. 0xF7` message of 7-bit bytes that fits one packet is
    /// forwarded, and one with no `0xF7`, a payload byte of 128 or more, or
    /// more than one packet's worth is dropped (never cut or masked).
    #[test]
    fn a_plugins_sysex_data_event_is_forwarded_only_when_whole() {
        let data = |bytes: &[u8]| {
            Event::at(
                2,
                Payload::Data(Data::new(0, bytes.iter().copied()).unwrap()),
            )
        };
        let whole = data(&[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7])
            .to_midi(&mut OutputStream::new())
            .unwrap();
        assert_eq!(whole.offset, 2);
        assert_eq!(
            whole.ump,
            Sysex7Packets::from_payload(MidiGroup::FIRST, &[0x7E, 0x7F, 0x06, 0x01])
                .expect("a payload of 7-bit bytes")
                .collect::<Vec<Ump>>()[0]
        );
        assert!(
            data(&[0xF0, 0x7E, 0x7F, 0x06, 0x01])
                .to_midi(&mut OutputStream::new())
                .is_none()
        );
        assert!(
            data(&[0xF0, 0x7E, 0x80, 0xF7])
                .to_midi(&mut OutputStream::new())
                .is_none()
        );
        assert!(
            data(&[0xF0, 1, 2, 3, 4, 5, 6, 7, 0xF7])
                .to_midi(&mut OutputStream::new())
                .is_none()
        );
    }

    /// **A SysEx the block's MIDI has no room for is dropped whole and lost**:
    /// one of 67 packets does not fit the 64 a block holds, so none of it is
    /// emitted and the stream counts one lost message (what the host reports in
    /// `midi_dropped`); one that fits is emitted and costs nothing.
    #[test]
    fn a_sysex_that_does_not_fit_the_block_is_counted_lost() {
        let drain = |bytes: usize| {
            let payload: Vec<u8> = (0..bytes).map(|i| 1 + (i % 100) as u8).collect();
            let events: Vec<UmpAt> = Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
                .expect("7-bit bytes")
                .map(|ump| (at(7), ump))
                .collect();
            let list = EventList::input();
            assert!(Stager::default().stage(
                &list,
                &InputEvents {
                    midi: &events,
                    ..Default::default()
                },
            ));
            let mut stream = OutputStream::new();
            let mut out = tutti_plugin_types::RtMidiEvents::new();
            drain_midi(&list, &mut stream, &mut out);
            (out.len(), stream.lost_total(), stream.sysex_dropped())
        };
        let (emitted, lost, sysex) = drain(6 * 64);
        assert_eq!((emitted, lost, sysex), (64, 0, 0), "64 packets fit");
        let (emitted, lost, sysex) = drain(6 * 64 + 1);
        assert_eq!((emitted, lost, sysex), (0, 1, 1), "65 packets do not");
    }

    /// **A list the plugin holds is lost, counted once**: a block whose output
    /// list a plugin thread holds is not read, and the stream counts one lost
    /// event for it (the least it holds), which is what `midi_dropped` reports.
    #[test]
    fn an_output_list_the_plugin_holds_is_counted_lost() {
        let list = EventList::input();
        let mut stream = OutputStream::new();
        let mut out = tutti_plugin_types::RtMidiEvents::new();
        {
            let _plugin_writing = list.begin();
            drain_midi(&list, &mut stream, &mut out);
        }
        assert!(out.is_empty());
        assert_eq!((stream.lists_busy(), stream.lost_total()), (1, 1));
        drain_midi(&list, &mut stream, &mut out);
        assert_eq!(stream.lost_total(), 1, "a free list costs nothing");
    }

    /// A chord at one frame stages as three Note Ons, in order, each at the
    /// velocity it was sent at.
    #[test]
    fn a_chord_stages_as_three_note_ons_in_order() {
        let events: Vec<UmpAt> = [60u8, 64, 67]
            .into_iter()
            .map(|n| {
                packet(Message::Midi1(
                    address(0),
                    Voice1::NoteOn {
                        note: note(n),
                        velocity: u7(100),
                    },
                ))
            })
            .collect();
        let list = EventList::input();
        let mut stager = Stager::default();
        assert!(stager.stage(
            &list,
            &InputEvents {
                midi: &events,
                ..Default::default()
            },
        ));
        let seen: Vec<(i16, f32, i32)> = list.with_events(|staged| {
            staged
                .events
                .iter()
                .map(|e| match e.payload {
                    Payload::NoteOn(on) => (on.pitch, on.velocity, e.sample_offset()),
                    other => panic!("expected Note Ons, got {other:?}"),
                })
                .collect()
        });
        // M2-115 §3.3: 100 upscales to 100 << 9 plus its low six bits repeated,
        // 0b110100 << 3 | 0b110100 >> 3 | 0b110100 >> 9.. = 51492.
        let velocity = (f64::from(51492_u16) / 65535.0) as f32;
        assert_eq!(
            seen,
            vec![(60, velocity, 7), (64, velocity, 7), (67, velocity, 7)]
        );
    }
}

/// **A plugin's note expression is the per-note message of its dimension**, on
/// the Group of its event bus: volume, pan and brightness are the Registered
/// Per-Note Controllers 7, 10 and 74 (M2-104 Appendix A), over the full 32 bits
/// of the unit; a tuning of `0.5 + 12/240` is 12 semitones, which is Registered
/// Per-Note Controller 3, Pitch 7.25, at the absolute pitch 72.0 (§7.4.15.2).
/// A dimension with no per-note message, a custom id, a note id the plugin did
/// not start, a value that is not a number and a tuning past Pitch 7.25's range
/// are dropped, and each is counted lost once.
#[test]
fn a_plugins_note_expression_is_the_per_note_message_of_its_dimension() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let note_id = spec_note_id(2, 60);
    // The note the plugin started under this id: an expression names a note only
    // through an id the plugin started.
    let mut stream = OutputStream::new();
    let started = Event::at(0, Payload::NoteOn(NoteOn::new(2, 60, 0.5, note_id)));
    assert!(started.to_midi(&mut stream).is_some());
    let mut decode = |type_id: u32, id: i32, value: f64| {
        let mut event = Event::at(
            4,
            Payload::ExpressionValue(ExpressionValue::new(type_id, id, value)),
        );
        event.header.bus_index = 5;
        event
            .to_midi(&mut stream)
            .map(|m| (m.offset, m.ump.decode()))
    };
    let per_note = |voice| {
        Some((
            4,
            Ok(Message::Midi2(
                Address {
                    group: MidiGroup::of(5),
                    channel: MidiChannel::of(2),
                },
                voice,
            )),
        ))
    };
    let controller = |controller, value| Voice2::RegisteredPerNote {
        note: NoteNumber::of(60),
        controller,
        value: ControlValue::new(value),
    };
    // Volume is `0` for silence, `0.25` for 0 dB and `1` for +12 dB
    // (`ivstnoteexpression.h`): unity is full scale, half of unity is half
    // scale (2^31, rounded), and a boost saturates.
    assert_eq!(
        decode(Ids::kVolumeTypeID, note_id, 0.25),
        per_note(controller(RegisteredPerNoteController::Volume, u32::MAX))
    );
    assert_eq!(
        decode(Ids::kVolumeTypeID, note_id, 0.125),
        per_note(controller(RegisteredPerNoteController::Volume, 0x8000_0000))
    );
    assert_eq!(
        decode(Ids::kVolumeTypeID, note_id, 1.0),
        per_note(controller(RegisteredPerNoteController::Volume, u32::MAX))
    );
    assert_eq!(
        decode(Ids::kPanTypeID, note_id, 1.0),
        per_note(controller(RegisteredPerNoteController::Pan, u32::MAX))
    );
    assert_eq!(
        decode(Ids::kBrightnessTypeID, note_id, 0.0),
        per_note(controller(RegisteredPerNoteController::Brightness, 0))
    );
    assert_eq!(
        decode(Ids::kTuningTypeID, note_id, 0.5 + 12.0 / 240.0),
        per_note(controller(RegisteredPerNoteController::Pitch, 72 << 25))
    );
    assert_eq!(
        decode(Ids::kTuningTypeID, note_id, 0.5 + 80.0 / 240.0),
        None,
        "key 60 + 80 semitones is past Pitch 7.25's range"
    );
    assert_eq!(decode(Ids::kVibratoTypeID, note_id, 0.5), None);
    assert_eq!(decode(Ids::kExpressionTypeID, note_id, 0.5), None);
    assert_eq!(decode(Ids::kCustomStart, note_id, 0.5), None);
    assert_eq!(
        decode(Ids::kVolumeTypeID, 99_999, 0.25),
        None,
        "a plugin's own id"
    );
    assert_eq!(decode(Ids::kVolumeTypeID, -1, 0.5), None, "the wildcard");
    assert_eq!(decode(Ids::kVolumeTypeID, note_id, f64::NAN), None);
    // Each of the seven drops above is one lost event: the tuning past its
    // range, vibrato, expression, a custom id, a plugin's own id, the wildcard
    // and a value that is not a number.
    assert_eq!(stream.expressions_dropped(), 7);
    assert_eq!(stream.lost_total(), 7);
}

/// **Events with no UMP form are counted lost**: bytes that are no MIDI
/// message, a Data event too short to be one, a SysEx too long for one packet
/// or with a payload byte past 127, a controller number with no message (Cable
/// Select 135) and the dimensions the host does not translate (an expression's
/// text, a chord). A message the translator holds in a run is not a loss.
#[test]
fn events_with_no_ump_form_are_counted_lost() {
    let mut stream = OutputStream::new();
    let data = |bytes: &[u8]| {
        Event::at(
            0,
            Payload::Data(Data::new(0, bytes.iter().copied()).unwrap()),
        )
    };
    // A lone data byte, then bytes whose status is not a message.
    for bytes in [&[0x90][..], &[0x90, 60][..], &[0x00, 1, 2][..]] {
        assert!(data(bytes).to_midi(&mut stream).is_none(), "{bytes:?}");
    }
    assert_eq!(stream.untranslated_dropped(), 3);
    // A SysEx of two packets, and one with an 8-bit payload byte.
    assert!(
        data(&[0xF0, 1, 2, 3, 4, 5, 6, 7, 0xF7])
            .to_midi(&mut stream)
            .is_none()
    );
    assert!(data(&[0xF0, 1, 0x80, 0xF7]).to_midi(&mut stream).is_none());
    assert_eq!(stream.sysex_dropped(), 2);
    // Cable Select (135) has no message, and an expression's integer value has
    // no UMP form.
    let cable_select = Event::at(0, Payload::LegacyCcOut(legacy_cc(135, 0, 0, 0)));
    assert!(cable_select.to_midi(&mut stream).is_none());
    let int_value = Event::at(0, Payload::ExpressionInt(ExpressionInt::new(3, 7, 1)));
    assert!(int_value.to_midi(&mut stream).is_none());
    assert_eq!(stream.untranslated_dropped(), 5);
    assert_eq!(stream.lost_total(), 7);
    // A message the translator takes into a run is held, not lost.
    let select = Event::at(0, Payload::LegacyCcOut(legacy_cc(101, 0, 0, 0)));
    assert!(select.to_midi(&mut stream).is_none());
    assert_eq!(stream.lost_total(), 7, "an RPN select is held, not lost");
}

/// **A parameter run assembles across blocks**: the Data Entry LSB that ends a
/// Registered Parameter run arrives in a later drain than the select and the
/// MSB (M2-104 Appendix D.3.3), and the run is one controller (pitch bend
/// sensitivity of 12 semitones, zero-extended, M2-115 section 4.1).
#[test]
fn a_plugins_parameter_run_assembles_across_blocks() {
    let drain = |translator: &mut OutputStream, controls: &[(u8, i8)]| {
        let list = EventList::input();
        let mut staging = list.begin();
        for &(control, value) in controls {
            let event = Event::at(0, Payload::LegacyCcOut(legacy_cc(control, 0, value, 0)));
            assert!(staging.push(event));
        }
        staging.finish();
        let mut out = tutti_plugin_types::RtMidiEvents::new();
        drain_midi(&list, translator, &mut out);
        out.iter().map(|m| m.ump.decode()).collect::<Vec<_>>()
    };
    let mut translator = OutputStream::new();
    assert!(drain(&mut translator, &[(101, 0), (100, 0), (6, 12)]).is_empty());
    let got = drain(&mut translator, &[(38, 0)]);
    let [Ok(Message::Midi2(_, Voice2::Registered { bank, index, value }))] = got[..] else {
        panic!("expected one Registered Controller, got {got:?}");
    };
    assert_eq!((bank.get(), index.get(), value.get()), (0, 0, 12 << 25));
}

/// **A plugin's expression on a note it started under its own id is that note's
/// message**: the id table is fed by the plugin's NoteOn and freed by its NoteOff
/// (by id, or by channel and pitch when the off names no id).
#[test]
fn a_plugins_expression_on_its_own_note_id_is_translated() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let mut stream = OutputStream::new();
    let own_id = 9_000;
    let pan = |id| {
        Event::at(
            0,
            Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, id, 1.0)),
        )
    };
    let note_on = Event::at(0, Payload::NoteOn(NoteOn::new(3, 62, 0.5, own_id)));
    assert!(
        pan(own_id).to_midi(&mut stream).is_none(),
        "no such note yet"
    );
    assert!(note_on.to_midi(&mut stream).is_some());
    let got = pan(own_id)
        .to_midi(&mut stream)
        .expect("the plugin's own note");
    assert_eq!(
        got.ump.decode(),
        Ok(Message::Midi2(
            Address {
                group: MidiGroup::FIRST,
                channel: MidiChannel::of(3)
            },
            Voice2::RegisteredPerNote {
                note: NoteNumber::of(62),
                controller: RegisteredPerNoteController::Pan,
                value: ControlValue::new(u32::MAX),
            },
        ))
    );
    // Ended by channel and pitch: the entry stays for the release.
    let off = Event::at(0, Payload::NoteOff(NoteOff::new(3, 62, 0.0, -1)));
    assert!(off.to_midi(&mut stream).is_some());
    assert!(pan(own_id).to_midi(&mut stream).is_some());
}

/// What a note's volume expressions are after `events` (a Note On on `note` 60
/// first), by `kVolumeTypeID` (0).
fn volumes_after(follow: Voice2) -> Vec<f64> {
    let mut encoder = MidiEncoder::default();
    let on = Message::Midi2(
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::of(0),
        },
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::CENTER),
    );
    encoder.encode(&(Offset::ZERO, Ump::from(on)));
    encoder
        .encode(&(
            Offset::ZERO,
            Ump::from(Message::Midi2(
                Address {
                    group: MidiGroup::FIRST,
                    channel: MidiChannel::of(0),
                },
                follow,
            )),
        ))
        .iter()
        .filter_map(|event| match event.payload {
            Payload::ExpressionValue(e) if e.typeId == 0 => Some(e.value),
            _ => None,
        })
        .collect()
}

/// **A Per-Note volume stages on VST3's scale**: `kVolumeTypeID` is `0` for
/// silence, `0.25` for 0 dB and `1` for +12 dB (`ivstnoteexpression.h`), and a
/// per-note controller's full scale is unity, so full scale is `0.25` and the
/// centre word `0x8000_0000` a quarter of its position.
#[test]
fn a_full_scale_per_note_volume_stages_at_vst3_unity() {
    let volume = |value: u32| Voice2::AssignablePerNote {
        note: NoteNumber::of(60),
        controller: 7,
        value: ControlValue::new(value),
    };
    assert_eq!(volumes_after(volume(u32::MAX)), [0.25]);
    let half = volumes_after(volume(0x8000_0000));
    assert_eq!(
        half,
        [f64::from(0x8000_0000_u32) / f64::from(u32::MAX) / 4.0]
    );
    assert_eq!(volumes_after(volume(0)), [0.0]);
}

/// **A Per-Note Management Reset puts a note's volume back to unity**, which on
/// VST3's scale is `0.25`, not `1` (+12 dB).
#[test]
fn a_per_note_reset_sends_volume_at_vst3_unity() {
    let reset = Voice2::PerNoteManagement {
        note: NoteNumber::of(60),
        management: PerNoteManagement::Reset,
    };
    assert_eq!(volumes_after(reset), [0.25]);
}

/// **Every event of the plugin's output reaches the stream, whether or not the
/// block's list has room**: a run's Data Entry LSB and a note's start that
/// arrive after the list is full are read, so the run is one controller in the
/// block after and the note's expression is that note's.
#[test]
fn a_run_and_a_note_through_a_full_list_are_still_read() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let drain = |stream: &mut OutputStream, events: Vec<Event>| {
        let list = EventList::input();
        let mut staging = list.begin();
        for event in events {
            assert!(staging.push(event));
        }
        staging.finish();
        let mut out = tutti_plugin_types::RtMidiEvents::new();
        drain_midi(&list, stream, &mut out);
        (
            out.iter().map(|m| m.ump.decode()).collect::<Vec<_>>(),
            out.is_overflowed(),
        )
    };
    let cc = |control: u8, value: i8| {
        Event::at(0, Payload::LegacyCcOut(legacy_cc(control, 0, value, 0)))
    };
    let mut stream = OutputStream::new();
    // Block 1: the list fills with notes, then a run and an own-id note arrive.
    let mut first: Vec<Event> = (0..tutti_plugin_types::RT_MIDI_CAPACITY)
        .map(|_| Event::at(0, Payload::NoteOn(NoteOn::new(0, 60, 0.5, -1))))
        .collect();
    first.extend([cc(101, 0), cc(100, 0), cc(6, 12), cc(38, 0)]);
    first.push(Event::at(
        0,
        Payload::NoteOn(NoteOn::new(3, 62, 0.5, 9_000)),
    ));
    let (got, overflowed) = drain(&mut stream, first);
    assert!(overflowed);
    assert_eq!(
        got.len(),
        tutti_plugin_types::RT_MIDI_CAPACITY,
        "no room for the rest"
    );
    // Block 2: RPN 0/0 is still selected, so one more value is one controller,
    // and the note under id 9000 takes an expression.
    let pan = Event::at(
        0,
        Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, 9_000, 1.0)),
    );
    let (got, _) = drain(&mut stream, vec![cc(6, 7), cc(38, 0), pan]);
    let [
        Ok(Message::Midi2(_, Voice2::Registered { value, .. })),
        Ok(Message::Midi2(
            address,
            Voice2::RegisteredPerNote {
                note, controller, ..
            },
        )),
    ] = got[..]
    else {
        panic!("expected a controller and an expression, got {got:?}");
    };
    assert_eq!(value.get(), 7 << 25);
    assert_eq!(
        (address.channel.get(), note.get(), controller),
        (3, 62, RegisteredPerNoteController::Pan)
    );
}

/// **An id the plugin's output has not started names no note**: a low own id is
/// not read as the host's `channel * 128 + note` (id 5 is not channel 0 key 5),
/// whether never started or started with id `-1`; it is dropped and counted.
#[test]
fn an_expression_on_an_id_no_output_note_started_is_dropped_and_counted() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let mut stream = OutputStream::new();
    let pan = |id| {
        Event::at(
            0,
            Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, id, 1.0)),
        )
    };
    assert!(pan(5).to_midi(&mut stream).is_none(), "never started");
    let anonymous = Event::at(0, Payload::NoteOn(NoteOn::new(2, 40, 0.5, -1)));
    assert!(anonymous.to_midi(&mut stream).is_some());
    assert!(
        pan(5).to_midi(&mut stream).is_none(),
        "a note on with no id"
    );
    let on = Event::at(0, Payload::NoteOn(NoteOn::new(2, 40, 0.5, 5)));
    assert!(on.to_midi(&mut stream).is_some());
    assert!(pan(5).to_midi(&mut stream).is_some(), "while it sounds");
    assert_eq!(stream.expressions_dropped(), 2);
    let not_a_number = Event::at(
        0,
        Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, 5, f64::NAN)),
    );
    assert!(not_a_number.to_midi(&mut stream).is_none());
    assert_eq!(stream.expressions_dropped(), 3, "a NaN is counted too");
}

/// **A voice in its release keeps its expressions**: the plugin's note off ends
/// the note, but an expression on its id after it is still that note's.
#[test]
fn a_release_phase_expression_after_the_note_off_is_translated() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let mut stream = OutputStream::new();
    let on = Event::at(0, Payload::NoteOn(NoteOn::new(2, 40, 0.5, 9_000)));
    assert!(on.to_midi(&mut stream).is_some());
    let off = Event::at(0, Payload::NoteOff(NoteOff::new(2, 40, 0.0, 9_000)));
    assert!(off.to_midi(&mut stream).is_some());
    let pan = Event::at(
        0,
        Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, 9_000, 1.0)),
    );
    let got = pan
        .to_midi(&mut stream)
        .expect("the release is that note's");
    assert!(matches!(
        got.ump.decode(),
        Ok(Message::Midi2(a, Voice2::RegisteredPerNote { note, .. }))
            if a.channel == MidiChannel::of(2) && note == NoteNumber::of(40)
    ));
    assert_eq!(stream.expressions_dropped(), 0);
}

/// **A full table gives up an ended note before a live one**, however old: a
/// release that has run its course does not cost a sounding note its id.
#[test]
fn a_full_table_evicts_an_ended_note_before_the_oldest_live_one() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let mut stream = OutputStream::new();
    for id in 0..256 {
        let on = Event::at(0, Payload::NoteOn(NoteOn::new(0, 60, 0.5, 10_000 + id)));
        let _ = on.to_midi(&mut stream);
    }
    // The newest but one ends; the oldest stays live.
    let off = Event::at(0, Payload::NoteOff(NoteOff::new(0, 60, 0.0, 10_254)));
    let _ = off.to_midi(&mut stream);
    let extra = Event::at(0, Payload::NoteOn(NoteOn::new(0, 61, 0.5, 20_000)));
    let _ = extra.to_midi(&mut stream);
    assert_eq!(stream.notes_evicted(), 1);
    assert_eq!(
        stream.lost_total(),
        0,
        "an eviction loses a mapping, not an event"
    );
    let pan = |id| {
        Event::at(
            0,
            Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, id, 1.0)),
        )
    };
    assert!(
        pan(10_254).to_midi(&mut stream).is_none(),
        "the ended note went"
    );
    assert!(
        pan(10_000).to_midi(&mut stream).is_some(),
        "the oldest live stayed"
    );
}

/// **A full table of live notes evicts its oldest** for a new one, and counts
/// it: stale entries (a note off the plugin never sent) do not pin the table.
#[test]
fn a_full_note_table_evicts_the_oldest() {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;

    let mut stream = OutputStream::new();
    for id in 0..257 {
        let on = Event::at(0, Payload::NoteOn(NoteOn::new(0, 60, 0.5, 10_000 + id)));
        let _ = on.to_midi(&mut stream);
    }
    assert_eq!(stream.notes_evicted(), 1);
    let pan = |id| {
        Event::at(
            0,
            Payload::ExpressionValue(ExpressionValue::new(Ids::kPanTypeID, id, 1.0)),
        )
    };
    assert!(
        pan(10_000).to_midi(&mut stream).is_none(),
        "the oldest went"
    );
    assert!(
        pan(10_256).to_midi(&mut stream).is_some(),
        "the newest is held"
    );
}
