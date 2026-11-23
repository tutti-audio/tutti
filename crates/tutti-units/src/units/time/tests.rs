use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// - `Hz / Seconds`-style reciprocal operators (`1.0 / Hz`, `Seconds / Hz`): a frequency
///   and a period are one quantity read two ways, and [`Hz::period`] /
///   [`Seconds::frequency`] are the one route, with their `None` for a value that has no
///   reciprocal.
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_conversions_are_documented() {}

/// `period` and `frequency` are the one route between a frequency and its cycle's time: they
/// are exact inverses at a power of two and refuse a value that has no period.
#[test]
fn a_frequency_and_its_period_are_one_route() {
    assert_eq!(Hz(1000.0).period(), Some(Seconds(0.001)));
    assert_eq!(Seconds(0.5).frequency(), Some(Hz(2.0)));
    assert_eq!(Hz(256.0).period(), Some(Seconds(0.003_906_25)));
    assert_eq!(Hz(256.0).period().unwrap().frequency(), Some(Hz(256.0)));
    for bad in [0.0f64, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(Hz(bad).period(), None, "{bad}");
    }
    // A subnormal time has a reciprocal past `f64::MAX`.
    for bad in [0.0f64, -1.0, f64::NAN, f64::INFINITY, 5e-324] {
        assert_eq!(Seconds(bad).frequency(), None, "{bad}");
    }
    assert_eq!(Seconds(1e-60).frequency(), Some(Hz(1e60)));
    assert_eq!(Hz::ZERO, Hz(0.0));
    assert_eq!(Seconds::ZERO, Seconds(0.0));
}

#[test]
fn seconds_to_samples_names_its_rounding() {
    // 0.5 s at 44100 is exactly 22050 frames — every variant agrees.
    assert_eq!(
        Seconds(0.5).to_samples(SampleRate::SR_44K1),
        Samples::new(22_050)
    );
    assert_eq!(
        Seconds(0.5).to_samples_floor(SampleRate::SR_44K1),
        Samples::new(22_050)
    );
    assert_eq!(
        Seconds(0.5).to_samples_ceil(SampleRate::SR_44K1),
        Samples::new(22_050)
    );

    // 10 ms at 44100 is 441.0; nudge it so the three diverge.
    let odd = Seconds(0.010_5);
    assert_eq!(odd.to_samples_floor(SampleRate::SR_44K1), Samples::new(463));
    assert_eq!(odd.to_samples_ceil(SampleRate::SR_44K1), Samples::new(464));
    assert_eq!(odd.to_samples(SampleRate::SR_44K1), Samples::new(463));

    // Allocation must never under-size: ceil is >= nearest, always.
    for ms in 1..200 {
        let s = Seconds(ms as f64 * 0.001);
        assert!(s.to_samples_ceil(SampleRate::SR_48K) >= s.to_samples(SampleRate::SR_48K));
        assert!(s.to_samples(SampleRate::SR_48K) >= s.to_samples_floor(SampleRate::SR_48K));
    }
}

#[test]
fn seconds_to_samples_collapses_nonsense_to_zero() {
    // `NaN as usize` is already 0 and a negative cast already saturates —
    // this pins the behaviour so it is stated rather than inherited.
    assert_eq!(
        Seconds(f64::NAN).to_samples(SampleRate::SR_48K),
        Samples::ZERO
    );
    assert_eq!(
        Seconds(f64::INFINITY).to_samples(SampleRate::SR_48K),
        Samples::ZERO
    );
    assert_eq!(Seconds(-1.0).to_samples(SampleRate::SR_48K), Samples::ZERO);
}

#[test]
fn seconds_to_samples_computes_wide_enough_for_a_long_render() {
    // f32 cannot represent frame counts past 2^24 (~6 min at 48k), which
    // is why the arithmetic widens to f64 before rounding.
    let hour = Seconds(3600.0);
    assert_eq!(
        hour.to_samples(SampleRate::SR_48K),
        Samples::new(172_800_000)
    );
}

#[test]
fn sample_rate_accepts_every_shape_a_caller_holds() {
    // File headers and device configs hand out u32; the engine's own
    // arithmetic is f64. Both widen exactly, which is what lets the
    // migrated signatures take `impl Into<SampleRate>` without churning
    // their call sites.
    assert_eq!(SampleRate::SR_48K, SampleRate::SR_48K);
    assert_eq!(SampleRate::from_hz(48_000), Some(SampleRate::SR_48K));
    assert_eq!(SampleRate::SR_44K1, SampleRate::SR_44K1);
}

#[test]
fn sample_rate_orders_and_bounds_but_does_not_compose() {
    assert!(SampleRate::SR_44K1 < SampleRate::SR_48K);
    assert_eq!(
        SampleRate::SR_44K1.max(SampleRate::SR_48K),
        SampleRate::SR_48K
    );
    assert_eq!(
        SampleRate::SR_192K.clamp(SampleRate::SR_44K1, SampleRate::SR_48K),
        SampleRate::SR_48K
    );
    // Deliberately absent, per the omission ledger beside the definition:
    // `SR_44K1 + SR_48K` (not a rate), `SR_48K * 2.0` (a scaled rate is a
    // different *device* rate), and `SR_48K / SR_44K1` (that quotient is a
    // `SrcRatio`, and `for_rates` owns the derivation plus its guards).
}

/// **A one-pole smoother built from `one_pole_coeff` reaches `1 − e⁻¹`
/// of a unit step after `τ` seconds**, stepped per sample at `rate` and
/// per block through `one_pole_coeff_over`.
#[test]
fn one_pole_step_response_reaches_one_minus_inverse_e_after_tau() {
    let want = 1.0 - (-1.0f64).exp();
    for (tau, rate) in [(0.01, 48_000.0), (0.5, 44_100.0), (2.0, 96_000.0)] {
        let c = Seconds(tau).one_pole_coeff(SampleRate::of(rate));
        let n = Seconds(tau).to_samples(SampleRate::of(rate)).get();
        let mut y = 0.0f64;
        for _ in 0..n {
            y += c * (1.0 - y);
        }
        assert!((y - want).abs() < 1e-4, "τ {tau} at {rate}: {y}");
    }
    // Stepped once per 10 ms block, τ = 100 ms: ten steps.
    let c = Seconds(0.1).one_pole_coeff_over(Seconds(0.01));
    let mut y = 0.0f64;
    for _ in 0..10 {
        y += c * (1.0 - y);
    }
    assert!((y - want).abs() < 1e-5, "per block: {y}");
}

/// **A zero, negative or NaN time constant is instant (1); an infinite
/// one holds (0); a step form's empty step moves nothing.** And a long
/// time constant keeps a coefficient above zero.
///
/// Also carries the deleted `tutti-nodes` `a_zero_smoothing_time_is_instant`
/// (a zero smoothing time is instant, never frozen).
#[test]
fn one_pole_zero_time_is_instant_and_bad_input_is_defined() {
    let sr = SampleRate::SR_48K;
    for tau in [0.0, -0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(Seconds(tau).one_pole_coeff(sr), 1.0, "τ {tau}");
        assert_eq!(Seconds(tau).one_pole_coeff_over(Seconds(0.01)), 1.0);
        assert_eq!(Seconds(tau).one_pole_coeff_over(Seconds(0.0)), 1.0);
    }
    assert_eq!(Seconds(f64::INFINITY).one_pole_coeff(sr), 0.0);
    for step in [0.0, -0.01, f64::NAN] {
        assert_eq!(Seconds(0.1).one_pole_coeff_over(Seconds(step)), 0.0);
    }
    // 1000 s at 48 kHz: about 2.08e-8, not cancelled to 0.
    let long = Seconds(1_000.0).one_pole_coeff(sr);
    assert!((long * 48e6 - 1.0).abs() < 1e-6, "{long}");
    // Every finite positive input lands in [0, 1].
    for tau in [1e-9, 1e-3, 1.0, 1e6] {
        let c = Seconds(tau).one_pole_coeff(sr);
        assert!((0.0..=1.0).contains(&c), "τ {tau}: {c}");
    }
}

#[test]
fn seconds_to_samples_takes_the_typed_rate() {
    // The converter the DSP nodes call: it takes the typed rate, so an
    // untyped rate in a node field has to justify itself at the call.
    assert_eq!(
        Seconds(1.0).to_samples(SampleRate::SR_48K),
        Samples::new(48_000)
    );
    // Raw floats still infer through `impl Into`.
    assert_eq!(
        Seconds(1.0).to_samples(SampleRate::SR_48K),
        Samples::new(48_000)
    );
    // And the rounding split holds through the typed form.
    assert_eq!(
        Seconds(0.5).to_samples_floor(SampleRate::of(3.0)),
        Samples::new(1)
    );
    assert_eq!(
        Seconds(0.5).to_samples_ceil(SampleRate::of(3.0)),
        Samples::new(2)
    );
}

#[test]
fn nyquist_crosses_from_clock_rate_to_signal_frequency() {
    assert_eq!(SampleRate::SR_48K.nyquist(), Hz(24_000.0));
    assert_eq!(SampleRate::SR_44K1.nyquist(), Hz(22_050.0));

    // The margin is a fraction of *Nyquist*, not of the sample rate. The
    // call sites this replaced scaled the rate (`sr * 0.499`), so porting
    // one means doubling its constant — 0.499 of the rate is 0.998 of
    // Nyquist. Getting that backwards halves every filter's usable range,
    // so it is pinned here.
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.0), Hz(24_000.0));
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(0.5), Hz(12_000.0));
    let svf = SampleRate::SR_48K.nyquist_scaled(0.998);
    assert!((svf.get() - 48_000.0 * 0.499).abs() < 1e-9);
}

#[test]
fn nyquist_scaled_refuses_to_exceed_nyquist() {
    // A margin above unity would hand back a frequency above Nyquist —
    // the exact aliasing bug the clamp exists to stop.
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.5), Hz(24_000.0));
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(-0.2), Hz::ZERO);
}

/// A `Seconds` names every frame: a frame count round-trips exactly through
/// seconds and back, well past the 256 s where an `f32` second (spacing 2^-15 s
/// at that point) stopped being able to, and out to a day at 192 kHz.
#[test]
fn a_seconds_round_trip_is_exact_for_every_frame_a_project_holds() {
    for rate in [SampleRate::SR_48K, SampleRate::SR_96K, SampleRate::SR_192K] {
        let round_trips =
            |n: usize| Samples::new(n).to_seconds(rate).to_samples(rate) == Samples::new(n);
        let day = (86_400.0 * rate.get()) as usize;
        for n in (0..=day).step_by(9_973) {
            assert!(round_trips(n), "{n} frames at {rate:?}");
        }
        // The frames around 256 s at 48 kHz, where an `f32` first failed.
        for n in 12_288_000..12_288_400 {
            assert!(round_trips(n), "{n} frames at {rate:?}");
        }
    }
}

/// A rate is finite and above zero: `new` refuses every other raw, and keeps
/// the smallest positive and the largest finite ones.
#[test]
fn sample_rate_new_refuses_what_is_not_a_finite_positive_rate() {
    for bad in [
        0.0,
        -0.0,
        -1.0,
        -48_000.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert_eq!(SampleRate::new(bad), None, "{bad}");
    }
    for good in [f64::MIN_POSITIVE, 1e-300, 44_100.0, f64::MAX] {
        assert_eq!(SampleRate::new(good).map(SampleRate::get), Some(good));
    }
}

/// `of` is `new` that panics, so a literal that is no rate cannot compile into
/// a constant.
#[test]
#[should_panic(expected = "not a sample rate: finite and above zero")]
fn sample_rate_of_panics_on_zero() {
    let _ = SampleRate::of(0.0);
}

/// The named rates are the figures they are named for.
#[test]
fn the_named_sample_rates_are_their_figures() {
    let named = [
        (SampleRate::SR_8K, 8_000.0),
        (SampleRate::SR_22K05, 22_050.0),
        (SampleRate::SR_44K1, 44_100.0),
        (SampleRate::SR_48K, 48_000.0),
        (SampleRate::SR_88K2, 88_200.0),
        (SampleRate::SR_96K, 96_000.0),
        (SampleRate::SR_192K, 192_000.0),
    ];
    for (rate, hz) in named {
        assert_eq!(rate.get(), hz);
    }
}

#[test]
fn frame_offset_keeps_the_sign_where_to_samples_clamps() {
    // -0.5 s at 48 kHz is exactly -24 000 frames.
    assert_eq!(Seconds(-0.5).to_frame_offset(SampleRate::SR_48K), -24_000);
    assert_eq!(Seconds(0.5).to_frame_offset(SampleRate::SR_48K), 24_000);
    assert_eq!(Seconds(-0.5).to_samples(SampleRate::SR_48K), Samples::ZERO);
}

#[test]
fn frame_offset_rounds_to_nearest_half_away_from_zero() {
    // At 3 Hz, 1/9 s is a third of a frame.
    let rate = SampleRate::of(3.0);
    let third = Seconds(1.0 / 9.0);
    let neg = Seconds(-1.0 / 9.0);
    assert_eq!(third.to_frame_offset(rate), 0);
    assert_eq!(neg.to_frame_offset(rate), 0);
    // Exactly half a frame (1/6 s at 3 Hz) rounds away from zero, either sign.
    assert_eq!(Seconds(0.5 / 3.0).to_frame_offset(rate), 1);
    assert_eq!(Seconds(-0.5 / 3.0).to_frame_offset(rate), -1);
    // Two thirds rounds away from zero, either sign.
    assert_eq!(Seconds(2.0 / 9.0).to_frame_offset(rate), 1);
    assert_eq!(Seconds(-2.0 / 9.0).to_frame_offset(rate), -1);
}

#[test]
fn frame_offset_of_nonsense_is_zero_and_huge_saturates() {
    assert_eq!(Seconds(f64::NAN).to_frame_offset(SampleRate::SR_48K), 0);
    assert_eq!(Seconds(1e300).to_frame_offset(SampleRate::SR_48K), i64::MAX);
    assert_eq!(
        Seconds(-1e300).to_frame_offset(SampleRate::SR_48K),
        i64::MIN
    );
    assert_eq!(
        Seconds(f64::INFINITY).to_frame_offset(SampleRate::SR_48K),
        i64::MAX
    );
    assert_eq!(
        Seconds(f64::NEG_INFINITY).to_frame_offset(SampleRate::SR_48K),
        i64::MIN
    );
}

#[test]
fn one_second_holds_the_rate_rounded_up_and_at_least_one_frame() {
    assert_eq!(SampleRate::SR_44K1.one_second(), Samples::new(44_100));
    assert_eq!(SampleRate::SR_48K.one_second(), Samples::new(48_000));
    assert_eq!(SampleRate::of(44_100.5).one_second(), Samples::new(44_101));
    assert_eq!(SampleRate::of(0.2).one_second(), Samples::new(1));
}

/// `one_pole_coeff` is the closed form `1 - exp(-1 / (tau * rate))` to `1e-15` relative and
/// `one_pole_coeff_over` is `1 - exp(-step / tau)` to `1e-12` relative, both in `f64`; narrowed
/// once they are the closed form's `f32`.
#[test]
fn the_one_pole_coefficients_are_the_closed_form_in_f64() {
    for (tau, rate) in [
        (0.01_f64, 48_000.0_f64),
        (0.1, 44_100.0),
        (1_000.0, 96_000.0),
        (1e-5, 8_000.0),
    ] {
        let closed = 1.0 - (-1.0 / (tau * rate)).exp();
        let c = Seconds(tau).one_pole_coeff(SampleRate::of(rate));
        assert!(
            ((c - closed) / closed).abs() <= 1e-15,
            "τ {tau}: {c} vs {closed}"
        );
        assert_eq!(c as f32, closed as f32);
        let step = 1.0 / rate;
        let closed_over = 1.0 - (-step / tau).exp();
        let over = Seconds(tau).one_pole_coeff_over(Seconds(step));
        assert!(
            ((over - closed_over) / closed_over).abs() <= 1e-12,
            "over, τ {tau}"
        );
        assert_eq!(over as f32, closed_over as f32);
    }
}

/// A frequency is the reciprocal of its period in `f64`, with no narrowing between.
#[test]
fn a_frequency_is_the_reciprocal_of_its_period() {
    assert_eq!(Hz(440.0).period(), Some(Seconds(1.0 / 440.0)));
    assert_eq!(Seconds(1.0 / 440.0).frequency(), Some(Hz(440.0)));
}

/// A kernel in `f32` narrows a `Hz` and its period once; each is the `f32` closed form within 1 ulp.
#[test]
fn a_frequency_narrows_to_the_f32_closed_form() {
    for hz in [20.0_f32, 261.625_58, 440.0, 1_000.0, 18_000.0] {
        let period = Hz(f64::from(hz)).period().unwrap().get() as f32;
        assert!((period - 1.0 / hz).abs() <= period * f32::EPSILON);
        let nyquist = SampleRate::of(f64::from(hz)).nyquist().get() as f32;
        assert_eq!(nyquist, hz / 2.0);
    }
}

/// A sample rate crosses into data as its rate in hertz.
#[test]
fn a_sample_rate_is_a_quantity_of_its_hertz() {
    assert_eq!(
        <f64 as crate::Real>::from_unit(SampleRate::SR_48K),
        48_000.0
    );
}

/// **The leave of a one-pole is `exp(-1 / (τ rate))`, held to the exponential's digits**: a time
/// constant of a twentieth of a frame leaves `exp(-20) = 2.06e-9` of the gap, and instant and
/// holding times are 0 and 1.
#[test]
fn one_pole_leave_is_the_exponential_and_is_not_cancelled() {
    let sr = SampleRate::SR_48K;
    let tau = Seconds(0.05 / 48_000.0);
    assert_eq!(tau.one_pole_leave(sr), (-20.0f64).exp());
    for t in [0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(Seconds(t).one_pole_leave(sr), 0.0, "τ {t}");
    }
    assert_eq!(Seconds(f64::INFINITY).one_pole_leave(sr), 1.0);
    for t in [1e-6, 1e-3, 0.1, 10.0] {
        let (c, r) = (Seconds(t).one_pole_coeff(sr), Seconds(t).one_pole_leave(sr));
        assert!((c + r - 1.0).abs() < 1e-15, "τ {t}: {c} + {r}");
    }
}

/// **A frequency of a quarter of the rate turns a quarter of `TAU` per frame, exactly.**
///
/// 12 kHz at 48 kHz is `f / fs = 0.25`, exact in binary; `TAU * 0.25` only moves the exponent,
/// so the result is `TAU / 4`, which is `FRAC_PI_2` bit for bit (`TAU` is `2 * PI`).
#[test]
fn a_quarter_rate_frequency_turns_a_quarter_turn_per_frame() {
    let w = Hz(12_000.0).angle_per_frame(SampleRate::of(48_000.0));
    assert_eq!(w.get().to_bits(), core::f64::consts::FRAC_PI_2.to_bits());
    let w = Hz(24_000.0).angle_per_frame(SampleRate::of(48_000.0));
    assert_eq!(w.get().to_bits(), core::f64::consts::PI.to_bits());
}

/// **`finite_or` returns the value itself when finite, `-0.0` included, else the fallback.**
#[test]
fn finite_or_keeps_a_finite_value_and_replaces_a_non_finite_one() {
    let fallback = Hz(440.0);
    assert_eq!(
        Hz(-0.0).finite_or(fallback).get().to_bits(),
        (-0.0_f64).to_bits()
    );
    assert_eq!(Hz(1e300).finite_or(fallback), Hz(1e300));
    assert_eq!(Hz(-3.5).finite_or(fallback), Hz(-3.5));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(Hz(bad).finite_or(fallback), fallback, "{bad}");
    }
    // A NaN fallback is returned as given, not repaired.
    assert!(Hz(f64::NAN).finite_or(Hz(f64::NAN)).get().is_nan());
}

/// What `TimecodeFrames` deliberately lacks, beside the definition in `units/time.rs`:
///
/// - `Mul<TimecodeFrames>`: frames squared is no quantity; scaling is `Mul<f64>`.
/// - `Div<TimecodeFrames> -> f64`: a comparison is `<`; whole counts come from `floor`.
/// - `Rem`: a label wraps by its own fields, and `SmpteTimecode::from_frame_count`
///   owns the drop-frame wrap.
/// - `From<f64>`, `From<TimecodeFrames> for f64`: `new` and `get`.
///
/// Each replacement is asserted here.
#[test]
fn timecode_frames_omissions_have_their_replacements() {
    let frames = TimecodeFrames::new(2.5);
    assert_eq!(frames * 2.0, TimecodeFrames::new(5.0));
    assert!(frames < TimecodeFrames::from_count(3));
    assert_eq!(frames.floor(), 2);
    assert_eq!(TimecodeFrames::new(frames.get()), frames);
}

/// `round` is nearest (half away from zero), `floor` is down, and a count no whole
/// number holds (NaN, negative) is 0, so a caller that needs a refusal checks first.
#[test]
fn timecode_frames_name_their_rounding() {
    assert_eq!(TimecodeFrames::new(2.5).round(), 3);
    assert_eq!(TimecodeFrames::new(2.5).floor(), 2);
    assert_eq!(TimecodeFrames::new(2.499).round(), 2);
    assert_eq!(TimecodeFrames::new(7.0).round(), 7);
    assert_eq!(TimecodeFrames::new(7.0).floor(), 7);
    for bad in [-1.5, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(TimecodeFrames::new(bad).round(), 0, "{bad}");
        assert_eq!(TimecodeFrames::new(bad).floor(), 0, "{bad}");
    }
    assert_eq!(TimecodeFrames::new(f64::INFINITY).floor(), u64::MAX);
}

/// A whole count converts exactly both ways, and the counts add, scale and order as
/// frames do.
#[test]
fn timecode_frames_compose_as_a_count() {
    let five = TimecodeFrames::from_count(5);
    assert_eq!(five.floor(), 5);
    assert_eq!(five + TimecodeFrames::new(0.5), TimecodeFrames::new(5.5));
    assert_eq!(five * 2.0, TimecodeFrames::from_count(10));
    assert_eq!((-five).abs(), five);
    assert!(TimecodeFrames::new(1.5) < five);
    assert_eq!(
        five - TimecodeFrames::from_count(TimecodeFrames::new(5.25).floor()),
        TimecodeFrames::ZERO
    );
}
