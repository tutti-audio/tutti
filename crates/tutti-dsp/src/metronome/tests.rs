use tutti_music::{BeatsPerBar, MeterChange, NoteValue};

use super::*;

const RATE: SampleRate = SampleRate::SR_48K;

fn prepared() -> Metronome<f64> {
    let mut metronome = Metronome::new();
    metronome.prepare(RATE);
    metronome
}

/// One frame at `beat` under `meter`.
fn at(metronome: &mut Metronome<f64>, meter: Option<&MeterMap>, beat: f64) {
    metronome.begin_block();
    let _ = metronome.tick(meter, Beat::new(beat), 1.0);
}

/// The accent is the bar's downbeat, taken from the meter, not a fixed every-N count, which
/// would ignore the project's time signature.
#[test]
fn the_accent_follows_the_meter_downbeat() {
    let mut metronome = prepared();
    let mut accents = |meter: Option<&MeterMap>, beat: f64| {
        metronome.silence();
        at(&mut metronome, meter, beat);
        metronome.is_accent()
    };
    // 4/4, by a map and by none: an accent every 4 quarter notes.
    let four = MeterMap::default();
    for meter in [Some(&four), None] {
        assert!(accents(meter, 0.0));
        assert!(!accents(meter, 1.0));
        assert!(!accents(meter, 3.0));
        assert!(accents(meter, 4.0));
    }
    // 3/4: every 3 quarters, where a fixed count of 4 would drift against the bar.
    let three = MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::new(BeatsPerBar::of(3), NoteValue::QUARTER),
    )]);
    assert!(accents(Some(&three), 3.0));
    assert!(!accents(Some(&three), 4.0));
    assert!(accents(Some(&three), 6.0));
}

/// In 7/8 the notated beat is an eighth, so a bar holds seven clicks across 3.5 quarter
/// notes, and the next bar starts at 3.5.
#[test]
fn the_click_rate_follows_the_notated_beat() {
    let mut metronome = prepared();
    let map = MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
    )]);
    at(&mut metronome, Some(&map), 0.0);
    assert_eq!(metronome.onset(), Some(Beat::new(0.0)));
    assert!(metronome.is_accent());

    at(&mut metronome, Some(&map), 0.5);
    assert_eq!(metronome.onset(), Some(Beat::new(0.5)));
    assert!(!metronome.is_accent());

    at(&mut metronome, Some(&map), 3.5);
    assert!(metronome.is_accent());
}

/// Clicks stay distinct across a meter change. A running index scaled by the current
/// segment's `beat_length` would collide across a `MeterChange`; identifying the beat by its
/// onset position does not.
#[test]
fn clicks_stay_distinct_across_a_meter_change() {
    let mut metronome = prepared();
    // 7/8 (eighth-note beats) for three bars, then 4/4 (quarter-note beats).
    let map = MeterMap::new([
        MeterChange::new(
            Beat::new(0.0),
            TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
        ),
        MeterChange::new(Beat::new(10.5), TimeSignature::default()),
    ]);
    let mut onsets = Vec::new();
    let mut beat = 0.0;
    while beat < 14.5 {
        at(&mut metronome, Some(&map), beat);
        if let Some(onset) = metronome.onset()
            && onsets.last() != Some(&onset)
        {
            onsets.push(onset);
        }
        beat += if beat < 10.5 { 0.5 } else { 1.0 };
    }
    for pair in onsets.windows(2) {
        assert!(pair[1] > pair[0], "{:?} then {:?}", pair[0], pair[1]);
    }
    // 21 eighths before the change (0.0..10.5) and 4 quarters after.
    assert_eq!(onsets.len(), 25);
    assert!(onsets.contains(&Beat::new(10.5)));
}

/// Pre-roll sits at negative beats, so the accent comes from the meter and not from a cast
/// that would turn -1 into 4294967295.
#[test]
fn negative_beats_do_not_wrap() {
    let mut metronome = prepared();
    at(&mut metronome, None, -4.0);
    assert!(
        metronome.is_accent(),
        "-4.0 in 4/4 is the downbeat of bar 0"
    );
    at(&mut metronome, None, -3.0);
    assert!(!metronome.is_accent(), "-3.0 is beat 2 of bar 0");
}

/// A loop wrap, a beat jumping back, retriggers on the beat it lands in.
#[test]
fn a_backward_jump_retriggers() {
    let mut metronome = prepared();
    at(&mut metronome, None, 7.0);
    assert_eq!(metronome.onset(), Some(Beat::new(7.0)));
    at(&mut metronome, None, 4.0);
    assert_eq!(metronome.onset(), Some(Beat::new(4.0)));
}

/// The click starts silent (`sin(0)` under a zero attack), sounds on its second frame at the
/// level it is read at, and runs out to silence after its 30 ms.
#[test]
fn a_click_starts_at_zero_scales_with_volume_and_runs_out() {
    let mut metronome = prepared();
    let first = metronome.tick(None, Beat::new(1.0), 0.5);
    let second = metronome.tick(None, Beat::new(1.0), 0.5);
    assert_eq!(first, 0.0);
    assert_eq!(second, metronome.click(false)[1] * 0.5);
    assert_ne!(second, 0.0);
    let len = metronome.click(false).len();
    assert_eq!(len, 1_440, "30 ms at 48 kHz");
    for _ in 2..len {
        let _ = metronome.tick(None, Beat::new(1.0), 0.5);
    }
    assert_eq!(metronome.tick(None, Beat::new(1.0), 0.5), 0.0);
}

/// The accent is 1200 Hz at full level and the normal click 1000 Hz at 0.7: the accent's
/// peak is the unit and the normal's is 0.7 of it.
#[test]
fn the_accent_is_full_level_and_the_normal_click_is_seven_tenths() {
    let metronome = prepared();
    let peak = |click: &[f64]| click.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!((peak(metronome.click(true)) - 1.0).abs() < 1e-2);
    assert!((peak(metronome.click(false)) - 0.7).abs() < 1e-2);
}
