use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Beat + Beat` — adding two timeline positions has no meaning.
/// - `Beat * f64` — scaling a position depends on where zero is.
/// - `Beat % BeatDuration` — hides the loop-start origin.
/// - `Bpm - Bpm` — a tempo difference is not a tempo.
/// - `Bpm * f64`, `-Bpm`, `Bpm::from(f64)`, `Unit::from_raw` — each can build
///   a zero, negative or non-finite tempo, which a `Bpm` never is. The named
///   replacement is the checked constructor: scale the raw number (`get`) and
///   call `Bpm::new`, which returns `None` when the result is not a tempo. (No
///   such operator ever existed on `Bpm`; `From<f64>` and `Unit` did, and are
///   replaced by `new` and `of`.)
/// - `ReadRate * f64`, `ReadRate + ReadRate` — `advance`, which names the
///   operand as a sample count and returns a `SamplePosition`. This type
///   exists to be multiplied by exactly one thing, so a bare `*` would let
///   it be multiplied by anything — including the `PlaybackRate` and
///   `SrcRatio` it is already the product of, which would apply a ratio twice.
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// - `BeatDuration * FramesPerBeat`, `FramesPerBeat * f64`, `FramesPerBeat +
///   FramesPerBeat` — a beat span becomes frames only through
///   [`FramesPerBeat::frames_in`] and frames become beats only through
///   [`FramesPerBeat::beats_over`], so every reader groups the product the same
///   way; a scaled or summed rate is a different tempo, made with
///   [`Bpm::frames_per_beat`].
///
/// And a beat-synced rate is a [`BeatsPerCycle`] (beats per cycle), never an
/// [`Hz`]: the two are *inverse*. Doubling
/// the span halves the rate, so the substitution is a silent reciprocal.
/// [`Beat::cycles_in`] is the only interpreter.
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_operators_are_documented() {}

/// The affine discipline: subtracting two positions yields a *displacement*,
/// and only a displacement can be added back to a position.
#[test]
fn beat_and_beat_duration_form_an_affine_pair() {
    let a = Beat(4.0);
    let b = Beat(6.5);

    let span: BeatDuration = b - a;
    assert_eq!(span, BeatDuration(2.5));

    assert_eq!(a + span, b);
    assert_eq!(b - span, a);

    let mut cursor = Beat(0.0);
    cursor += BeatDuration(0.25);
    assert_eq!(cursor, Beat(0.25));
}

#[test]
fn sample_position_advances_by_a_frame_count_without_losing_the_fraction() {
    // The named replacement for the forbidden `SamplePosition + Samples`.
    // Exactness is the whole point: an interpolating reader sits between two
    // integer indices, and advancing must not quietly seat it on one.
    let entry = SamplePosition(1000.5);
    assert_eq!(
        entry.advanced_by(Samples::new(8192)),
        SamplePosition(9192.5)
    );
    assert_eq!(entry.advanced_by(Samples::new(0)), entry);
}

#[test]
fn sample_position_to_samples_names_its_rounding() {
    // The distinction that makes these two methods rather than one: a
    // preload window ending mid-frame must load THAT frame, so a window
    // sized by `floor` under-loads and the reader interpolates against data
    // it does not have.
    let mid_frame = SamplePosition(9192.5);
    assert_eq!(mid_frame.to_samples_floor(), Samples::new(9192));
    assert_eq!(mid_frame.to_samples_ceil(), Samples::new(9193));

    // Exact positions agree — the rounding only bites on the fraction.
    let exact = SamplePosition(9192.0);
    assert_eq!(exact.to_samples_floor(), exact.to_samples_ceil());

    // Ordering holds for every input, which is the invariant a caller
    // reasons with when it picks one.
    for raw in [0.0, 0.25, 1.0, 1.75, 48_000.5, 1e9 + 0.5] {
        let p = SamplePosition(raw);
        assert!(p.to_samples_ceil() >= p.to_samples_floor(), "{raw}");
    }
}

#[test]
fn sample_position_to_samples_collapses_nonsense_to_zero() {
    // Matches `Seconds::to_samples`'s convention rather than inventing a
    // second one: a negative or non-finite position has no frames behind it.
    assert_eq!(SamplePosition(-1.0).to_samples_floor(), Samples::ZERO);
    assert_eq!(SamplePosition(-1.0).to_samples_ceil(), Samples::ZERO);
    assert_eq!(SamplePosition(f64::NAN).to_samples_floor(), Samples::ZERO);
    assert_eq!(SamplePosition(f64::NAN).to_samples_ceil(), Samples::ZERO);
}

#[test]
fn sample_position_survives_a_long_file() {
    // The reason `SamplePosition` is f64: f32 holds integers exactly only to
    // 2^24, ~5.8 minutes at 48k. A loop point in an hour-long file must not
    // snap, and neither must a preload window computed against one.
    let hour_in = SamplePosition(48_000.0 * 3600.0);
    assert_eq!(
        hour_in.advanced_by(Samples::new(8192)).to_samples_floor(),
        Samples::new(172_800_000 + 8192)
    );
}

#[test]
fn beat_duration_to_seconds_is_the_readout_conversion() {
    // At 120 BPM a beat is half a second.
    assert_eq!(
        BeatDuration(1.0).to_seconds(Bpm::of(120.0)),
        Seconds::new(0.5)
    );
    assert_eq!(
        BeatDuration(4.0).to_seconds(Bpm::of(120.0)),
        Seconds::new(2.0)
    );
    assert_eq!(
        BeatDuration(1.0).to_seconds(Bpm::of(60.0)),
        Seconds::new(1.0)
    );
    // The slowest tempo a `Bpm` holds (zero is not one) yields zero rather
    // than inf.
    assert_eq!(
        BeatDuration(1.0).to_seconds(Bpm::of(f64::from_bits(1))),
        Seconds::new(0.0)
    );
}

#[test]
fn beat_splits_into_floor_and_fraction() {
    let b = Beat(4.75);
    assert_eq!(b.floor(), Beat(4.0));
    assert_eq!(b.fract(), BeatDuration(0.75));
    // The two halves recombine, and are visibly different kinds of thing.
    assert_eq!(b.floor() + b.fract(), b);
}

#[test]
fn cycles_in_reads_the_divisor_as_a_span_not_a_frequency() {
    // The reciprocal that shipped twice: a *longer* span is a *slower*
    // source. Under the Hz reading, 2.0 would be twice as fast and land on
    // cycle 8 here.
    assert_eq!(Beat(4.0).cycles_in(BeatsPerCycle::of(2.0)), 2.0);
    assert_eq!(Beat(4.0).cycles_in(BeatsPerCycle::of(0.5)), 8.0);

    // Un-wrapped: the integer part is the cycle index a stepped shape
    // addresses itself by, the fraction is the phase.
    let c = Beat(5.0).cycles_in(BeatsPerCycle::of(2.0));
    assert_eq!(c.floor(), 2.0);
    assert_eq!(c.fract(), 0.5);
}

#[test]
fn cycles_in_is_computed_in_f64() {
    // Dividing `beat as f32` by an f32 span is exact for power-of-two spans
    // and drifts on everything else.
    let beat = Beat(1_000_000.0);
    let span = BeatsPerCycle::of(3.0);
    let wide = beat.cycles_in(span);
    let narrowed = ((beat.get() as f32) / (span.get() as f32)) as f64;
    assert!(
        (wide.fract() - narrowed.fract()).abs() > 0.01,
        "the narrowing this method exists to avoid should be measurable here"
    );
    assert_eq!(wide, 1_000_000.0 / 3.0);
}

#[test]
fn beat_duration_ratio_counts_how_many_fit() {
    // "how many samples fit in this beat span" — the dimensional analysis
    // a rate type would otherwise be invented for.
    let span = BeatDuration(1.0);
    let per_sample = BeatDuration(0.25);
    let n: f64 = span / per_sample;
    assert_eq!(n, 4.0);
}

#[test]
fn rem_euclid_wraps_negative_overshoot_inside_the_span() {
    let len = BeatDuration(4.0);
    // Plain `%` keeps the dividend's sign, landing outside the span.
    assert_eq!(BeatDuration(-1.0) % len, BeatDuration(-1.0));
    // `rem_euclid` is the wrapping-safe form.
    assert_eq!(BeatDuration(-1.0).rem_euclid(len), BeatDuration(3.0));
}

#[test]
fn bpm_differs_from_ignores_ulp_noise() {
    let a = Bpm::of(120.0);
    assert!(!a.differs_from(Bpm::of(120.000_000_1), Bpm::of(0.001)));
    assert!(a.differs_from(Bpm::of(140.0), Bpm::of(0.001)));
}

#[test]
fn src_ratio_derives_from_typed_rates() {
    // `for_rates` is the one place the derivation lives, and it now speaks
    // the same type its two arguments are measured in.
    assert_eq!(
        SrcRatio::for_rates(SampleRate::SR_44K1, SampleRate::SR_44K1),
        SrcRatio::UNITY
    );
    let r = SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1);
    assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-12);
}

#[test]
fn src_ratio_is_exactly_unity_for_matched_rates() {
    // Bit-exact, not 1.0000001 — the matched case is the common one and must
    // not accumulate resampling error.
    assert_eq!(
        SrcRatio::for_rates(SampleRate::SR_44K1, SampleRate::SR_44K1),
        SrcRatio::UNITY
    );
    // Within the 0.01 Hz tolerance.
    assert_eq!(
        SrcRatio::for_rates(SampleRate::of(44_100.005), SampleRate::SR_44K1),
        SrcRatio::UNITY
    );
}

#[test]
fn src_ratio_converts_mismatched_rates() {
    let r = SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1);
    assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-12);
    assert!(
        r > SrcRatio::UNITY,
        "a faster file reads more source samples"
    );
}

#[test]
fn read_rate_composes_varispeed_with_conversion() {
    // Half speed on a 48k file in a 44.1k session: both effects apply once.
    // Both rates are f64-backed and the quotient is rounded once, so the
    // composition matches the exact figure to f64 precision (an f32 ratio was
    // off by ~1e-8 here).
    let rate = PlaybackRate::of(0.5);
    let src = SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1);
    let expected = 0.5 * (48_000.0 / 44_100.0);
    assert!((rate.read_rate(src).get() - expected).abs() < 1e-12);

    // Unity on both sides consumes exactly one source sample per output —
    // this one IS exact, and must stay so: it is the matched-rate path every
    // same-sample-rate session takes.
    assert_eq!(
        PlaybackRate::UNITY.read_rate(SrcRatio::UNITY),
        ReadRate::UNITY
    );
}

/// `ReadRate` omits `Mul`, so `advance` is the only way to turn a rate into a
/// distance — the omission ledger's rule that a removed operator ships with
/// its named replacement.
#[test]
fn read_rate_advances_a_position_by_a_sample_count() {
    // Half speed: 512 output samples consume 256 source samples.
    let half = PlaybackRate::of(0.5).read_rate(SrcRatio::UNITY);
    assert_eq!(half.advance(Samples::new(512)), SamplePosition(256.0));

    // Unity is the identity, exactly.
    assert_eq!(
        ReadRate::UNITY.advance(Samples::new(1024)),
        SamplePosition(1024.0)
    );

    // Zero output samples advance nowhere, whatever the rate.
    assert_eq!(half.advance(Samples::new(0)), SamplePosition(0.0));
}

/// `then` composes two read rates applied in series, and composition must be
/// exactly multiplication — a stretched varispeed voice reads at the product.
#[test]
fn read_rates_compose_in_series() {
    let half = PlaybackRate::of(0.5).read_rate(SrcRatio::UNITY);
    let double = PlaybackRate::of(2.0).read_rate(SrcRatio::UNITY);

    // Half speed then double: back to unity, and exactly so.
    assert_eq!(half.then(double), ReadRate::UNITY);

    // Unity is the identity on both sides.
    assert_eq!(half.then(ReadRate::UNITY), half);
    assert_eq!(ReadRate::UNITY.then(half), half);

    // Order does not matter — the composition is a product.
    assert_eq!(half.then(double), double.then(half));

    // The motivating case: a 2x-stretched voice at half varispeed reads a
    // quarter of a source sample per output sample. Composed, not
    // hand-multiplied — which is the whole point of the method.
    let stretch_half = ReadRate(0.5);
    assert_eq!(half.then(stretch_half), ReadRate(0.25));
    assert_eq!(
        half.then(stretch_half).advance(Samples::new(1024)),
        SamplePosition(256.0)
    );
}

#[test]
fn stretch_factor_clamps_and_follows_the_non_finite_rule() {
    assert_eq!(StretchFactor::new_clamped(8.0), StretchFactor::MAX);
    assert_eq!(StretchFactor::new_clamped(0.1), StretchFactor::MIN);
    assert!(StretchFactor::new_clamped(f64::NAN).get().is_nan());
    assert_eq!(
        StretchFactor::new_clamped(f64::INFINITY),
        StretchFactor::MAX
    );
    assert_eq!(
        StretchFactor::new_clamped(f64::NEG_INFINITY),
        StretchFactor::MIN
    );
}

/// A converted stream's read position must not drift against the timeline:
/// after one hour of session frames the accumulated file position is
/// `n * file_rate / session_rate` to a small fraction of a frame. The oracle is
/// the closed form, not another render.
#[test]
fn a_converted_read_position_does_not_drift_over_an_hour() {
    for (file, session) in [
        (48_000.0, 44_100.0),
        (44_100.0, 48_000.0),
        (44_100.0, 96_000.0),
        (96_000.0, 44_100.0),
    ] {
        let rate = PlaybackRate::UNITY.read_rate(SrcRatio::for_rates(
            SampleRate::of(file),
            SampleRate::of(session),
        ));
        let n = (session * 3600.0) as i64;
        // Block-wise accumulation, as a stream does.
        let block = 512;
        let mut pos = SamplePosition(0.0);
        let mut left = n;
        while left > 0 {
            let k = left.min(block);
            pos = SamplePosition(pos.get() + rate.advance(Samples::new(k as _)).get());
            left -= k;
        }
        let exact = n as f64 * file / session;
        // The ratio alone: one multiply, so only the ratio's own error shows.
        let once = rate.advance(Samples::new(n as _)).get();
        assert!(
            (once - exact).abs() < 1e-6,
            "{file} -> {session}: {once} vs {exact}"
        );
        // 512-frame blocks add f64 rounding of the running sum (~1e-3 frame
        // here), still far under the several frames an f32 ratio loses.
        assert!(
            (pos.get() - exact).abs() < 0.05,
            "{file} -> {session}: {} vs {exact}",
            pos.get()
        );
    }
}

/// **A beat of half a second is exactly 120 bpm, and a beat of no length has no tempo.**
#[test]
fn a_beat_period_is_sixty_over_its_seconds() {
    assert_eq!(
        Bpm::from_beat_period(Seconds::new(0.5)),
        Some(Bpm::of(120.0))
    );
    assert_eq!(
        Bpm::from_beat_period(Seconds::new(2.0)),
        Some(Bpm::of(30.0))
    );
    for bad in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        let tempo = Bpm::from_beat_period(Seconds::new(bad));
        // An infinite period is a tempo of zero, which is none either.
        assert_eq!(tempo, None, "{bad}");
    }
    // 60 over a period below 60 / MAX overflows to infinity: outside `Bpm`'s range.
    assert_eq!(Bpm::from_beat_period(Seconds::new(1e-308)), None);
    // The largest finite period still has a finite, positive tempo.
    assert_eq!(
        Bpm::from_beat_period(Seconds::new(f64::MAX)),
        Some(Bpm::of(60.0 / f64::MAX))
    );
}

/// **At 120 bpm a beat lasts exactly half a second, and the period leads back
/// to its tempo.**
#[test]
fn a_tempos_beat_period_is_sixty_over_its_bpm() {
    assert_eq!(Bpm::of(120.0).beat_period(), Seconds::new(0.5));
    assert_eq!(Bpm::of(30.0).beat_period(), Seconds::new(2.0));
    for bpm in [30.0, 60.0, 120.0, 240.0] {
        let tempo = Bpm::of(bpm);
        assert_eq!(
            Bpm::from_beat_period(tempo.beat_period()),
            Some(tempo),
            "{bpm}"
        );
    }
}

/// **The beats over a span are the tempo times the span over 60.**
///
/// 2 s at 90 bpm is 180 / 60 = 3 beats, and one beat period at any tempo is
/// one beat; each product here is exact in binary.
#[test]
fn the_beats_in_a_span_are_bpm_times_seconds_over_sixty() {
    assert_eq!(
        Bpm::of(90.0).beats_in(Seconds::new(2.0)),
        BeatDuration::new(3.0)
    );
    assert_eq!(
        Bpm::of(120.0).beats_in(Seconds::new(0.25)),
        BeatDuration::new(0.5)
    );
    assert_eq!(
        Bpm::of(120.0).beats_in(Seconds::new(-1.0)),
        BeatDuration::new(-2.0)
    );
    for tempo in [30.0, 60.0, 120.0, 240.0] {
        let tempo = Bpm::of(tempo);
        assert_eq!(tempo.beats_in(tempo.beat_period()), BeatDuration::new(1.0));
    }
}

/// `Beat::finite` refuses NaN and the infinities and keeps any finite value.
#[test]
fn a_beat_from_outside_must_be_finite() {
    for raw in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(Beat::finite(raw), None, "{raw}");
    }
    for raw in [0.0, -2.5, 1e9] {
        assert_eq!(Beat::finite(raw).map(Beat::get), Some(raw));
    }
}

/// **`ReadRate::step` is the rate in position counts**: unity is one frame, a
/// half rate half a frame; a negative or non-finite rate has no forward step.
#[test]
fn a_read_rate_steps_by_its_rate_in_counts() {
    assert_eq!(
        ReadRate::UNITY.step(),
        Some(ReadPosition::from_frames(Samples::new(1)))
    );
    assert_eq!(
        ReadRate(0.5).step(),
        Some(ReadPosition::from_count(1 << 31))
    );
    assert_eq!(ReadRate(0.0).step(), Some(ReadPosition::START));
    assert_eq!(ReadRate(-1.0).step(), None);
    assert_eq!(ReadRate(f64::NAN).step(), None);
    assert_eq!(ReadRate(f64::INFINITY).step(), None);
}

/// One beat at 120 BPM and 48 kHz spans 48 000 × 60 / 120 = 24 000 frames, and
/// the converters scale by exactly that: half a beat back is 12 000 frames
/// back, 6 000 frames is a quarter beat, and one frame is 1/24 000 of a beat.
#[test]
fn frames_per_beat_converts_beats_and_frames_at_rate_times_sixty_over_tempo() {
    let fpb = Bpm::of(120.0)
        .frames_per_beat(SampleRate::of(48_000.0))
        .expect("a finite count");
    assert_eq!(fpb.get(), 24_000.0);
    assert_eq!(fpb.frames_in(BeatDuration(-0.5)), -12_000.0);
    assert_eq!(fpb.beats_over(6_000.0), BeatDuration(0.25));
    assert_eq!(fpb.beats_per_frame(), BeatDuration(1.0 / 24_000.0));
}

/// A tempo so slow that 60 over it overflows has no finite count of frames per
/// beat: the clock does not move.
#[test]
fn the_slowest_tempo_has_no_frames_per_beat() {
    let rate = SampleRate::of(48_000.0);
    assert_eq!(Bpm::of(f64::from_bits(1)).frames_per_beat(rate), None);
}
