use super::*;
use tutti_measure::{Note, PitchClass, Region};
use tutti_tempo::TempoMap;
use tutti_ump::{MidiChannel, MidiGroup, RegisteredData};
use tutti_units::{Bpm, Cents, Db, SampleRate};

const SECOND: usize = 48_000;

fn map() -> TempoMap {
    TempoMap::constant(Bpm::of(120.0)).expect("120 BPM is a tempo")
}

fn note(cents: f64, start: usize, end: usize, level: f64) -> NoteEvent {
    NoteEvent {
        note: Note::C4,
        cents: Cents::new(cents),
        span: Region::new(Samples::new(start), Samples::new(end))
            .expect("a note ends after it starts"),
        velocity: Db::new(level),
    }
}

fn fixed() -> NoteMapping {
    NoteMapping::new(LevelVelocity::Fixed(NoteVelocity::CENTER), NoteCents::Drop)
}

/// A bend range of `cents` cents (under 100) through the Channel Pitch Bend
/// Sensitivity data, whose LSB is cents.
fn range(cents: u8) -> PitchBendSensitivity {
    PitchBendSensitivity::from_channel_data(RegisteredData::new(U7::of(0), U7::of(cents)))
}

fn voice(e: &TimedMidiEvent) -> (Address, Voice2) {
    match Message::try_from(e.event()).expect("a message") {
        Message::Midi2(address, voice) => (address, voice),
        other => panic!("not a MIDI 2.0 voice message: {other:?}"),
    }
}

fn events(notes: &[NoteEvent], mapping: NoteMapping) -> Vec<TimedMidiEvent> {
    let map = map();
    note_events(notes, &mut map.view(SampleRate::SR_48K), mapping)
}

#[test]
fn a_note_at_one_second_at_120_bpm_lands_on_beat_two() {
    let e = events(&[note(0.0, SECOND, 2 * SECOND, -10.0)], fixed());
    assert!(matches!(voice(&e[0]).1, Voice2::NoteOn { .. }));
    assert_eq!(e[0].beat(), Beat::new(2.0));
}

#[test]
fn a_note_off_lands_at_the_notes_end() {
    let e = events(&[note(0.0, SECOND, 3 * SECOND, -10.0)], fixed());
    assert_eq!(e.len(), 2);
    assert!(matches!(voice(&e[1]).1, Voice2::NoteOff { .. }));
    assert_eq!(e[1].beat(), Beat::new(6.0));
}

#[test]
fn a_note_is_sent_on_the_mappings_address_and_number() {
    let at = Address::new(MidiGroup::numbered(3).unwrap(), MidiChannel::of(5));
    let e = events(&[note(0.0, 0, SECOND, -10.0)], fixed().on(at));
    for event in &e {
        assert_eq!(voice(event).0, at);
    }
    let Voice2::NoteOn { note, .. } = voice(&e[0]).1 else {
        panic!("first is a Note On");
    };
    assert_eq!(note.get(), 60, "C4 is MIDI note 60");
}

#[test]
fn a_fixed_velocity_is_sent_whatever_the_level() {
    let mapping = NoteMapping::new(
        LevelVelocity::Fixed(NoteVelocity::new(0x1234)),
        NoteCents::Drop,
    );
    for level in [-90.0, -30.0, 0.0] {
        let e = events(&[note(0.0, 0, SECOND, level)], mapping);
        let Voice2::NoteOn { velocity, .. } = voice(&e[0]).1 else {
            panic!("first is a Note On");
        };
        assert_eq!(velocity, NoteVelocity::new(0x1234));
    }
}

#[test]
fn the_level_maps_floor_is_the_minimum_velocity_and_its_ceiling_the_maximum() {
    let mapping = NoteMapping::new(
        LevelVelocity::ByLevel(VelocityRange::default()),
        NoteCents::Drop,
    );
    let on = |level: f64| {
        let e = events(&[note(0.0, 0, SECOND, level)], mapping);
        let Voice2::NoteOn { velocity, .. } = voice(&e[0]).1 else {
            panic!("first is a Note On");
        };
        velocity
    };
    // MIDI 1.0 velocity 1 widens to `1 << 9` and 127 to the top (M2-115 section 3).
    assert_eq!(on(-60.0), NoteVelocity::new(0x0200));
    assert_eq!(
        on(-80.0),
        NoteVelocity::new(0x0200),
        "below the floor is the floor"
    );
    assert_eq!(on(-3.0), NoteVelocity::MAX);
    assert_eq!(
        on(0.0),
        NoteVelocity::MAX,
        "above the ceiling is the ceiling"
    );
}

fn bend_of(cents: f64, range: PitchBendSensitivity) -> BendValue {
    let mapping = NoteMapping::new(
        LevelVelocity::Fixed(NoteVelocity::CENTER),
        NoteCents::PerNotePitchBend(range),
    );
    events(&[note(cents, 0, SECOND, -10.0)], mapping)
        .iter()
        .find_map(|e| match voice(e).1 {
            Voice2::PerNotePitchBend { value, .. } => Some(value),
            _ => None,
        })
        .expect("a bend")
}

#[test]
fn plus_25_cents_over_a_50_cent_range_is_a_quarter_of_the_bend_span_above_centre() {
    // Half the up range, and the up range is half of the 32-bit span.
    assert_eq!(bend_of(25.0, range(50)).get(), 0x8000_0000 + (1 << 30));
    assert_eq!(bend_of(-25.0, range(50)).get(), 0x8000_0000 - (1 << 30));
    assert_eq!(bend_of(0.0, range(50)), BendValue::CENTER);
}

#[test]
fn cents_past_the_range_saturate_at_its_ends() {
    assert_eq!(bend_of(50.0, range(25)), BendValue::MAX);
    assert_eq!(bend_of(-50.0, range(25)), BendValue::MIN);
}

#[test]
fn a_range_of_nothing_bends_nothing() {
    assert_eq!(bend_of(30.0, range(0)), BendValue::CENTER);
}

#[test]
fn a_bend_precedes_its_note_on_at_the_same_beat_and_the_range_is_sent_once() {
    let mapping = NoteMapping::new(
        LevelVelocity::Fixed(NoteVelocity::CENTER),
        NoteCents::PerNotePitchBend(range(50)),
    );
    let e = events(
        &[
            note(10.0, SECOND, 2 * SECOND, -10.0),
            note(-10.0, 3 * SECOND, 4 * SECOND, -10.0),
        ],
        mapping,
    );
    let kinds: Vec<_> = e
        .iter()
        .map(|e| match voice(e).1 {
            Voice2::Registered { value, .. } => format!("rpn {:#x}", value.get()),
            Voice2::PerNotePitchBend { .. } => "bend".into(),
            Voice2::NoteOn { .. } => "on".into(),
            Voice2::NoteOff { .. } => "off".into(),
            other => panic!("{other:?}"),
        })
        .collect();
    // The sensitivity leads, at the first note's beat, ahead of the bend it scales.
    assert_eq!(
        kinds,
        ["rpn 0x1000000", "bend", "on", "off", "bend", "on", "off"]
    );
    assert_eq!(e[0].beat(), e[1].beat());
}

#[test]
fn dropped_cents_send_no_bend_and_no_range() {
    let e = events(&[note(40.0, 0, SECOND, -10.0)], fixed());
    assert_eq!(e.len(), 2);
    assert!(
        e.iter()
            .all(|e| matches!(voice(e).1, Voice2::NoteOn { .. } | Voice2::NoteOff { .. }))
    );
}

#[test]
fn a_chord_span_is_one_change_at_its_start_with_its_root_and_quality() {
    let spans = [
        ChordSpan {
            root: PitchClass::A,
            quality: ChordQuality::Minor,
            span: Region::new(Samples::new(SECOND), Samples::new(2 * SECOND)).expect("ascending"),
        },
        ChordSpan {
            root: PitchClass::C,
            quality: ChordQuality::Major,
            span: Region::new(Samples::new(2 * SECOND), Samples::new(3 * SECOND))
                .expect("ascending"),
        },
    ];
    let map = map();
    let changes = harmony_from_chords(&spans, &mut map.view(SampleRate::SR_48K))
        .expect("spans at distinct beats");
    let [a, c] = changes.as_slice() else {
        panic!("one change per span: {changes:?}");
    };
    assert_eq!(a.beat, Beat::new(2.0));
    // A minor: root 69 (A4), root, minor third and fifth.
    assert_eq!(a.harmony, Harmony::chord(69, 69, 0b1000_1001));
    assert_eq!(c.beat, Beat::new(4.0));
    assert_eq!(c.harmony, Harmony::chord(60, 60, 0b1001_0001));
}

#[test]
fn no_input_gives_no_events() {
    let map = map();
    let mut view = map.view(SampleRate::SR_48K);
    let bent = NoteMapping::new(
        LevelVelocity::Fixed(NoteVelocity::CENTER),
        NoteCents::PerNotePitchBend(range(50)),
    );
    assert!(clip_from_notes(&[], &mut view, bent).is_empty());
    assert!(harmony_from_chords(&[], &mut view).unwrap().is_empty());
}
