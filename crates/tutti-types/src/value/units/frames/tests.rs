use super::*;
use crate::Param;

// The omission ledger. What is deliberately *absent* is as load-bearing as
// what is present: these must not compile, and are written here so a future
// contributor does not "helpfully" add them.
//
// - `SampleSpan - SampleSpan` — a difference of two spans can be negative,
//   which is outside the range. `checked_sub` refuses it and `saturating_sub`
//   floors it at zero.
// - `SampleSpan * f64`, `SampleSpan / f64`, `-SampleSpan` — a negative or NaN
//   factor leaves the range. `scaled` refuses it.
// - `SampleSpan * SampleSpan` — frames squared is not a quantity.
// - `SampleSpan + Samples`, `From<Samples> for SampleSpan` — the count is
//   exact only to 2^53 frames, and a bare `+` would add an integer count and a
//   fractional span unnamed. `SampleSpan::from_samples` is the one spelling.
// - `From<SampleSpan> for Samples` — the conversion has a rounding choice, so
//   it is `to_samples_floor` (an index) or `to_samples_ceil` (a buffer size).
// - `SamplePosition - SamplePosition -> SampleSpan` — the earlier minus the
//   later is negative, and an operator cannot refuse. `span_since` returns
//   `None` for it. (`SamplePosition - SamplePosition` stays a `SamplePosition`.)
// - `From<f64> for SampleSpan` — a bare float must not become a span unnamed
//   at the call site. Build with `new` (or `of`, `clamped`, `from_seconds`);
//   `From<SampleSpan> for f64` is the range-checked shape's, beside `TryFrom`.

/// A span is finite and not below zero: `new`, `TryFrom`, `of` and serde
/// agree, and NaN, the infinities and every negative are refused.
#[cfg(feature = "serde")]
#[test]
fn sample_span_keeps_the_range_contract() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: SampleSpan::new,
        get: SampleSpan::get,
        of: Some(SampleSpan::of),
        valid: &[0.0, 0.5, 504.0, 1.0e12],
        invalid: &[-0.001, -50.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
    });
}

#[test]
fn a_span_refuses_nan_infinity_and_negatives() {
    assert_eq!(SampleSpan::new(f64::NAN), None);
    assert_eq!(SampleSpan::new(f64::INFINITY), None);
    assert_eq!(SampleSpan::new(-f64::MIN_POSITIVE), None);
    assert_eq!(
        SampleSpan::new(f64::MAX).map(SampleSpan::get),
        Some(f64::MAX)
    );
    assert!(SampleSpan::try_from(-1.0).is_err());
}

#[test]
fn saturation_is_by_name_and_through_a_cell_only() {
    assert_eq!(SampleSpan::clamped(-10.0), SampleSpan::ZERO);
    assert_eq!(SampleSpan::clamped(f64::NAN), SampleSpan::ZERO);
    assert_eq!(SampleSpan::clamped(f64::INFINITY).get(), f64::MAX);
    assert_eq!(<SampleSpan as Unit>::from_raw(-3.0), SampleSpan::ZERO);
    assert_eq!(<SampleSpan as Unit>::from_raw(12.5).to_raw(), 12.5);
}

/// 48 kHz: one second is 48 000 frames, 10.5 ms is 504, and a time that is
/// not a whole number of frames keeps its fraction.
#[test]
fn seconds_round_trip_through_a_span_at_a_rate() {
    let rate = SampleRate::SR_48K;
    assert_eq!(
        SampleSpan::from_seconds(Seconds(1.0), rate).map(SampleSpan::get),
        Some(48_000.0)
    );
    assert_eq!(SampleSpan::of(504.0).to_seconds(rate), Seconds(0.0105));
    let one_and_a_half = SampleSpan::from_seconds(Seconds(1.5 / 48_000.0), rate).unwrap();
    assert!((one_and_a_half.get() - 1.5).abs() < 1e-12);
    for secs in [0.0, 0.0105, 0.25, 2.0] {
        let back = SampleSpan::from_seconds(Seconds(secs), rate)
            .unwrap()
            .to_seconds(rate);
        assert!((back.get() - secs).abs() < 1e-12, "{secs}");
    }
    let rate = SampleRate::of(44_100.0);
    assert_eq!(SampleSpan::of(44_100.0).to_seconds(rate), Seconds(1.0));
}

#[test]
fn a_negative_or_nan_time_is_no_span() {
    let rate = SampleRate::SR_48K;
    assert_eq!(SampleSpan::from_seconds(Seconds(-0.001), rate), None);
    assert_eq!(SampleSpan::from_seconds(Seconds(f64::NAN), rate), None);
    assert_eq!(SampleSpan::from_seconds(Seconds(f64::INFINITY), rate), None);
}

#[test]
fn floor_and_ceil_land_on_the_frames_around_the_span() {
    let span = SampleSpan::of(504.25);
    assert_eq!(span.to_samples_floor(), Samples::new(504));
    assert_eq!(span.to_samples_ceil(), Samples::new(505));
    assert_eq!(span.fract(), 0.25);

    // A whole span is its own floor and ceil, with no fraction.
    let whole = SampleSpan::of(504.0);
    assert_eq!(whole.to_samples_floor(), Samples::new(504));
    assert_eq!(whole.to_samples_ceil(), Samples::new(504));
    assert_eq!(whole.fract(), 0.0);

    assert_eq!(SampleSpan::ZERO.to_samples_ceil(), Samples::ZERO);
    // The largest span saturates rather than wrapping.
    assert_eq!(
        SampleSpan::of(f64::MAX).to_samples_floor(),
        Samples::new(usize::MAX)
    );
    // Just under a frame: the ceiling is the frame, the floor none.
    let under = SampleSpan::of(1.0_f64.next_down());
    assert_eq!(under.to_samples_floor(), Samples::ZERO);
    assert_eq!(under.to_samples_ceil(), Samples::new(1));
    assert!(under.fract() < 1.0);
}

#[test]
fn a_count_is_a_span_of_that_many_frames() {
    assert_eq!(SampleSpan::from_samples(Samples::new(512)).get(), 512.0);
    assert_eq!(
        SampleSpan::from_samples(Samples::new(512)).to_samples_floor(),
        Samples::new(512)
    );
    assert_eq!(SampleSpan::from_samples(Samples::ZERO), SampleSpan::ZERO);
}

#[test]
fn spans_add_and_scale_and_subtract_by_name() {
    let a = SampleSpan::of(100.5);
    let b = SampleSpan::of(20.0);
    assert_eq!(a + b, SampleSpan::of(120.5));
    let mut c = a;
    c += b;
    assert_eq!(c, SampleSpan::of(120.5));
    // The sum stays in range.
    assert_eq!(
        SampleSpan::of(f64::MAX) + SampleSpan::of(f64::MAX),
        SampleSpan::of(f64::MAX)
    );

    assert_eq!(a.checked_sub(b), Some(SampleSpan::of(80.5)));
    assert_eq!(b.checked_sub(a), None);
    assert_eq!(a.saturating_sub(b), SampleSpan::of(80.5));
    assert_eq!(b.saturating_sub(a), SampleSpan::ZERO);

    assert_eq!(b.scaled(0.5), Some(SampleSpan::of(10.0)));
    assert_eq!(b.scaled(0.0), Some(SampleSpan::ZERO));
    assert_eq!(b.scaled(-1.0), None);
    assert_eq!(b.scaled(f64::NAN), None);
    assert_eq!(SampleSpan::of(f64::MAX).scaled(2.0), None);

    assert_eq!(a / b, 100.5 / 20.0);
    assert_eq!(a.min(b), b);
    assert_eq!(a.max(b), a);
    assert_eq!(SampleSpan::of(5.0).clamp(b, a), b);
    assert!(b < a);
}

/// A position moved by a span is a position; the span between two positions is
/// the later minus the earlier, and the reverse is refused.
#[test]
fn a_span_and_a_position_form_an_affine_pair() {
    let write = SamplePosition(1_000.25);
    let delay = SampleSpan::of(504.5);
    let read = write - delay;
    assert_eq!(read, SamplePosition(495.75));
    assert_eq!(read + delay, write);
    assert_eq!(write.span_since(read), Some(delay));
    assert_eq!(read.span_since(write), None);
    assert_eq!(write.span_since(write), Some(SampleSpan::ZERO));
    assert_eq!(SamplePosition(f64::NAN).span_since(read), None);

    let mut cursor = SamplePosition(10.0);
    cursor += SampleSpan::of(2.5);
    assert_eq!(cursor, SamplePosition(12.5));
    cursor -= SampleSpan::of(0.5);
    assert_eq!(cursor, SamplePosition(12.0));
}

/// A driven delay time: the cell holds a span at full `f64` width, a write
/// reads back exact, and a cell written out of range reads back saturated.
#[test]
fn a_span_is_a_param() {
    let time = Param::new(SampleSpan::of(504.25));
    let on_audio_thread = time.handle();
    assert_eq!(on_audio_thread.load(), SampleSpan::of(504.25));
    time.store(SampleSpan::of(48_000.5));
    assert_eq!(on_audio_thread.load().get(), 48_000.5);

    // The width-erased cell a modulation lane writes carries an `f32`.
    let cell = time.cell();
    cell.store(-3.0, core::sync::atomic::Ordering::Relaxed);
    assert_eq!(time.load(), SampleSpan::ZERO);
    cell.store(100.0, core::sync::atomic::Ordering::Relaxed);
    assert_eq!(time.load(), SampleSpan::of(100.0));
}
