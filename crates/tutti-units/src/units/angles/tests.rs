use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Azimuth < Azimuth`, `Azimuth.clamp(..)` — a wrapping coordinate has
///   no ordering and no saturating clamp. `wrap` is the constraint.
/// - `Radians < Radians`, `Radians.clamp(..)` — the same, for the radian
///   coordinate; `wrapped_signed` is the constraint.
/// - `Azimuth + ArcDegrees` — `rotate_by`, which wraps; the operator would
///   not. Same for `Elevation + ArcDegrees` vs `tilt_by`, which clamps.
/// - `Azimuth - Azimuth` — `shortest_arc_to`; a plain subtraction takes the
///   long way around the seam.
/// - `Phase + Phase`, `Phase * f64`, `Phase.clamp(..)` — a cycle position.
///   Clamping pins it at 0.999 where it belongs at the next cycle's start.
/// - `Phase % Phase` — `%` is one of the two sign-preserving wraps this
///   type was introduced to eliminate. `wrapped` is the only wrap.
/// - `Phase + PhaseIncrement` — `advance`, which wraps; the operator would
///   not.
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_operators_are_documented() {}

#[test]
fn azimuth_wraps_rather_than_saturating() {
    // The shipped bug this type exists to prevent: a saturating clamp maps
    // 190 degrees to 180 (hard left). It is 170 degrees to the *right*.
    assert_eq!(Azimuth(190.0).wrap(), Azimuth(-170.0));
    assert_eq!(Azimuth(-190.0).wrap(), Azimuth(170.0));

    // `rem_euclid`, not `%`: several full turns in either direction still
    // land in range, which plain `%` does not manage for negatives.
    assert_eq!(Azimuth(360.0 + 45.0).wrap(), Azimuth(45.0));
    assert_eq!(Azimuth(-720.0 - 45.0).wrap(), Azimuth(-45.0));

    // The range is half-open at +180: one endpoint, not two names for it.
    assert_eq!(Azimuth(180.0).wrap(), Azimuth(-180.0));
}

#[test]
fn azimuth_takes_the_short_way_across_the_seam() {
    // The second shipped bug: `to - self` reports -340 here. The listener
    // hears the panner sweep almost all the way around to travel 20
    // degrees.
    let from = Azimuth(170.0);
    let to = Azimuth(-170.0);
    assert_eq!(from.shortest_arc_to(to), ArcDegrees(20.0));
    assert_eq!(to.shortest_arc_to(from), ArcDegrees(-20.0));

    // And the interpolator built on it crosses the seam rather than
    // retreating from it: halfway from 170 to -170 is 180, not 0.
    assert_eq!(from.lerp_shortest(to, 0.5), Azimuth(-180.0));
    assert_eq!(from.lerp_shortest(to, 0.0), from);
    assert_eq!(from.lerp_shortest(to, 1.0), to);
}

#[test]
fn azimuth_rotation_stays_on_the_circle() {
    assert_eq!(Azimuth(170.0).rotate_by(ArcDegrees(20.0)), Azimuth(-170.0));
    // Mirroring across the front axis — the one operator a bearing keeps.
    assert_eq!(-Azimuth(45.0), Azimuth(-45.0));
}

#[test]
fn elevation_saturates_where_azimuth_wraps() {
    // Same numbers, opposite correct answers — which is why one angle
    // type could not serve both. Past the pole, elevation stops.
    assert_eq!(Elevation::new_clamped(120.0), Elevation::UP);
    assert_eq!(Elevation::new_clamped(-120.0), Elevation::DOWN);
    // Where the identical azimuth input wraps to the far side instead.
    assert_eq!(Azimuth(120.0).wrap(), Azimuth(120.0));
    assert_eq!(Azimuth(200.0).wrap(), Azimuth(-160.0));

    assert_eq!(Elevation::LEVEL.tilt_by(ArcDegrees(30.0)), Elevation(30.0));
    assert_eq!(Elevation(80.0).tilt_by(ArcDegrees(30.0)), Elevation::UP);

    // Ordering is meaningful here and absent on `Azimuth`.
    assert!(Elevation::DOWN < Elevation::LEVEL);
    assert!(Elevation::LEVEL < Elevation::UP);
}

#[test]
fn elevation_lerp_needs_no_seam_handling() {
    assert_eq!(Elevation(0.0).lerp(Elevation(90.0), 0.5), Elevation(45.0));
    assert_eq!(
        Elevation(-90.0).lerp(Elevation(90.0), 0.5),
        Elevation::LEVEL
    );
}

#[test]
fn phase_wrap_lands_a_negative_phase_in_range() {
    // `%` and `fract` keep the sign of the input, so a negative phase would
    // stay negative and index off the front of a shape table.
    assert_eq!(-0.25_f64 % 1.0, -0.25);
    assert_eq!((-0.25_f64).fract(), -0.25);
    // `rem_euclid` — the only wrap `Phase` performs.
    assert_eq!(Phase::wrapped(-0.25), Phase(0.75));

    assert_eq!(Phase::wrapped(1.25), Phase(0.25));
    assert_eq!(Phase::wrapped(0.5), Phase(0.5));
    // Many turns out, either direction, still lands in range.
    assert_eq!(Phase::wrapped(-3.25), Phase(0.75));
    assert_eq!(Phase::wrapped(7.5), Phase(0.5));
}

/// `wrapped`'s two shortcuts are `rem_euclid`, bit for bit — including at
/// the edges of each range, where a sloppier wrap (`v - 1.0` for all
/// `v >= 1`, or `< 1.0` tested as `<= 1.0`) would hand back `1.0`, which is
/// not a phase.
#[test]
fn the_wrap_shortcuts_are_rem_euclid_exactly() {
    let mut probes = vec![
        0.0f64,
        -0.0,
        1.0,
        2.0,
        -1.0,
        f64::MIN_POSITIVE,
        1.0 - f64::EPSILON / 2.0,
        1.0 + f64::EPSILON,
        2.0 - f64::EPSILON,
        f64::NAN,
        f64::INFINITY,
    ];
    let mut state = 0x1234_5678_u32;
    for _ in 0..20_000 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        probes.push((state >> 8) as f64 / (1u32 << 24) as f64 * 6.0 - 2.0);
    }
    for v in probes {
        let want = v.rem_euclid(1.0);
        let got = Phase::wrapped(v).get();
        assert!(
            got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
            "wrapped({v:e}) = {got:e}, rem_euclid gives {want:e}"
        );
    }
}

#[test]
fn phase_advances_backwards_without_escaping_the_cycle() {
    // A reverse-rate modulator: the case a sign-preserving wrap breaks.
    let p = Phase(0.1).advance(PhaseIncrement(-0.25));
    assert_eq!(p, Phase(0.85));
    assert!((0.0..1.0).contains(&p.get()));

    // Forward across the seam.
    assert_eq!(
        Phase(0.9).advance(PhaseIncrement(0.25)),
        Phase::wrapped(1.15)
    );

    // Walking a full cycle backwards stays in range at every step.
    let mut cursor = Phase::START;
    for _ in 0..40 {
        cursor = cursor.advance(PhaseIncrement(-0.1));
        assert!((0.0..1.0).contains(&cursor.get()));
    }
}

#[test]
fn phase_increment_derives_from_frequency_and_rate() {
    // One cycle per second at 100 Hz sampling = 1/100 turn per sample.
    assert_eq!(
        PhaseIncrement::per_sample(Hz(1.0), SampleRate::of(100.0)),
        PhaseIncrement(0.01)
    );
}

/// `wrapped_signed` holds `[-pi, pi)` exactly for every finite input, with
/// `pi` the `f64` constant, and is the identity inside it.
///
/// The figures are `x mod 2pi` taken from a 80-digit evaluation, not from
/// the code under test. Up to `2^24` the result is within one `f64` ulp of
/// it; beyond that only the range is asserted, since the `f64` `2pi` no
/// longer holds the turn count.
#[test]
fn wrapped_signed_holds_its_range_exactly() {
    use core::f64::consts::PI;
    let in_range = |w: f64| (-PI..PI).contains(&w);
    let near = |w: f64, want: f64| (w - want).abs() < 3.0e-7;

    // Identity inside, bits and all: both ends of the closed side, the
    // largest value under `pi`, the zeros and the subnormals.
    for x in [
        -PI,
        PI.next_down(),
        0.0,
        -0.0,
        1.0,
        -1.0,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::from_bits(0x007f_ffff),
    ] {
        assert_eq!(Radians(x).wrapped_signed().get().to_bits(), x.to_bits());
    }

    // `+pi` is the excluded end and folds to `-pi`.
    assert_eq!(Radians(PI).wrapped_signed(), Radians(-PI));

    // `x mod 2pi` into `[-pi, pi)`, exact to 80 digits.
    for (x, want) in [
        (3.0 * PI, -3.141_592_629_740_032_3),
        (16_384.0, -2.547_281_124_361_531_6),
        (33_554_432.0, -1.787_937_733_360_393_9),
        (1.0e6, -0.357_564_167_085_735),
        (-1.0e6, 0.357_564_167_085_735),
    ] {
        let w = Radians(x).wrapped_signed().get();
        assert!(in_range(w) && near(w, want), "{x:e} -> {w:e}, want {want}");
    }
    // The value just under `pi` that rounds to `pi` is the excluded end.
    assert_eq!(Radians(-3.0 * PI).wrapped_signed(), Radians(-PI));

    // Every magnitude, up to the largest finite `f64` and down to the
    // subnormals, in both signs.
    for bits in (1..0x7ff0_0000_0000_0000_u64).step_by(0x0003_1000_0000_1003) {
        let x = f64::from_bits(bits);
        for v in [x, -x, x.next_up(), x.next_down()] {
            let w = Radians(v).wrapped_signed().get();
            assert!(in_range(w), "{v:e} -> {w:e}");
        }
    }
    for v in [f64::MAX, -f64::MAX, f64::MAX.next_down(), f64::MIN] {
        let w = Radians(v).wrapped_signed().get();
        assert!(in_range(w), "{v:e} -> {w:e}");
    }
}

/// `Radians` wraps through `wrapped_signed`, never a saturating clamp.
#[test]
fn radians_wrap_signed_rather_than_saturating() {
    let pi = core::f64::consts::PI;

    // A deviation of +3pi/2 is really -pi/2. Keeping the larger number
    // moves a partial the wrong way round the circle.
    assert!((Radians(1.5 * pi).wrapped_signed().get() - (-0.5 * pi)).abs() < 1e-15);
    assert!((Radians(-1.5 * pi).wrapped_signed().get() - (0.5 * pi)).abs() < 1e-15);

    // Already inside: unchanged.
    assert_eq!(Radians(0.0).wrapped_signed(), Radians(0.0));

    // Half-open at +pi, which folds to -pi — the same point on the circle,
    // and the same convention `Phase::wrapped` uses at the top of a cycle.
    assert_eq!(Radians(pi).wrapped_signed(), Radians(-pi));

    // Arithmetic, not iteration: a phase accumulated over minutes of audio
    // wraps in constant time. The `while` loop this replaced ran once per
    // turn, on the audio thread.
    let far = Radians(1.0e6).wrapped_signed().get();
    assert!((-pi..pi).contains(&far), "{far}");
}

#[test]
fn turns_and_radians_convert_both_ways() {
    assert_eq!(Phase(0.5).to_radians(), Radians(core::f64::consts::PI));
    assert_eq!(Phase::START.to_radians(), Radians(0.0));
    assert_eq!(Radians::TAU.to_phase(), Phase::START);
    assert_eq!(Radians(core::f64::consts::PI).to_phase(), Phase(0.5));

    // A quarter turn is the sine peak — the check that the units line up
    // rather than being off by tau somewhere.
    assert_eq!(Phase(0.25).to_radians().sin(), 1.0);
    assert_eq!(Phase::START.to_radians().cos(), 1.0);

    // Degrees reach radians too, so a panner converts once instead of
    // hand-multiplying by pi/180 at each trig call.
    assert_eq!(
        Radians::from(Azimuth(180.0)),
        Radians(core::f64::consts::PI)
    );
    assert_eq!(Radians::from(Elevation::LEVEL), Radians(0.0));
}

#[test]
fn arc_degrees_composes_like_the_displacement_it_is() {
    assert_eq!(ArcDegrees(20.0) + ArcDegrees(15.0), ArcDegrees(35.0));
    assert_eq!(ArcDegrees(20.0) * 0.5, ArcDegrees(10.0));
    assert_eq!(-ArcDegrees(20.0), ArcDegrees(-20.0));
    assert!(ArcDegrees(10.0) < ArcDegrees(20.0));
}

#[test]
fn phase_sin_is_sine_of_a_turn() {
    assert_eq!(Phase::START.sin(), 0.0);
    assert_eq!(Phase(0.25).sin(), 1.0);
    assert_eq!(Phase(0.75).sin(), -1.0);
    assert!(Phase(0.5).sin().abs() < 1e-15);
    assert!((Phase(1.0 / 12.0).sin() - 0.5).abs() < 1e-15);
}

/// The f64 sine of a phase and the f64 sine of its radians are one function
/// to within f64 precision, over a whole turn.
#[test]
fn phase_sin_and_radians_sin_agree_to_f64_precision() {
    for i in 0..=1024u16 {
        let p = Phase(f64::from(i) / 1024.0);
        let (a, b) = (p.sin(), p.to_radians().sin());
        assert!((a - b).abs() < 1e-15, "{p:?}: {a} vs {b}");
    }
}

/// Whether `got` narrowed to `f32` is within `ulps` units in the last place of
/// the `f32` closed form `want`.
fn narrows_to(got: f64, want: f32, ulps: u32) -> bool {
    let narrowed = got as f32;
    let (a, b) = (narrowed.to_bits() as i64, want.to_bits() as i64);
    (a - b).unsigned_abs() <= u64::from(ulps)
}

/// An `f32` signal path narrows an angle converter's `f64` result once; that
/// value is the `f32` closed form to within 2 ulp: the sine of a twelfth of a
/// turn is 1/2, of a quarter of the circle 1, and the cosine of a third of a
/// turn is -1/2.
#[test]
fn the_trig_converters_narrow_to_the_f32_closed_forms() {
    assert!(narrows_to(Phase(1.0 / 12.0).sin(), 0.5, 2));
    assert!(narrows_to(
        Phase(0.125).sin(),
        core::f32::consts::FRAC_1_SQRT_2,
        2
    ));
    assert!(narrows_to(
        Radians(core::f64::consts::FRAC_PI_6).sin(),
        0.5,
        2
    ));
    assert!(narrows_to(
        Radians(core::f64::consts::FRAC_PI_3).cos(),
        0.5,
        2
    ));
    assert!(narrows_to(
        Radians(2.0 * core::f64::consts::FRAC_PI_3).cos(),
        -0.5,
        2
    ));
}

/// Degrees reach radians as the `f32` constants `pi/2`, `pi` and `pi/6`, to
/// within 1 ulp once narrowed.
#[test]
fn degrees_narrow_to_the_f32_radian_constants() {
    use core::f32::consts::{FRAC_PI_2, FRAC_PI_6, PI};
    assert!(narrows_to(Radians::from(Azimuth(90.0)).get(), FRAC_PI_2, 1));
    assert!(narrows_to(Radians::from(Azimuth(180.0)).get(), PI, 1));
    assert!(narrows_to(
        Radians::from(Elevation(30.0)).get(),
        FRAC_PI_6,
        1
    ));
}

/// The wraps and the phase step narrow to the `f32` figures: 190 degrees is
/// -170, a quarter-turn offset from a half turn is 0.75, and 1 kHz at 48 kHz steps
/// `1/48` of a turn.
#[test]
fn the_wraps_and_the_phase_step_narrow_to_the_f32_figures() {
    assert!(narrows_to(Azimuth(190.0).wrap().get(), -170.0, 0));
    assert!(narrows_to(
        Azimuth(170.0).shortest_arc_to(Azimuth(-170.0)).get(),
        20.0,
        0
    ));
    assert!(narrows_to(
        Phase(0.5).offset_by(PhaseIncrement(0.25)).get(),
        0.75,
        0
    ));
    // 7 - 2pi = 0.716_814_692_8 (from a 20-digit evaluation).
    assert!(narrows_to(
        Radians(7.0).wrapped_signed().get(),
        0.716_814_7,
        1
    ));
    let step = PhaseIncrement::per_sample(Hz(1000.0), SampleRate::SR_48K);
    assert!(narrows_to(step.get(), 1.0 / 48.0, 1));
}

const CYCLE: f64 = 18_446_744_073_709_551_616.0;

#[test]
fn running_phase_at_a_quarter_turn_is_two_to_the_62() {
    assert_eq!(RunningPhase::at(Phase(0.25)).get(), 1_u64 << 62);
    assert_eq!(RunningPhase::at(Phase(0.5)).get(), 1_u64 << 63);
    assert_eq!(RunningPhase::at(Phase::START), RunningPhase::START);
}

#[test]
fn running_phase_at_wraps_a_setting_and_holds_a_non_finite_one_at_the_start() {
    assert_eq!(RunningPhase::at(Phase(1.25)).get(), 1_u64 << 62);
    assert_eq!(RunningPhase::at(Phase(-0.25)).get(), 3 * (1_u64 << 62));
    assert_eq!(RunningPhase::at(Phase(f64::NAN)), RunningPhase::START);
    assert_eq!(RunningPhase::at(Phase(f64::INFINITY)), RunningPhase::START);
}

#[test]
fn a_step_of_two_to_the_k_wraps_to_the_start_after_two_to_the_64_minus_k_steps() {
    // 2^k * 2^(64-k) = 2^64 = 0 mod 2^64, and one step fewer is 2^64 - 2^k, not 0.
    for k in [40_u32, 52, 60, 63] {
        let step = PhaseStep::from_count(1_u64 << k);
        let n = 1_u64 << (64 - k);
        let mut p = RunningPhase::START;
        for _ in 0..n - 1 {
            p = p.advance(step);
        }
        assert_eq!(p.get(), (1_u64 << k).wrapping_neg(), "k {k}");
        assert_eq!(p.advance(step), RunningPhase::START, "k {k}");
    }
}

#[test]
fn n_steps_are_n_times_the_step_modulo_two_to_the_64() {
    let step = PhaseStep::from_count(0x0123_4567_89ab_cdef);
    let n = 1_000_003_u64;
    let mut p = RunningPhase::at(Phase(0.125));
    for _ in 0..n {
        p = p.advance(step);
    }
    let expect = (1_u64 << 61).wrapping_add(n.wrapping_mul(0x0123_4567_89ab_cdef));
    assert_eq!(p.get(), expect);
}

#[test]
fn a_negative_increment_steps_backwards_by_its_complement() {
    let fwd = PhaseStep::from(PhaseIncrement(0.125));
    let back = PhaseStep::from(PhaseIncrement(-0.125));
    assert_eq!(fwd.get(), 1_u64 << 61);
    assert_eq!(fwd.get().wrapping_add(back.get()), 0);
    let p = RunningPhase::at(Phase(0.5)).advance(back);
    assert_eq!(p.get(), 3 * (1_u64 << 61));
}

#[test]
fn a_non_finite_increment_is_a_held_phase() {
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(PhaseStep::from(PhaseIncrement(x)), PhaseStep::ZERO);
    }
}

#[test]
fn a_slow_lfo_step_is_exact_to_one_count_and_a_period_returns_to_the_start() {
    // 0.01 Hz at 192 kHz: a period is 19.2e6 samples.
    let inc = PhaseIncrement::per_sample(Hz(0.01), SampleRate::new(192_000.0).unwrap());
    let step = PhaseStep::from(inc);
    // Within one count (2^-64 cycles, truncation) of the exact rational 1 / 19.2e6 of a cycle.
    let exact = CYCLE / 19_200_000.0;
    assert!((step.get() as f64 - exact).abs() <= 1.0, "{}", step.get());
    let mut p = RunningPhase::START;
    for _ in 0..19_200_000_u64 {
        p = p.advance(step);
    }
    // 19.2e6 steps carry at most 19.2e6 counts of error: 1e-12 of a cycle.
    let residue = p.get().min(p.get().wrapping_neg());
    assert!(
        residue <= 19_200_000,
        "{residue} counts away from the start"
    );
}

#[test]
fn an_increment_of_a_cycle_and_a_quarter_steps_a_quarter() {
    assert_eq!(PhaseStep::from(PhaseIncrement(1.25)).get(), 1_u64 << 62);
}

#[test]
fn an_increment_of_half_a_cycle_is_two_to_the_63() {
    assert_eq!(PhaseStep::from(PhaseIncrement(0.5)).get(), 1_u64 << 63);
}

#[test]
fn a_negative_increment_steps_back_by_the_two_s_complement_of_the_positive_step() {
    for x in [1e-20, 2f64.powi(-40), 1.0 / 19.2e6, 0.125] {
        assert_eq!(
            PhaseStep::from(PhaseIncrement(-x)).get(),
            PhaseStep::from(PhaseIncrement(x)).get().wrapping_neg(),
            "{x}"
        );
    }
    // `-2^-60` cycles is exactly 16 counts back.
    assert_eq!(
        PhaseStep::from(PhaseIncrement(-(2f64.powi(-60)))).get(),
        0_u64.wrapping_sub(16)
    );
}
