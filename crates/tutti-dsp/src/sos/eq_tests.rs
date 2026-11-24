use core::f64::consts::PI;

use super::*;
use crate::{Complex64, DecayTime};
use tutti_units::{Amplitude, Q};

const RATE: SampleRate = SampleRate::SR_48K;

fn magnitude(b: &Biquad, f: f64) -> f64 {
    b.response(2.0 * PI * f / RATE.get()).norm()
}

/// The analog bell `(s^2 + s A/Q + 1) / (s^2 + s / (A Q) + 1)` at `s = j f / fc`, in dB.
fn analog_bell_db(fc: f64, q: f64, db: f64, f: f64) -> f64 {
    let a = 10f64.powf(db / 40.0);
    let x = f / fc;
    let off = 1.0 - x * x;
    let num = off * off + (a * x / q).powi(2);
    let den = off * off + (x / (a * q)).powi(2);
    10.0 * (num / den).log10()
}

/// The cookbook's bilinear bell at `fc`, `q` and `db`.
fn cookbook_bell(fc: f64, q: f64, db: f64) -> Biquad {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * PI * fc / RATE.get();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha / a;
    Biquad {
        b0: (1.0 + alpha * a) / a0,
        b1: -2.0 * w0.cos() / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: -2.0 * w0.cos() / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

fn bell(fc: f64, q: f64, db: f64) -> Biquad {
    MatchedBell::new(Q::new(q), Db::new(db))
        .design(Hz::new(fc), RATE)
        .unwrap()
        .sections()[0]
}

const CENTERS: [f64; 7] = [60.0, 250.0, 1000.0, 5000.0, 12000.0, 19200.0, 22000.0];
const QS: [f64; 5] = [0.3, 0.707, 2.0, 8.0, 25.0];
const GAINS: [f64; 6] = [-24.0, -12.0, -3.0, 3.0, 12.0, 24.0];

/// The squared analog tilt `(A^2 x^2 + 1) / (x^2 + A^2)` for `A = 10^(db / 40)`, at the
/// frequency ratio `x`, as a magnitude.
fn analog_tilt(db: f64, x: f64) -> f64 {
    let a2 = 10f64.powf(db / 20.0);
    ((a2 * x * x + 1.0) / (x * x + a2)).sqrt()
}

/// The ratio the bilinear transform prewarped at `fc` gives frequency `f`.
fn warped(fc: f64, f: f64) -> f64 {
    (PI * f / RATE.get()).tan() / (PI * fc / RATE.get()).tan()
}

fn tilt(fc: f64, db: f64) -> Biquad {
    Tilt::new(Db::new(db))
        .design(Hz::new(fc), RATE)
        .unwrap()
        .sections()[0]
}

/// The matched bell is the analog one at DC, at the center and at Nyquist, in dB: to 1e-9
/// at DC and Nyquist and to 1e-5 at the center (where the rounding of the `f64`
/// coefficients of a pole this near `z = 1` at 60 Hz sets the floor), for centers up to 22 kHz, Q 0.3 to 25 and gains of 3 to 24 dB.
#[test]
fn the_matched_bell_equals_the_analog_magnitude_at_dc_the_center_and_nyquist() {
    for fc in CENTERS {
        for q in QS {
            for db in GAINS {
                let b = bell(fc, q, db);
                let (fc, q, db) = (fc, q, db);
                let at = |f: f64| {
                    Amplitude::new(magnitude(&b, f)).to_db().get() - analog_bell_db(fc, q, db, f)
                };
                let tag = format!("fc {fc} q {q} gain {db}");
                assert!(at(0.0).abs() < 1e-9, "{tag} DC: {}", at(0.0));
                assert!(at(fc).abs() < 1e-5, "{tag} center: {}", at(fc));
                assert!(
                    at(RATE.get() / 2.0).abs() < 1e-9,
                    "{tag} Nyquist: {}",
                    at(RATE.get() / 2.0)
                );
            }
        }
    }
}

/// The most the dB magnitude of `b` is from the analog bell's, over 100 Hz steps up to 0.9
/// Nyquist.
fn worst_deviation(b: &Biquad, fc: f64, q: f64, db: f64) -> f64 {
    (1..)
        .map(|i| f64::from(i) * 100.0)
        .take_while(|f| *f <= 0.9 * RATE.get() / 2.0)
        .map(|f| {
            (Amplitude::new(magnitude(b, f)).to_db().get() - analog_bell_db(fc, q, db, f)).abs()
        })
        .fold(0.0, f64::max)
}

/// The stated bound in dB of each center's grid, in Hz.
const BOUNDS: [(f64, f64); 5] = [
    (250.0, 0.05),
    (1000.0, 0.4),
    (5000.0, 1.0),
    (12000.0, 1.1),
    (19200.0, 2.8),
];

/// Between the three matched points the bell follows the analog one to within a bound that
/// grows with the center (the analog bell reaches past Nyquist and a second-order section
/// cannot follow it): in dB up to 0.9 Nyquist, over Q 0.3 to 25 and gains of 3 to 24 dB. The
/// bounds are stated limits, measured with about 10% slack; the exact property is the match
/// at the three points above. A bound that the cookbook's bilinear bell also meets does not
/// discriminate, so every row is one the cookbook bell exceeds.
#[test]
fn the_matched_bell_stays_within_a_stated_bound_of_the_analog_one_to_0_9_nyquist() {
    for (fc, bound) in BOUNDS {
        for q in QS {
            for db in GAINS {
                let worst = worst_deviation(&bell(fc, q, db), fc, q, db);
                assert!(
                    worst <= bound,
                    "fc {fc} q {q} gain {db}: {worst} dB > {bound}"
                );
            }
        }
    }
}

/// The cookbook's bilinear bell, cramped near Nyquist, leaves every row's bound of the test
/// above somewhere in its grid, while the matched bell holds it: the bound can fail.
#[test]
fn the_bilinear_bell_leaves_every_bound_the_matched_bell_holds() {
    for (fc, bound) in BOUNDS {
        let cookbook = QS
            .iter()
            .flat_map(|q| GAINS.iter().map(move |db| (*q, *db)))
            .map(|(q, db)| {
                let (fc, q, db) = (fc, q, db);
                worst_deviation(&cookbook_bell(fc, q, db), fc, q, db)
            })
            .fold(0.0, f64::max);
        assert!(
            cookbook > bound,
            "fc {fc}: the cookbook bell's {cookbook} dB is within {bound}"
        );
    }
}

/// A bell of 0 dB is a wire: its numerator is its denominator to 1e-12 (the closed form
/// reaches it through a square root).
#[test]
fn a_bell_of_zero_gain_is_the_identity() {
    for fc in CENTERS {
        for q in QS {
            let b = bell(fc, q, 0.0);
            let diffs = [b.b0 - 1.0, b.b1 - b.a1, b.b2 - b.a2];
            assert!(
                diffs.iter().all(|d| d.abs() < 1e-12),
                "fc {fc} q {q}: {b:?}"
            );
        }
    }
}

/// Every pole and zero is inside the unit circle: the filter is stable and its inverse is.
#[test]
fn the_bell_poles_and_zeros_are_inside_the_unit_circle() {
    for fc in CENTERS {
        for q in QS {
            for db in GAINS {
                let b = bell(fc, q, db);
                assert!(b.spectral_radius() < 1.0, "fc {fc} q {q} gain {db}: {b:?}");
                let zeros = Biquad {
                    a1: b.b1 / b.b0,
                    a2: b.b2 / b.b0,
                    ..b
                };
                assert!(
                    zeros.spectral_radius() < 1.0,
                    "fc {fc} q {q} gain {db}: zeros"
                );
            }
        }
    }
}

/// A boost and the cut of the same gain multiply to unity at every frequency (the analog
/// cut is the reciprocal of the analog boost), in magnitude and in phase.
#[test]
fn a_boost_and_the_cut_of_the_same_gain_cancel() {
    for fc in CENTERS {
        for q in QS {
            for db in [3.0f64, 12.0, 24.0] {
                let (up, down) = (bell(fc, q, db), bell(fc, q, -db));
                for f in [200.0, 1500.0, 9000.0, 17000.0, 23500.0] {
                    let w = 2.0 * PI * f / RATE.get();
                    let product = up.response(w) * down.response(w);
                    assert!(
                        (product - Complex64::new(1.0, 0.0)).norm() < 1e-9,
                        "fc {fc} q {q} gain {db} at {f}: {product}"
                    );
                }
            }
        }
    }
}

/// A center at or past Nyquist, at or below 0 or not a number, a Q that is not above 0, and
/// a gain past 120 dB are refused, and the design `into` is left as it was.
#[test]
fn the_designs_refuse_what_the_rate_or_the_range_cannot_take() {
    let nyquist = Hz::new(24000.0);
    for center in [24000.0f64, 30000.0, 0.0, -10.0, f64::NAN] {
        let err = MatchedBell::new(Q::new(1.0), Db::new(6.0)).design(Hz::new(center), RATE);
        assert!(
            matches!(err, Err(SosError::CutoffOutOfRange { nyquist: n, .. }) if n == nyquist),
            "bell {center}: {err:?}"
        );
        let err = Tilt::new(Db::new(6.0)).design(Hz::new(center), RATE);
        assert!(
            matches!(err, Err(SosError::CutoffOutOfRange { .. })),
            "tilt {center}"
        );
    }
    for q in [0.0f64, -1.0, f64::NAN, f64::INFINITY] {
        let err = MatchedBell::new(Q::new(q), Db::new(6.0)).design(Hz::new(1000.0), RATE);
        assert!(
            matches!(err, Err(SosError::QOutOfRange { .. })),
            "q {q}: {err:?}"
        );
    }
    for gain in [121.0f64, -200.0, f64::NAN, f64::INFINITY] {
        let err = MatchedBell::new(Q::new(1.0), Db::new(gain)).design(Hz::new(1000.0), RATE);
        assert!(
            matches!(err, Err(SosError::GainOutOfRange { .. })),
            "bell {gain}"
        );
        let err = Tilt::new(Db::new(gain)).design(Hz::new(1000.0), RATE);
        assert!(
            matches!(err, Err(SosError::GainOutOfRange { .. })),
            "tilt {gain}"
        );
    }
    let mut into = MatchedBell::new(Q::new(1.0), Db::new(6.0))
        .design(Hz::new(1000.0), RATE)
        .unwrap();
    let before = into.clone();
    assert!(
        Tilt::new(Db::new(6.0))
            .design_into(Hz::new(30000.0), RATE, &mut into)
            .is_err()
    );
    assert_eq!(into, before);
}

/// The tilt is `-gain / 2` at DC and `+gain / 2` at Nyquist (the bilinear transform's image
/// of 0 and infinity) and 1 at the center, to 1e-12.
#[test]
fn the_tilt_is_half_the_gain_down_at_dc_up_at_nyquist_and_unity_at_the_center() {
    for fc in [30.0f64, 500.0, 4000.0, 15000.0, 23000.0] {
        for db in [-24.0f64, -6.0, 6.0, 24.0] {
            let b = tilt(fc, db);
            let half = 10f64.powf(db / 40.0);
            let at = |w: f64| b.response(w).norm();
            let tag = format!("fc {fc} gain {db}");
            assert!(
                (at(0.0) * half - 1.0).abs() < 1e-12,
                "{tag} DC: {}",
                at(0.0)
            );
            assert!(
                (at(PI) / half - 1.0).abs() < 1e-12,
                "{tag} Nyquist: {}",
                at(PI)
            );
            let center = at(2.0 * PI * fc / RATE.get());
            assert!((center - 1.0).abs() < 1e-12, "{tag} center: {center}");
        }
    }
}

/// At every frequency the tilt is the analog `(A^2 x^2 + 1) / (x^2 + A^2)` at the ratio
/// `x = tan(pi f / fs) / tan(pi fc / fs)` (the bilinear image), to 1e-12 relative.
#[test]
fn the_tilt_is_the_bilinear_image_of_the_analog_shelf() {
    for fc in [100.0f64, 1000.0, 8000.0, 20000.0] {
        for db in [-18.0f64, -6.0, 6.0, 18.0] {
            let b = tilt(fc, db);
            for f in [
                20.0, 150.0, 700.0, 2000.0, 6000.0, 13000.0, 21000.0, 23900.0,
            ] {
                let want = analog_tilt(db, warped(fc, f));
                let got = magnitude(&b, f);
                assert!(
                    (got - want).abs() < 1e-12 * want,
                    "fc {fc} gain {db} at {f}: {got} vs {want}"
                );
            }
        }
    }
}

/// The tilt is one first-order section with its pole inside the circle, and a tilt of the
/// opposite gain undoes it.
#[test]
fn the_tilt_is_one_stable_first_order_section_and_its_opposite_undoes_it() {
    for fc in [30.0f64, 1000.0, 23000.0] {
        for db in [-18.0f64, 18.0] {
            let sos = Tilt::new(Db::new(db)).design(Hz::new(fc), RATE).unwrap();
            assert_eq!(sos.sections().len(), 1);
            let (b, inverse) = (sos.sections()[0], tilt(fc, -db));
            assert!(b.b2 == 0.0 && b.a2 == 0.0 && b.spectral_radius() < 1.0);
            for w in [0.0, 0.01, 0.5, 2.0, PI] {
                let product = b.response(w) * inverse.response(w);
                assert!(
                    (product - Complex64::new(1.0, 0.0)).norm() < 1e-12,
                    "fc {fc} gain {db}"
                );
            }
        }
    }
}

/// The impulse response of `b` over `len` frames, in `f64` transposed direct form II (never
/// the kernel's realization).
fn impulse_response(b: &Biquad, len: usize) -> Vec<f64> {
    run(b, len, |n| if n == 0 { 1.0 } else { 0.0 })
}

/// `b` over `len` frames of the input `x(n)`.
fn run(b: &Biquad, len: usize, x: impl Fn(usize) -> f64) -> Vec<f64> {
    let mut z = [0.0f64; 2];
    (0..len)
        .map(|n| {
            let x = x(n);
            let y = b.b0 * x + z[0];
            z[0] = b.b1 * x - b.a1 * y + z[1];
            z[1] = b.b2 * x - b.a2 * y;
            y
        })
        .collect()
}

/// The first frame from which the tail sum of `h` is at most `eps`.
fn tail_frame(h: &[f64], eps: f64) -> usize {
    let mut sum = 0.0;
    for n in (0..h.len()).rev() {
        sum += h[n].abs();
        if sum > eps {
            return n + 1;
        }
    }
    0
}

fn eps() -> f64 {
    crate::TAIL_FLOOR.to_amplitude().get()
}

fn frames(decay: DecayTime) -> usize {
    match decay {
        DecayTime::Finite(s) => (s.get() * RATE.get()).round() as usize,
        DecayTime::Unbounded => panic!("unbounded"),
    }
}

/// The reported ring-out of a bell covers the first frame from which the f64 impulse
/// response sums to at most the floor (so any stopped input in [-1, 1] is under it), over
/// centers 90 Hz to 15 kHz, Q 0.1 to 30 and gains of 6 to 24 dB either way.
#[test]
fn the_bell_ring_out_brings_the_tail_sum_under_the_floor() {
    const LEN: usize = 1 << 21;
    for fc in [90.0f64, 700.0, 4_000.0, 15_000.0] {
        for q in [0.1f64, 0.7, 4.0, 30.0] {
            for db in [-24.0f64, -6.0, 6.0, 24.0] {
                let reported = frames(
                    MatchedBell::new(Q::new(q), Db::new(db))
                        .decay_time(Hz::new(fc), RATE, crate::TAIL_FLOOR)
                        .unwrap(),
                );
                let b = bell(fc, q, db);
                let needed = tail_frame(&impulse_response(&b, LEN), eps());
                let tag = format!("fc {fc} q {q} gain {db}: reported {reported}, needed {needed}");
                assert!(reported < LEN && needed <= reported, "{tag}");
            }
        }
    }
}

/// The tilt's ring-out is exact: the first frame its tail sum is under the floor, to one frame.
#[test]
fn the_tilt_ring_out_is_the_first_frame_the_tail_sum_is_under_the_floor() {
    for fc in [30.0f64, 500.0, 4_000.0, 15_000.0, 23_000.0] {
        for db in [-24.0f64, -6.0, 6.0, 24.0] {
            let reported = frames(
                Tilt::new(Db::new(db))
                    .decay_time(Hz::new(fc), RATE, crate::TAIL_FLOOR)
                    .unwrap(),
            );
            let needed = tail_frame(&impulse_response(&tilt(fc, db), 1 << 20), eps());
            assert!(
                reported.abs_diff(needed) <= 1,
                "fc {fc} gain {db}: {reported} vs {needed}"
            );
        }
    }
}

/// `Tilt::decay_time_over` is at least the ring-out of every tilt in its ranges: centers 20 Hz
/// to 0.99 Nyquist, gains within 24 dB.
#[test]
fn the_tilt_ceiling_covers_every_tilt_in_its_ranges() {
    let top = RATE.nyquist_scaled(0.99);
    let ceiling = frames(
        Tilt::decay_time_over(Hz::new(20.0), top, Db::new(24.0), RATE, crate::TAIL_FLOOR).unwrap(),
    );
    for fc in (0..)
        .map(|k| 20.0 * 1.15f64.powi(k))
        .take_while(|f| *f < top.get())
    {
        for db in (-12..=12).map(|k| 2.0 * k as f64) {
            let own = frames(
                Tilt::new(Db::new(db))
                    .decay_time(Hz::new(fc), RATE, crate::TAIL_FLOOR)
                    .unwrap(),
            );
            assert!(own <= ceiling, "fc {fc} gain {db}: {own} > {ceiling}");
        }
    }
}

/// A sustained sine at the center of a narrow boost (Q 30, +24 dB, 20 Hz), stopped, rings
/// under the floor from the reported tail on: the output is above the floor within the first
/// period after the stop (a ring a short tail would cut) and under it for every frame from
/// `reported`.
#[test]
fn a_stopped_sustained_sine_at_a_narrow_boost_rings_under_the_floor_within_the_tail() {
    let (fc, q, db) = (20.0f64, 30.0f64, 24.0f64);
    let b = bell(fc, q, db);
    let reported = frames(
        MatchedBell::new(Q::new(q), Db::new(db))
            .decay_time(Hz::new(fc), RATE, crate::TAIL_FLOOR)
            .unwrap(),
    );
    let on = 600_000;
    let w = 2.0 * PI * fc / RATE.get();
    let y = run(&b, on + 4 * reported, |n| {
        if n < on { (w * n as f64).sin() } else { 0.0 }
    });
    let after = &y[on..];
    let first = after[..2_400].iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(
        after[reported..].iter().all(|v| v.abs() <= eps()),
        "rings past the tail"
    );
    assert!(
        first > 100.0 * eps(),
        "the ring is not above the floor before the tail"
    );
}

/// A floor that is not a number or is infinite reads as an unbounded ring-out, for the bell and
/// the tilt, and `decay_time_over` takes the ends of the centers in either order.
#[test]
fn a_floor_that_is_not_a_number_is_unbounded_and_the_ends_of_the_centers_may_swap() {
    for floor in [
        Db::new(f64::NAN),
        Db::new(f64::INFINITY),
        Db::new(f64::NEG_INFINITY),
    ] {
        let bell =
            MatchedBell::new(Q::new(1.0), Db::new(6.0)).decay_time(Hz::new(1000.0), RATE, floor);
        let tilt = Tilt::new(Db::new(6.0)).decay_time(Hz::new(1000.0), RATE, floor);
        assert!(
            matches!(
                (bell, tilt),
                (Ok(DecayTime::Unbounded), Ok(DecayTime::Unbounded))
            ),
            "{floor:?}"
        );
    }
    let over = |lo, hi| {
        Tilt::decay_time_over(
            Hz::new(lo),
            Hz::new(hi),
            Db::new(24.0),
            RATE,
            crate::TAIL_FLOOR,
        )
    };
    assert_eq!(over(20.0, 20_000.0), over(20_000.0, 20.0));
}
