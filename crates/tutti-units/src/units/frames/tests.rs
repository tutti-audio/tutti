use super::*;

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
        invalid: &[
            -0.0,
            -0.001,
            -50.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ],
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
        SampleSpan::from_seconds(Seconds::new(1.0), rate).map(SampleSpan::get),
        Some(48_000.0)
    );
    assert_eq!(SampleSpan::of(504.0).to_seconds(rate), Seconds::new(0.0105));
    let one_and_a_half = SampleSpan::from_seconds(Seconds::new(1.5 / 48_000.0), rate).unwrap();
    assert!((one_and_a_half.get() - 1.5).abs() < 1e-12);
    for secs in [0.0, 0.0105, 0.25, 2.0] {
        let back = SampleSpan::from_seconds(Seconds::new(secs), rate)
            .unwrap()
            .to_seconds(rate);
        assert!((back.get() - secs).abs() < 1e-12, "{secs}");
    }
    let rate = SampleRate::of(44_100.0);
    assert_eq!(SampleSpan::of(44_100.0).to_seconds(rate), Seconds::new(1.0));
}

#[test]
fn a_negative_or_nan_time_is_no_span() {
    let rate = SampleRate::SR_48K;
    assert_eq!(SampleSpan::from_seconds(Seconds::new(-0.001), rate), None);
    assert_eq!(SampleSpan::from_seconds(Seconds::new(f64::NAN), rate), None);
    assert_eq!(
        SampleSpan::from_seconds(Seconds::new(f64::INFINITY), rate),
        None
    );
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
    // Nothing scaled by a negative factor is still nothing, with a positive zero.
    assert!(
        SampleSpan::ZERO
            .scaled(-1.0)
            .unwrap()
            .get()
            .is_sign_positive()
    );
    assert_eq!(b.scaled(f64::NAN), None);
    assert_eq!(SampleSpan::of(f64::MAX).scaled(2.0), None);

    assert_eq!(a / b, 5.025);
    assert_eq!(a.min(b), b);
    assert_eq!(a.max(b), a);
    assert_eq!(SampleSpan::of(5.0).clamp(b, a), b);
    assert!(b < a);
}

/// A position moved by a span is a position; the span between two positions is
/// the later minus the earlier, and the reverse is refused.
#[test]
fn a_span_and_a_position_form_an_affine_pair() {
    let write = SamplePosition::new(1_000.25);
    let delay = SampleSpan::of(504.5);
    let read = write - delay;
    assert_eq!(read, SamplePosition::new(495.75));
    assert_eq!(read + delay, write);
    assert_eq!(write.span_since(read), Some(delay));
    assert_eq!(read.span_since(write), None);
    assert_eq!(write.span_since(write), Some(SampleSpan::ZERO));
    assert_eq!(SamplePosition::new(f64::NAN).span_since(read), None);

    let mut cursor = SamplePosition::new(10.0);
    cursor += SampleSpan::of(2.5);
    assert_eq!(cursor, SamplePosition::new(12.5));
    cursor -= SampleSpan::of(0.5);
    assert_eq!(cursor, SamplePosition::new(12.0));
}

#[test]
fn negative_zero_is_never_a_span() {
    assert_eq!(SampleSpan::new(-0.0), None);
    assert!(SampleSpan::clamped(-0.0).get().is_sign_positive());
    assert!(
        <SampleSpan as Unit>::from_raw(-0.0)
            .get()
            .is_sign_positive()
    );
    let none = SampleSpan::from_seconds(Seconds::new(-0.0), SampleRate::SR_48K).unwrap();
    assert!(none.get().is_sign_positive());
}

const QUANTUM: f64 = 1.0 / ReadPosition::COUNTS_PER_FRAME;

#[test]
fn a_read_position_of_whole_frames_is_the_count_shifted_up_32_bits() {
    assert_eq!(
        ReadPosition::from_frames(Samples::new(0)),
        ReadPosition::START
    );
    assert_eq!(
        ReadPosition::from_frames(Samples::new(1)).get(),
        1_u64 << 32
    );
    assert_eq!(
        ReadPosition::from_frames(Samples::new(1_920_000)).get(),
        1_920_000_u64 << 32
    );
    assert_eq!(
        ReadPosition::from_frames(Samples::new(1_920_000)).frames(),
        Samples::new(1_920_000)
    );
}

#[test]
fn a_read_position_saturates_at_two_to_the_32_frames() {
    let top = u32::MAX as usize;
    assert_eq!(
        ReadPosition::from_frames(Samples::new(top)).get(),
        u64::from(u32::MAX) << 32
    );
    assert_eq!(
        ReadPosition::from_frames(Samples::new(top + 1)),
        ReadPosition::MAX
    );
    assert_eq!(
        ReadPosition::of(Seconds::new(1e12), SampleRate::SR_48K),
        ReadPosition::MAX
    );
}

#[test]
fn a_read_position_of_a_time_is_its_frames_to_the_quantum() {
    // 9.9 s at 192 kHz is 1 900 800 frames; 0.3 of a frame is 3/10 * 2^32 counts, rounded.
    let rate = SampleRate::SR_192K;
    assert_eq!(
        ReadPosition::of(Seconds::new(9.9), rate).frames(),
        Samples::new(1_900_800)
    );
    let one_tenth = ReadPosition::of(Seconds::new(0.1 / 192_000.0), rate);
    assert!((one_tenth.get() as f64 * QUANTUM - 0.1).abs() <= QUANTUM);
    assert_eq!(
        ReadPosition::of(Seconds::new(0.25), SampleRate::SR_48K).get(),
        12_000_u64 << 32
    );
}

#[test]
fn a_read_position_of_a_negative_or_nan_time_is_the_start() {
    for t in [-1.0, -1e-30, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(
            ReadPosition::of(Seconds::new(t), SampleRate::SR_48K),
            ReadPosition::START
        );
    }
    assert_eq!(
        ReadPosition::of(Seconds::new(f64::INFINITY), SampleRate::SR_48K),
        ReadPosition::MAX
    );
}

#[test]
fn a_read_position_holds_a_count_a_float_would_round() {
    // 2^23 + 2^-32 frames: an f32 holds 2^23 + 1 at best. The position keeps every bit.
    let p = ReadPosition::from_count((1_u64 << 55) + 1);
    assert_eq!(p.frames(), Samples::new(1 << 23));
    assert_eq!(p.get() & 0xffff_ffff, 1);
}

#[test]
fn a_read_position_saturating_sub_stops_at_the_start() {
    let p = ReadPosition::from_count((5_u64 << 32) + 7);
    assert_eq!(p.saturating_sub(Samples::new(2)).get(), (3_u64 << 32) + 7);
    assert_eq!(p.saturating_sub(Samples::new(6)), ReadPosition::START);
}

#[test]
fn a_dyadic_span_round_trips_through_a_read_position_exactly() {
    for x in [0.0, 0.5, 1.0, 3.25, 1_900_800.375, 2f64.powi(-32)] {
        let span = SampleSpan::of(x);
        assert_eq!(ReadPosition::from_span(span).span(), span, "{x}");
    }
}

#[test]
fn a_span_past_the_range_is_the_last_position_and_a_negative_one_the_start() {
    assert_eq!(
        ReadPosition::from_span(SampleSpan::of(1e12)),
        ReadPosition::MAX
    );
    assert_eq!(
        ReadPosition::from_span(SampleSpan::clamped(-5.0)),
        ReadPosition::START
    );
}

/// **`advanced` moves by exact counts**: `n` steps of a fractional step land where
/// `n` single advances do, and the sum saturates at `MAX`.
#[test]
fn advanced_is_n_exact_steps_and_saturates() {
    let step = ReadPosition::from_count(3 << 30); // 0.75 frame
    let mut one_by_one = ReadPosition::START;
    for _ in 0..1000 {
        one_by_one = one_by_one.advanced(step, Samples::new(1));
    }
    let at_once = ReadPosition::START.advanced(step, Samples::new(1000));
    assert_eq!(at_once, one_by_one);
    assert_eq!(at_once.frames(), Samples::new(750));
    assert_eq!(
        ReadPosition::from_frames(Samples::new(10)).advanced(ReadPosition::START, Samples::new(5)),
        ReadPosition::from_frames(Samples::new(10)),
        "a zero step stays put"
    );
    assert_eq!(
        ReadPosition::MAX.advanced(step, Samples::new(1)),
        ReadPosition::MAX
    );
    assert_eq!(
        ReadPosition::START.advanced(ReadPosition::MAX, Samples::new(2)),
        ReadPosition::MAX
    );
}
