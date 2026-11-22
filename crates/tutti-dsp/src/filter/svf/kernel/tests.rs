use super::*;
use crate::SvfControls;

/// The design of the controls `(ty, freq, q, gain)` at `rate`, which must be in range.
fn designed(ty: SvfType, freq: Hz, q: Q, gain: Db, rate: SampleRate) -> SvfDesign {
    SvfControls { ty, freq, q, gain }.design(rate).unwrap()
}

/// The held design of the controls `(ty, freq, q, gain)` at `rate`.
fn clamped(ty: SvfType, freq: Hz, q: Q, gain: Db, rate: SampleRate) -> SvfDesign {
    SvfControls { ty, freq, q, gain }.design_clamped(rate)
}

#[test]
fn test_svf_coeffs_lowpass_m_values() {
    let c = designed(
        SvfType::LowPass,
        Hz::new(1000.0),
        Q::new(0.707),
        Db::new(0.0),
        SampleRate::SR_44K1,
    );
    assert!((c.m0 - 0.0).abs() < 1e-10);
    assert!((c.m1 - 0.0).abs() < 1e-10);
    assert!((c.m2 - 1.0).abs() < 1e-10);
    assert!(c.d > 0.0 && c.d < 1.0);
}

#[test]
fn test_svf_coeffs_highpass_m_values() {
    let c = designed(
        SvfType::HighPass,
        Hz::new(1000.0),
        Q::new(0.707),
        Db::new(0.0),
        SampleRate::SR_44K1,
    );
    assert!((c.m0 - 1.0).abs() < 1e-10);
    assert!((c.m2 - (-1.0)).abs() < 1e-10);
}

/// The RBJ cookbook biquad's magnitude (dB) at `f`, closed form.
fn rbj_db(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64, f: f64) -> f64 {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * core::f64::consts::PI * fc / rate;
    let (sn, cs) = w0.sin_cos();
    let al = sn / (2.0 * q);
    let sa = 2.0 * a.sqrt() * al;
    let (b, d) = match ty {
        SvfType::Bell => (
            [1.0 + al * a, -2.0 * cs, 1.0 - al * a],
            [1.0 + al / a, -2.0 * cs, 1.0 - al / a],
        ),
        SvfType::LowShelf => (
            [
                a * ((a + 1.0) - (a - 1.0) * cs + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                a * ((a + 1.0) - (a - 1.0) * cs - sa),
            ],
            [
                (a + 1.0) + (a - 1.0) * cs + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                (a + 1.0) + (a - 1.0) * cs - sa,
            ],
        ),
        SvfType::HighShelf => (
            [
                a * ((a + 1.0) + (a - 1.0) * cs + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                a * ((a + 1.0) + (a - 1.0) * cs - sa),
            ],
            [
                (a + 1.0) - (a - 1.0) * cs + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cs),
                (a + 1.0) - (a - 1.0) * cs - sa,
            ],
        ),
        _ => unreachable!(),
    };
    let w = 2.0 * core::f64::consts::PI * f / rate;
    let mag = |c: [f64; 3]| {
        let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
        let im = -(c[1] * w.sin() + c[2] * (2.0 * w).sin());
        (re * re + im * im).sqrt()
    };
    20.0 * (mag(b) / mag(d)).log10()
}

/// Steady-state gain (dB) of a unit sine at `f` through the filter's own
/// recurrence, projected on the last whole second.
fn measured_db(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64, f: f64) -> f64 {
    let c = SvfCoefficients::<f64>::from(&designed(
        ty,
        Hz::new(fc),
        Q::new(q),
        Db::new(gain_db),
        SampleRate::of(rate),
    ));
    let mut st = [0.0f64; 2];
    let n = rate as usize;
    let (mut re, mut im) = (0.0, 0.0);
    for i in 0..2 * n {
        let ph = 2.0 * core::f64::consts::PI * f * i as f64 / rate;
        let y = svf_step(&c, &mut st, ph.sin());
        if i >= n {
            re += y * ph.sin();
            im += y * ph.cos();
        }
    }
    20.0 * (2.0 * (re * re + im * im).sqrt() / n as f64).log10()
}

/// Bell and both shelves follow the RBJ cookbook response at fc/2, fc and
/// 2fc, for +-12 dB and Q 0.7, 1, 4; the shelf gain at fc is half the dB
/// gain.
#[test]
fn bell_and_shelves_match_the_rbj_cookbook() {
    let (rate, fc) = (48000.0, 1000.0);
    for ty in [SvfType::Bell, SvfType::LowShelf, SvfType::HighShelf] {
        for gain in [12.0, -12.0] {
            for q in [0.7, 1.0, 4.0] {
                for f in [fc / 2.0, fc, 2.0 * fc] {
                    let want = rbj_db(ty, fc, q, gain, rate, f);
                    let got = measured_db(ty, fc, q, gain, rate, f);
                    assert!(
                        (got - want).abs() < 0.05,
                        "{ty:?} {gain} dB Q {q} at {f} Hz: got {got:.3}, RBJ {want:.3}"
                    );
                }
            }
        }
    }
    for ty in [SvfType::LowShelf, SvfType::HighShelf] {
        let at_fc = measured_db(ty, fc, core::f64::consts::FRAC_1_SQRT_2, 12.0, rate, fc);
        assert!((at_fc - 6.0).abs() < 0.05, "{ty:?} at fc: {at_fc}");
    }
}

/// A gain far outside any audible range, or not a number, still solves to
/// a finite set (it was NaN at -1e30 dB: `10^(dB / 40)` underflows to 0
/// and `k = 1 / (Q * A)` is infinite), and a gain past the cap is the cap's. `design_clamped` is the call that holds it.
#[test]
fn an_absurd_gain_solves_finite_and_saturates() {
    for ty in [SvfType::Bell, SvfType::LowShelf, SvfType::HighShelf] {
        for db in [f64::NAN, -1e30, 1e30, f64::NEG_INFINITY, f64::INFINITY] {
            let c = clamped(
                ty,
                Hz::new(1000.0),
                Q::new(1.0),
                Db::new(db),
                SampleRate::SR_48K,
            );
            for v in [c.d, c.a2, c.a3, c.m0, c.m1, c.m2] {
                assert!(v.is_finite(), "{ty:?} at {db} dB: {c:?}");
            }
        }
        let cap = clamped(
            ty,
            Hz::new(1000.0),
            Q::new(1.0),
            SvfDesign::MAX_GAIN,
            SampleRate::SR_48K,
        );
        let past = clamped(
            ty,
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(1e30),
            SampleRate::SR_48K,
        );
        assert_eq!((cap.m0, cap.m1, cap.m2), (past.m0, past.m1, past.m2));
    }
    let nan = clamped(
        SvfType::Bell,
        Hz::new(1000.0),
        Q::new(1.0),
        Db::new(f64::NAN),
        SampleRate::SR_48K,
    );
    let flat = clamped(
        SvfType::Bell,
        Hz::new(1000.0),
        Q::new(1.0),
        Db::new(0.0),
        SampleRate::SR_48K,
    );
    assert_eq!(nan.m1, flat.m1);
}

/// `design` refuses what `design_clamped` holds: a `Q` below `MIN_Q` or not finite, a gain past
/// `MAX_GAIN` or not finite; the bounds themselves are solved.
#[test]
fn new_refuses_a_q_or_gain_out_of_range() {
    let new = |q: f64, db: f64| {
        SvfControls {
            ty: SvfType::Bell,
            freq: Hz::new(1000.0),
            q: Q::new(q),
            gain: Db::new(db),
        }
        .design(SampleRate::SR_48K)
    };
    for q in [0.0, -1.0, 0.0099, f64::NAN, f64::INFINITY] {
        assert!(matches!(new(q, 0.0), Err(SvfDesignError::Q(_))), "Q {q}");
    }
    for db in [
        200.0001,
        -200.0001,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert!(
            matches!(new(1.0, db), Err(SvfDesignError::Gain(_))),
            "{db} dB"
        );
    }
    assert!(new(crate::SvfControls::MIN_Q, 200.0).is_ok());
    assert!(new(1.0, -200.0).is_ok());
}

#[test]
fn test_svf_coeffs_bell_zero_gain_is_unity() {
    let c = designed(
        SvfType::Bell,
        Hz::new(1000.0),
        Q::new(1.0),
        Db::new(0.0),
        SampleRate::SR_44K1,
    );
    assert!(
        (c.m1).abs() < 1e-10,
        "Bell with 0dB gain should have m1≈0, got {}",
        c.m1
    );
}

/// Cutoff and Q are not interchangeable, and the types are what say so.
///
/// As adjacent floats, `(freq, q)` and `(q, freq)` both compile and the
/// second silently detunes the filter. Passing them as `Hz` and `Q` makes
/// the transposition a type error — this pins the difference the swap would
/// make, so the two are never quietly given one type again.
#[test]
fn cutoff_and_q_are_not_interchangeable() {
    let right = designed(
        SvfType::LowPass,
        Hz::new(1000.0),
        Q::new(0.707),
        Db::UNITY,
        SampleRate::SR_44K1,
    );
    // What the transposed call computes: a 0.707 Hz cutoff at Q 1000.
    let swapped = designed(
        SvfType::LowPass,
        Hz::new(0.707),
        Q::new(1000.0),
        Db::UNITY,
        SampleRate::SR_44K1,
    );
    assert!(
        (right.a2 - swapped.a2).abs() > 1e-6,
        "the swap has to be observable, or this test proves nothing"
    );
}

#[test]
fn test_svf_coeffs_deterministic() {
    let c1 = designed(
        SvfType::LowPass,
        Hz::new(500.0),
        Q::new(0.707),
        Db::new(0.0),
        SampleRate::SR_44K1,
    );
    let c2 = designed(
        SvfType::LowPass,
        Hz::new(500.0),
        Q::new(0.707),
        Db::new(0.0),
        SampleRate::SR_44K1,
    );
    assert_eq!(c1.d, c2.d);
    assert_eq!(c1.a2, c2.a2);
    assert_eq!(c1.m0, c2.m0);
}

// =========================================================================
// Width
// =========================================================================
