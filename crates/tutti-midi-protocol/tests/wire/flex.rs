//! Flex Set Tempo and Set Time Signature as unit values (M2-104 sections
//! 7.5.3 and 7.5.4).

use tutti_midi_protocol::{flex_metronome, flex_time_signature, flex_time_signature_wire};
use tutti_music::TimeSignature;
use tutti_ump::FlexTimeSignature as Wire;
use tutti_ump::{BarAccents, FlexTempo};
use tutti_units::Bpm;

fn wire(numerator: u8, denominator: u8) -> Wire {
    Wire {
        numerator,
        denominator,
        thirty_seconds: 8,
    }
}

/// Section 7.5.3: the field is 10 ns units per quarter note, so 120 bpm (0.5 s)
/// is 50,000,000 and 60 bpm (one second a quarter) is 100,000,000.
/// Spec: m2-104/msg/d/00.00
#[test]
fn a_tempo_is_ten_ns_units_per_quarter() {
    assert_eq!(FlexTempo::new(50_000_000).bpm(), Bpm::new(120.0));
    assert_eq!(FlexTempo::new(100_000_000).bpm(), Bpm::new(60.0));
}

/// The slowest the field holds is 1.39698 bpm (section 7.5.3); a field of 0 is
/// no tempo; the fastest, 1, is 6e9 bpm.
#[test]
fn the_field_has_a_slowest_tempo_and_zero_is_none() {
    let slowest = FlexTempo::new(u32::MAX).bpm().unwrap().get();
    assert!((slowest - 1.39698).abs() < 1e-4, "{slowest}");
    assert_eq!(FlexTempo::new(0).bpm(), None);
    assert_eq!(FlexTempo::new(1).bpm(), Bpm::new(6.0e9));
}

/// Section 7.5.4: the denominator is a negative power of 2 (2 is a
/// quarter note, 3 an eighth note).
/// Spec: m2-104/msg/d/00.01
#[test]
fn the_denominator_is_a_power_of_two_exponent() {
    assert_eq!(
        flex_time_signature(wire(4, 2)),
        Some(TimeSignature::of(4, 4))
    );
    assert_eq!(
        flex_time_signature(wire(7, 3)),
        Some(TimeSignature::of(7, 8))
    );
    assert_eq!(
        flex_time_signature(wire(3, 1)),
        Some(TimeSignature::of(3, 2))
    );
    assert_eq!(
        flex_time_signature(wire(6, 6)),
        Some(TimeSignature::of(6, 64))
    );
}

/// What the meter cannot hold is refused, never clamped: a zero denominator is
/// non-standard (7.5.4), a numerator of 0 is outside 1 to 256, and the meter's
/// own ceilings (64 beats, a 64th note) apply.
#[test]
fn what_the_meter_cannot_hold_is_refused() {
    for (n, d) in [(4, 0), (0, 2), (65, 2), (4, 7), (4, 31), (4, 32), (4, 255)] {
        assert_eq!(flex_time_signature(wire(n, d)), None, "{n}/{d}");
    }
    assert_eq!(
        flex_time_signature(wire(64, 2)),
        Some(TimeSignature::of(64, 4))
    );
}

/// Section 7.5.3: the field written for a tempo is its beat period in 10 ns units, 120 bpm
/// 50,000,000 and 60 bpm 100,000,000, and `FlexTempo::bpm` reads it back; the
/// slowest and fastest the field holds are its two ends.
#[test]
fn a_tempo_writes_its_ten_ns_units_per_quarter() {
    assert_eq!(FlexTempo::from_bpm(Bpm::of(120.0)).get(), 50_000_000);
    assert_eq!(FlexTempo::from_bpm(Bpm::of(60.0)).get(), 100_000_000);
    assert_eq!(FlexTempo::from_bpm(Bpm::of(1.0)).get(), u32::MAX);
    assert_eq!(FlexTempo::from_bpm(Bpm::of(6.0e9)).get(), 1);
    assert_eq!(FlexTempo::from_bpm(Bpm::of(1.0e12)).get(), 1);
    for bpm in [20.0, 93.7, 128.0, 300.0] {
        let bpm = Bpm::of(bpm);
        let back = FlexTempo::from_bpm(bpm).bpm().unwrap().get();
        assert!((back - bpm.get()).abs() < 1e-6, "{bpm:?} -> {back}");
        // The tempo read back writes the same field: within one field step.
        assert_eq!(
            FlexTempo::from_bpm(Bpm::of(back)).get(),
            FlexTempo::from_bpm(bpm).get()
        );
    }
}

/// **The tempo field is read at the resolution of one 10 ns unit.** Section
/// 7.5.3 leaves a sequencer to choose how many bits of the field it supports;
/// this one supports all 32. The field is 10 ns units per quarter note, so a
/// quarter of 5 s (500,000,000 units) is 60 / 5 = 12 bpm, 1 s (100,000,000) is
/// 60 bpm, 0.5 s (50,000,000) is 120 bpm and one unit (10 ns) is
/// 60 / 10e-9 = 6e9 bpm; and a field one unit above any of them is a slower
/// tempo, so no low bit of the field is discarded.
/// Spec: m2-104/7.5.3#7, m2-116/7.1.1#5
#[test]
fn the_tempo_field_is_read_to_the_unit() {
    for (field, bpm) in [
        (500_000_000u32, 12.0),
        (100_000_000, 60.0),
        (50_000_000, 120.0),
        (1, 6.0e9),
    ] {
        assert_eq!(FlexTempo::new(field).bpm(), Bpm::new(bpm), "{field}");
        let slower = FlexTempo::new(field + 1).bpm().unwrap().get();
        assert!(slower < bpm, "{field}: {slower}");
    }
}

/// Section 7.5.4: 7/8 writes as numerator 7, denominator exponent 3 (an eighth
/// note) and 8 thirty-seconds in 24 clocks; every signature but a whole-note
/// denominator reads back as itself.
/// Spec: m2-104/msg/d/00.01
#[test]
fn a_time_signature_writes_its_numerator_and_exponent() {
    let wire = flex_time_signature_wire(TimeSignature::of(7, 8)).unwrap();
    assert_eq!(
        (wire.numerator, wire.denominator, wire.thirty_seconds),
        (7, 3, 8)
    );
    for beats in 1..=64 {
        for exponent in 1..=6 {
            let signature = TimeSignature::of(beats, 1 << exponent);
            let wire = flex_time_signature_wire(signature).unwrap();
            assert_eq!(u32::from(wire.numerator), beats);
            assert_eq!(u32::from(wire.denominator), exponent);
            assert_eq!(flex_time_signature(wire), Some(signature));
        }
    }
    // A whole-note denominator would be exponent 0, non-standard: no message.
    assert_eq!(flex_time_signature_wire(TimeSignature::of(4, 1)), None);
}

/// Section 7.5.5: a click on every beat is `4 * 24 / note value` clocks (a
/// quarter 24, an eighth 12, a half 48, a whole 96); the bar is undivided, its
/// beat count in Part 1 and Parts 2 and 3 zero, so the parts add up to the
/// bar's beats; a 64th note's beat (1.5 clocks) has no integer field.
/// Spec: m2-104/7.5.5#5, m2-104/7.5.5#9
#[test]
fn a_metronome_clicks_each_beat_with_an_undivided_bar() {
    for (beats, value, clocks) in [(4, 4, 24), (6, 8, 12), (3, 2, 48), (1, 1, 96), (5, 16, 6)] {
        let m = flex_metronome(TimeSignature::of(beats, value)).unwrap();
        assert_eq!(u32::from(m.clocks_per_click), clocks, "{beats}/{value}");
        assert_eq!(m.accents.parts(), [beats as u8, 0, 0]);
        assert!(matches!(m.accents, BarAccents::Undivided(_)));
        assert_eq!(u32::from(m.accents.beats()), beats);
        assert_eq!((m.subdivision_1, m.subdivision_2), (0, 0));
    }
    assert_eq!(flex_metronome(TimeSignature::of(4, 64)), None);
}
