//! Each law against its closed form. The spec is the oracle: the expected
//! figures are written out here, not read from another implementation.

use tutti_units::Normalized;

use super::Law;
use crate::{Curvature, Exponent, Piece};

const LAWS: [Law; 9] = [
    Law::Linear,
    Law::Hold,
    Law::PowerIn(Exponent::of(2.0)),
    Law::PowerOut(Exponent::of(3.0)),
    Law::PowerInOut(Exponent::of(2.0)),
    Law::PowerIn(Exponent::of(1.7)),
    Law::Exp(Curvature::of(3.0)),
    Law::Exp(Curvature::of(-2.5)),
    Law::Smootherstep,
];

fn at(t: f64) -> Normalized {
    Normalized::of(t)
}

fn g(law: Law, t: f64) -> f64 {
    law.eval(at(t)).get()
}

/// Every law starts at 0 and ends at 1, and the midpoints that are binary
/// fractions are exact: a quadratic's is a quarter, the smootherstep's is a
/// half, and an in-out power is half at the middle.
#[test]
fn every_law_runs_from_0_to_1_and_the_dyadic_midpoints_are_exact() {
    for law in LAWS {
        assert_eq!(g(law, 0.0), 0.0, "{law:?} at 0");
        assert_eq!(g(law, 1.0), 1.0, "{law:?} at 1");
    }
    assert_eq!(g(Law::Linear, 0.5), 0.5);
    assert_eq!(g(Law::Hold, 0.5), 0.0);
    assert_eq!(g(Law::PowerIn(Exponent::of(2.0)), 0.5), 0.25);
    assert_eq!(g(Law::PowerOut(Exponent::of(2.0)), 0.5), 0.75);
    assert_eq!(g(Law::PowerInOut(Exponent::of(2.0)), 0.5), 0.5);
    assert_eq!(g(Law::Smootherstep, 0.5), 0.5);
}

/// The unit steps: `Linear` is `t`, `Hold` is 0 until the end.
#[test]
fn linear_is_t_and_hold_steps_at_the_end() {
    for t in [0.0, 0.125, 0.5, 0.75, 1.0] {
        assert_eq!(g(Law::Linear, t), t);
    }
    for t in [0.0, 0.25, 0.75, 1.0_f64.next_down()] {
        assert_eq!(g(Law::Hold, t), 0.0, "Hold at {t}");
    }
    assert_eq!(g(Law::Hold, 1.0), 1.0);
}

/// The integer exponents are multiplications: exact in binary fractions.
/// `t^p` at 1/2 is `2^-p`; `1 - (1 - t)^p` at 1/2 is `1 - 2^-p`; the
/// in-out law is `2^(p-1) t^p` below 1/2, so at 1/4 it is `2^(p-1) 4^-p`.
#[test]
fn integer_powers_are_exact() {
    for (p, p_in) in [
        (1.0, 0.5),
        (2.0, 0.25),
        (3.0, 0.125),
        (4.0, 0.0625),
        (5.0, 0.03125),
    ] {
        assert_eq!(g(Law::PowerIn(Exponent::of(p)), 0.5), p_in, "PowerIn({p})");
        assert_eq!(
            g(Law::PowerOut(Exponent::of(p)), 0.5),
            1.0 - p_in,
            "PowerOut({p}) at 1/2"
        );
    }
    for (p, at_quarter) in [
        (2.0, 0.125),
        (3.0, 0.0625),
        (4.0, 0.03125),
        (5.0, 0.015_625),
    ] {
        let law = Law::PowerInOut(Exponent::of(p));
        assert_eq!(g(law, 0.25), at_quarter, "PowerInOut({p}) at 1/4");
        assert_eq!(g(law, 0.75), 1.0 - at_quarter, "PowerInOut({p}) at 3/4");
        assert_eq!(g(law, 0.5), 0.5, "PowerInOut({p}) at 1/2");
    }
}

/// An exponent of one half is the square root (exact: `sqrt(1/4) = 1/2`), and
/// any other exponent is `t^p` to within a few `f64` ulps.
#[test]
fn other_exponents_follow_the_power() {
    assert_eq!(g(Law::PowerIn(Exponent::of(0.5)), 0.25), 0.5);
    for (p, t) in [(1.7, 0.3), (0.3, 0.9), (7.5, 0.6)] {
        let want = f64::powf(t, p);
        assert!(
            (g(Law::PowerIn(Exponent::of(p)), t) - want).abs() < 1e-15,
            "PowerIn({p}) at {t}"
        );
        let want = 1.0 - f64::powf(1.0 - t, p);
        assert!(
            (g(Law::PowerOut(Exponent::of(p)), t) - want).abs() < 1e-15,
            "PowerOut({p}) at {t}"
        );
    }
}

/// `Exp(k)` is `(e^(kt) - 1) / (e^k - 1)`, so with `k = ln(to / from)` the
/// value is geometric: `from · (to / from)^t`.
#[test]
fn exp_is_the_exponential_and_geometric_at_log_ratio() {
    for k in [3.0f64, -2.5, 0.5, 20.0] {
        for t in [0.1f64, 0.5, 0.9] {
            let want = (k * t).exp_m1() / k.exp_m1();
            assert!(
                (g(Law::Exp(Curvature::of(k)), t) - want).abs() < 1e-15,
                "Exp({k}) at {t}"
            );
        }
    }
    let (from, to) = (100.0f64, 1600.0f64);
    let k = (to / from).ln();
    for t in [0.25, 0.5, 0.75] {
        let want = from * (to / from).powf(t);
        let got = from + (to - from) * g(Law::Exp(Curvature::of(k)), t);
        assert!(
            (got - want).abs() < 1e-12 * want,
            "geometric at {t}: {got} vs {want}"
        );
    }
}

/// `Exp` near zero curvature is linear: `(e^(kt) - 1) / (e^k - 1)` is
/// `t (1 + k (t - 1) / 2)` to first order in `k`, never `0 / 0`; at `k = 0`
/// it is `t` exactly.
#[test]
fn exp_near_zero_curvature_is_linear() {
    assert_eq!(g(Law::Exp(Curvature::ZERO), 0.3), 0.3);
    assert_eq!(g(Law::Exp(Curvature::of(-0.0)), 0.3), 0.3);
    for k in [1e-3, -1e-3, 1e-9, -1e-12, 1e-300] {
        let got = g(Law::Exp(Curvature::of(k)), 0.3);
        assert!((got - 0.3).abs() <= k.abs(), "Exp({k}): {got}");
    }
}

/// The extreme curvatures a `Curvature` holds are evaluated as they are, not
/// clamped, and the ratio never reaches `inf / inf`.
#[test]
fn the_extreme_curvatures_stay_finite_and_monotone() {
    for k in [Curvature::MAX, -Curvature::MAX] {
        let law = Law::Exp(Curvature::of(k));
        let mut last = 0.0;
        for i in 0..=100 {
            let v = g(law, f64::from(i) / 100.0);
            assert!(
                v.is_finite() && v >= last && v <= 1.0,
                "Exp({k}) at {i}: {v}"
            );
            last = v;
        }
        assert_eq!(g(law, 1.0), 1.0);
    }
}

/// A progress is a `Normalized`, so the law never sees an out-of-range or
/// NaN one: `Normalized` refuses it, and `clamped` pins it (a NaN is 0), which
/// is what every caller that builds a progress from a quotient uses.
#[test]
fn a_progress_outside_the_unit_interval_is_refused_or_clamped() {
    assert_eq!(Normalized::new(-0.5), None);
    assert_eq!(Normalized::new(2.0), None);
    assert_eq!(Normalized::new(f64::NAN), None);
    for law in LAWS {
        assert_eq!(
            law.eval(Normalized::clamped(-0.5)),
            law.eval(at(0.0)),
            "{law:?}"
        );
        assert_eq!(
            law.eval(Normalized::clamped(2.0)),
            law.eval(at(1.0)),
            "{law:?}"
        );
        assert_eq!(law.eval(Normalized::clamped(f64::NAN)), at(0.0), "{law:?}");
    }
}

/// Every law meets its closed form (`f64`, within a few ulps; `Exp` and a
/// non-integer power go through libm) and is monotone. The smootherstep's
/// Horner form is monotone to the last bit and stays within `[0, 1]` near 1,
/// where its terms cancel.
#[test]
fn every_law_meets_its_closed_form() {
    use Law as S;
    let closed = |s: S, t: f64| -> f64 {
        match s {
            S::Linear => t,
            S::Hold => f64::from(u8::from(t >= 1.0)),
            S::PowerIn(p) => t.powf(p.get()),
            S::PowerOut(p) => 1.0 - (1.0 - t).powf(p.get()),
            S::PowerInOut(p) if t < 0.5 => 2f64.powf(p.get() - 1.0) * t.powf(p.get()),
            S::PowerInOut(p) => 1.0 - (2.0 - 2.0 * t).powf(p.get()) / 2.0,
            S::Exp(k) => ((k.get() * t).exp() - 1.0) / (k.get().exp() - 1.0),
            S::Smootherstep => t * t * t * (6.0 * t * t - 15.0 * t + 10.0),
        }
    };
    let shapes = [
        S::Linear,
        S::Hold,
        S::PowerIn(Exponent::of(2.0)),
        S::PowerIn(Exponent::of(0.5)),
        S::PowerIn(Exponent::of(5.0)),
        S::PowerIn(Exponent::of(1.7)),
        S::PowerOut(Exponent::of(3.0)),
        S::PowerInOut(Exponent::of(2.0)),
        S::PowerInOut(Exponent::of(4.0)),
        S::Exp(Curvature::of(5.0)),
        S::Smootherstep,
        S::Exp(Curvature::of(-3.0)),
    ];
    for s in shapes {
        let mut prev = 0.0f64;
        for i in 0..=1000 {
            let t = f64::from(i) / 1000.0;
            let v = g(s, t);
            assert!(
                (v - closed(s, t)).abs() < 1e-12,
                "{s:?} at {t}: {v} vs {}",
                closed(s, t)
            );
            assert!((0.0..=1.0).contains(&v), "{s:?} at {t}: {v}");
            assert!(v >= prev, "{s:?} is monotone at {t}");
            prev = v;
        }
    }
}

/// `Linear` returns its progress bit for bit, so a linear [`Piece`] is
/// `from + (to - from) * t` with `t` narrowed once to the `f32` endpoint
/// words: the narrowing test of the `f32` boundary.
#[test]
fn a_linear_piece_narrows_the_law_once_to_the_f32_words() {
    for n in [1u32, 3, 7, 64, 97, 128, 1000] {
        for i in 0..n {
            let t = at(f64::from(i + 1) / f64::from(n));
            assert_eq!(Law::Linear.eval(t), t);
            let (from, to) = (0.3f32, -7.9f32);
            assert_eq!(
                Piece::new(from, to, Law::Linear).value(None, t).to_bits(),
                (from + (to - from) * (t.get() as f32)).to_bits()
            );
        }
    }
}

/// `Exp(k)` at the midpoint is `1 / (e^(k/2) + 1)`, since
/// `(e^(k/2) - 1) / (e^k - 1)` factors with `e^k - 1 = (e^(k/2) - 1)(e^(k/2) + 1)`.
/// A power law at `1/4` with exponent `1.5` is `(1/2)^3 = 1/8`, and at `1/2`
/// with exponent `2.5` it is `2^-2.5`.
#[test]
fn exp_midpoint_and_fractional_power_meet_their_closed_forms() {
    for k in [4.0f64, -4.0, 1.0, 12.0] {
        let want = 1.0 / ((k / 2.0).exp() + 1.0);
        let got = g(Law::Exp(Curvature::of(k)), 0.5);
        assert!(
            (got - want).abs() < 1e-15,
            "Exp({k}) midpoint: {got} vs {want}"
        );
    }
    let got = g(Law::PowerIn(Exponent::of(1.5)), 0.25);
    assert!((got - 0.125).abs() < 1e-16, "0.25^1.5: {got}");
    let got = g(Law::PowerIn(Exponent::of(2.5)), 0.5);
    let want = 2f64.powf(-2.5);
    assert!((got - want).abs() < 1e-16, "0.5^2.5: {got} vs {want}");
}
