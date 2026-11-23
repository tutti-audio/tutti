use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `CentsSpan + CentsSpan`, `CentsSpan - CentsSpan` — a difference of two
///   distances can be negative, which is outside the range. Add or subtract as
///   offsets, `to_cents()` and arithmetic on `Cents`, then `CentsSpan::new` the
///   result.
/// - `CentsSpan * f64`, `-CentsSpan` — a negative factor leaves the range.
///   Scale the raw number and call `new` (or `clamped`, where saturating is
///   meant).
/// - `HoldFactor + HoldFactor`, `HoldFactor * f64` — a hold is a count of frames
///   read after rounding; a sum or a scaled factor can fall below 1. Build the
///   new factor from the raw number with `HoldFactor::new_clamped`.
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`). `CentsSpan` is range-checked and has
///   `From<CentsSpan> for f64` beside `TryFrom<f64>`, as `Bipolar` does.
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_conversions_are_documented() {}

/// The quotient of two offsets is the bare number of times one holds the other.
#[test]
fn cents_over_cents_is_the_number_of_times_one_holds_the_other() {
    assert_eq!(Cents(150.0) / Cents(300.0), 0.5);
    assert_eq!(Cents(-50.0) / Cents::SEMITONE, -0.5);
}

#[test]
fn cents_and_semitones_convert_rather_than_scale() {
    // `Cents` being `Scalable` means `Cents / 100.0` COMPILES and
    // yields `Cents` — wrong by 100x with a type that says otherwise.
    // These are the forms that carry the unit change.
    assert_eq!(Cents(100.0).to_semitones(), Semitones(1.0));
    assert_eq!(Cents::SEMITONE.to_semitones(), Semitones(1.0));
    assert_eq!(Semitones(1.0).to_cents(), Cents(100.0));
    assert_eq!(Semitones::OCTAVE.to_cents(), Cents(1200.0));

    // An octave doubles the frequency, by either spelling.
    assert_eq!(Semitones::OCTAVE.to_pitch_ratio(), 2.0);
    assert_eq!(Semitones::UNISON.to_pitch_ratio(), 1.0);
    assert_eq!(Cents(1200.0).to_pitch_ratio(), 2.0);
}

/// `Cents::from_pitch_ratio` is the named inverse of `to_pitch_ratio`.
///
/// It exists because `tuning.rs` was hand-rolling `1200.0 * ratio.log2()`
/// while the forward direction was already a converter — the asymmetry the
/// omission rule is meant to catch.
#[test]
fn a_pitch_ratio_round_trips_through_cents() {
    for ratio in [0.5_f64, 1.0, 1.5, 2.0, 3.0] {
        let back = Cents::from_pitch_ratio(fr(ratio)).to_pitch_ratio();
        assert!(
            (back - ratio).abs() <= ratio * f64::EPSILON * 4.0,
            "{ratio} round-tripped to {back}"
        );
    }
    assert_eq!(Cents::from_pitch_ratio(fr(2.0)), Cents(1200.0));
    assert_eq!(Cents::from_pitch_ratio(fr(1.0)), Cents(0.0));
    assert_eq!(Semitones::from_pitch_ratio(fr(2.0)), Semitones(12.0));
}

/// A ratio with no logarithm is not a `FrequencyRatio`, so the converters
/// never see one.
#[test]
fn a_ratio_without_a_logarithm_is_unwritable() {
    for raw in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(FrequencyRatio::new(raw), None, "{raw}");
    }
}

fn fr(raw: f64) -> FrequencyRatio {
    FrequencyRatio::new(raw).expect("a positive finite ratio")
}

/// The four ratios store `f64`: a figure past `f32` resolution survives construction, clamping
/// and `get`, and `Q::BUTTERWORTH` is the `f64` figure `1/sqrt(2)`.
#[test]
fn the_ratios_hold_full_f64_figures() {
    let fine = 1.0 + f64::EPSILON;
    assert_eq!(CompressionRatio::new_clamped(fine).get(), fine);
    assert_eq!(HoldFactor::new_clamped(fine).get(), fine);
    assert_eq!(Q::new_clamped(fine).get(), fine);
    assert_eq!(
        Resonance::new_clamped(1.0 - f64::EPSILON).get(),
        1.0 - f64::EPSILON
    );
    assert_eq!(Q::BUTTERWORTH.get(), core::f64::consts::FRAC_1_SQRT_2);
}

#[test]
fn the_three_ratios_have_disjoint_ranges() {
    // Which is why one `Ratio` could not serve all three: a legal value of
    // one is an illegal value of another.
    assert_eq!(CompressionRatio::new_clamped(0.5), CompressionRatio::UNITY);
    assert_eq!(CompressionRatio::new_clamped(4.0), CompressionRatio(4.0));

    assert_eq!(Resonance::new_clamped(4.0), Resonance::SELF_OSCILLATION);
    assert_eq!(Resonance::new_clamped(-1.0), Resonance::NONE);

    // `Q` only has to stay above zero — it divides into the damping term.
    assert!(Q::new_clamped(0.0).get() > 0.0);
    assert!(Q::new_clamped(-5.0).get() > 0.0);
    assert_eq!(Q::new_clamped(10.0), Q(10.0));

    // The sharp case: 1.0 means "mild bell" as a Q and "self-oscillating"
    // as a ladder resonance. Same number, different instrument.
    assert!(Q(1.0) > Q::BUTTERWORTH);
    assert_eq!(Resonance(1.0), Resonance::SELF_OSCILLATION);
}

/// A cents span is finite and not below zero: `new`, `TryFrom`, `of` and serde
/// agree, and NaN, the infinities and every negative are refused.
#[cfg(feature = "serde")]
#[test]
fn cents_span_keeps_the_range_contract() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: CentsSpan::new,
        get: CentsSpan::get,
        of: Some(CentsSpan::of),
        valid: &[0.0, 0.5, 12.0, 1200.0],
        invalid: &[-0.001, -50.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
    });
}

/// The one saturating constructor floors a negative at zero and maps NaN to
/// zero, and `Unit::from_raw` (a parameter cell's read) is it: a cell can hold
/// any float and a span never leaves its range.
#[test]
fn cents_span_saturates_only_by_name_and_through_a_cell() {
    assert_eq!(CentsSpan::clamped(-10.0), CentsSpan::ZERO);
    assert_eq!(CentsSpan::clamped(f64::NAN), CentsSpan::ZERO);
    assert_eq!(CentsSpan::clamped(f64::INFINITY).get(), f64::MAX);
    assert_eq!(<CentsSpan as Unit>::from_raw(-10.0), CentsSpan::ZERO);
    assert_eq!(<CentsSpan as Unit>::from_raw(12.0).get(), 12.0);
}

/// 100 cents is one semitone, by the span's own converter.
#[test]
fn cents_span_converts_to_semitones() {
    assert_eq!(CentsSpan::of(100.0).to_semitones(), Semitones(1.0));
}

#[test]
fn twelve_semitones_and_twelve_hundred_cents_are_exactly_an_octave() {
    assert_eq!(Semitones(12.0).to_pitch_ratio(), 2.0);
    assert_eq!(Cents(1200.0).to_pitch_ratio(), 2.0);
    assert_eq!(Semitones(-12.0).to_pitch_ratio(), 0.5);
}

#[test]
fn a_semitone_is_the_twelfth_root_of_two() {
    let want = 1.059_463_094_359_295_3_f64;
    assert!((Semitones(1.0).to_pitch_ratio() - want).abs() <= want * f64::EPSILON * 4.0);
    assert!((Cents(100.0).to_pitch_ratio() - want).abs() <= want * f64::EPSILON * 4.0);
}

/// A hold factor is at least 1: `new_clamped` raises a smaller value to `NONE`,
/// passes a larger one, fractional or not, and leaves NaN to the reader.
#[test]
fn a_hold_factor_is_at_least_one() {
    assert_eq!(HoldFactor::new_clamped(0.5), HoldFactor::NONE);
    assert_eq!(HoldFactor::new_clamped(-4.0), HoldFactor::NONE);
    assert_eq!(HoldFactor::new_clamped(2.6), HoldFactor::new(2.6));
    assert_eq!(HoldFactor::new_clamped(f64::INFINITY).get(), f64::INFINITY);
    assert!(HoldFactor::new_clamped(f64::NAN).get().is_nan());
    assert_eq!(HoldFactor::NONE.get(), 1.0);
}

/// A kernel that runs in `f32` narrows the converter's `f64` once; the narrowed ratio is the
/// `f32` closed form within 2 ulp.
#[test]
fn a_pitch_ratio_narrows_to_the_f32_closed_form() {
    for st in [-24.0_f32, -7.0, -1.0, 0.5, 3.0, 7.0, 19.0] {
        let by_semitones = Semitones(f64::from(st)).to_pitch_ratio() as f32;
        let by_cents = Cents(f64::from(st) * 100.0).to_pitch_ratio() as f32;
        let closed = 2.0_f32.powf(st / 12.0);
        for got in [by_semitones, by_cents] {
            assert!(
                (got - closed).abs() <= closed * f32::EPSILON * 2.0,
                "{st} st: {got} against {closed}"
            );
        }
    }
}

/// The inverse converters narrow to the `f32` closed form within 2 ulp.
#[test]
fn a_pitch_interval_narrows_to_the_f32_closed_form() {
    for ratio in [0.25_f32, 0.5, 1.5, 2.0, 3.0, 7.0] {
        let st = Semitones::from_pitch_ratio(fr(f64::from(ratio))).0 as f32;
        let cents = Cents::from_pitch_ratio(fr(f64::from(ratio))).0 as f32;
        let closed = 12.0 * ratio.log2();
        assert!((st - closed).abs() <= closed.abs().max(1.0) * f32::EPSILON * 2.0);
        assert!(
            (cents - closed * 100.0).abs() <= (closed * 100.0).abs().max(1.0) * f32::EPSILON * 2.0
        );
    }
}
