use super::*;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Hz, Samples};

const EPS: f64 = f64::EPSILON;

fn rate() -> SampleRate {
    SampleRate::new(192_000.0).expect("a rate")
}

/// A mono stage at saturation `c`, coercivity position `w` and `drive` decibels.
fn stage(c: f64, w: f64, drive: f64) -> Hysteresis<f64> {
    let mut s = Hysteresis::new(
        ChannelLayout::MONO,
        rate(),
        Normalized::of(c),
        Normalized::of(w),
    );
    s.retarget(&HysteresisControls {
        drive: HysteresisDrive::of(Db::new(drive)),
    });
    s.jump_to_target();
    s
}

/// The default material: `c` 0.2 and `w` 0.5 (`k` 400 A/m).
fn default_stage(drive: f64) -> Hysteresis<f64> {
    stage(0.2, 0.5, drive)
}

fn run(s: &mut Hysteresis<f64>, x: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0; x.len()];
    let input = [x];
    let mut out = [&mut y[..]];
    let mut block = InputOutput::new(
        Samples::new(x.len()),
        Planes::new(&input).unwrap(),
        PlanesMut::new(&mut out).unwrap(),
    )
    .unwrap();
    s.process(&mut block).unwrap();
    y
}

/// `periods` periods of a unit sine of `per` samples each.
fn sine(per: usize, periods: usize) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        Hz::new(1.0),
        tutti_units::Amplitude::new(1.0),
        SampleRate::of(per as f64),
        Samples::new(per * periods),
    )
}

fn amplitude(drive: f64) -> f64 {
    Db::new(drive).to_amplitude().get()
}

// The oracle: the model's equation as written in the module's documentation, with the
// Langevin function from its own series (`2^(2n) B_(2n) / (2n)!` as decimals) and from `tanh`.

const SERIES: [f64; 9] = [
    0.3333333333333333,
    -0.022222222222222223,
    0.0021164021164021165,
    -0.00021164021164021165,
    2.1377799155576935e-05,
    -2.1644042808063972e-06,
    2.1925947851873778e-07,
    -2.2214608789979678e-08,
    2.2507846516808994e-09,
];

/// `L` and `L'` of the oracle: the series below 0.3 (truncation under `1e-18`), `tanh` above.
fn lang(x: f64) -> (f64, f64) {
    if x.abs() < 0.3 {
        let t = x * x;
        let l: f64 = SERIES
            .iter()
            .enumerate()
            .map(|(j, c)| c * x.powi(2 * j as i32 + 1))
            .sum();
        let l1: f64 = SERIES
            .iter()
            .enumerate()
            .map(|(j, c)| c * (2 * j + 1) as f64 * t.powi(j as i32))
            .sum();
        (l, l1)
    } else {
        let th = x.tanh();
        (1.0 / th - 1.0 / x, 1.0 / (x * x) - 1.0 / x.sinh().powi(2))
    }
}

/// The Jiles-Atherton parameters of the oracle in the dimensionless form.
#[derive(Clone, Copy)]
struct Ja {
    alpha: f64,
    kappa: f64,
    c: f64,
}

impl Ja {
    /// The set of the module: `alpha'` from the physical constants, `kappa` at position `w`.
    fn at(c: f64, w: f64) -> Self {
        Self {
            alpha: 1.6e-3 * 1.6e6 / 1_100.0,
            kappa: 400.0 / 1_100.0 * 16f64.powf(2.0 * w - 1.0),
            c,
        }
    }

    /// `chi = dm/dh_e` at `(h, m)` going `delta`.
    fn chi(&self, h: f64, m: f64, delta: f64) -> f64 {
        let (l, l1) = lang(h + self.alpha * m);
        (1.0 - self.c) * (delta * (l - m)).max(0.0) / self.kappa + self.c * l1
    }

    /// `dm/dh`.
    fn slope(&self, h: f64, m: f64, delta: f64) -> f64 {
        let chi = self.chi(h, m, delta);
        chi / (1.0 - self.alpha * chi)
    }

    /// `d(dm/dh)/dm` by a central difference (its error `1e-12` of the slope's second derivative
    /// scale is far under what the callers resolve).
    fn slope_m(&self, h: f64, m: f64, delta: f64) -> f64 {
        let e = 1e-6;
        (self.slope(h, m + e, delta) - self.slope(h, m - e, delta)) / (2.0 * e)
    }

    /// `m` after `h0 -> h1` by Radau IIA (three stages, order 5) in `substeps` equal steps,
    /// the stage equations solved by Newton to `1e-14`. Its error is the kink's: the pull
    /// `max(0, ..)` has a corner, a local error of about `hs^2 / 2` times the corner's jump in
    /// slope, under `1e-9` at the steps used.
    fn radau(&self, h0: f64, m0: f64, h1: f64, substeps: usize) -> f64 {
        let delta = (h1 - h0).signum();
        let hs = (h1 - h0) / substeps as f64;
        let r6 = 6.0f64.sqrt();
        let cc = [(4.0 - r6) / 10.0, (4.0 + r6) / 10.0, 1.0];
        let a = [
            [
                (88.0 - 7.0 * r6) / 360.0,
                (296.0 - 169.0 * r6) / 1800.0,
                (-2.0 + 3.0 * r6) / 225.0,
            ],
            [
                (296.0 + 169.0 * r6) / 1800.0,
                (88.0 + 7.0 * r6) / 360.0,
                (-2.0 - 3.0 * r6) / 225.0,
            ],
            [(16.0 - r6) / 36.0, (16.0 + r6) / 36.0, 1.0 / 9.0],
        ];
        let (mut h, mut m) = (h0, m0);
        for _ in 0..substeps {
            let mut k = [self.slope(h, m, delta); 3];
            for _ in 0..60 {
                let stage: [f64; 3] =
                    core::array::from_fn(|i| m + hs * (0..3).map(|j| a[i][j] * k[j]).sum::<f64>());
                let g: [f64; 3] =
                    core::array::from_fn(|i| self.slope(h + cc[i] * hs, stage[i], delta) - k[i]);
                let fm: [f64; 3] =
                    core::array::from_fn(|i| self.slope_m(h + cc[i] * hs, stage[i], delta));
                let mut sys = [[0.0; 4]; 3];
                for i in 0..3 {
                    for j in 0..3 {
                        sys[i][j] = f64::from(u8::from(i == j)) - hs * a[i][j] * fm[j];
                    }
                    sys[i][3] = g[i];
                }
                for p in 0..3 {
                    for i in p + 1..3 {
                        let r = sys[i][p] / sys[p][p];
                        for j in p..4 {
                            sys[i][j] -= r * sys[p][j];
                        }
                    }
                }
                let mut dk = [0.0; 3];
                for i in (0..3).rev() {
                    dk[i] = (sys[i][3] - (i + 1..3).map(|j| sys[i][j] * dk[j]).sum::<f64>())
                        / sys[i][i];
                }
                for i in 0..3 {
                    k[i] += dk[i];
                }
                if dk.iter().all(|d| d.abs() <= 1e-14) {
                    break;
                }
            }
            m += hs * (0..3).map(|j| a[2][j] * k[j]).sum::<f64>();
            h += hs;
        }
        m
    }
}

/// The unit of the module's `h` per unit of signal at `drive` decibels.
fn field(x: &[f64], drive: f64) -> Vec<f64> {
    x.iter().map(|v| v * amplitude(drive)).collect()
}

/// The reference path of a field `h` from rest, one Radau solve over 8 substeps per sample.
fn reference(ja: &Ja, h: &[f64]) -> Vec<f64> {
    let (mut hp, mut m) = (0.0, 0.0);
    h.iter()
        .map(|&h| {
            m = ja.radau(hp, m, h, 8);
            hp = h;
            m
        })
        .collect()
}

/// `d^2(dm/dh)/dm^2` by a second difference (its error `1e-6` relative is far under the
/// second-order term it scales).
fn slope_mm(ja: &Ja, h: f64, m: f64, delta: f64) -> f64 {
    let e = 1e-3;
    (ja.slope(h, m + e, delta) - 2.0 * ja.slope(h, m, delta) + ja.slope(h, m - e, delta)) / (e * e)
}

/// The envelope of the digital path's error against the reference at every sample. With
/// `tau_n` the defect of the reference in the trapezoid's equation,
/// `m_n - m_(n-1) - (dh / 2) (S_(n-1) + S_n)`, the digital root and the reference differ by
/// `e_n (1 - a_n) = e_(n-1) (1 + p_n) + tau_n + r_n`, with `a_n = (dh / 2) S_m(h_n)` and
/// `p_n = (dh / 2) S_m(h_(n-1))` read along the reference and `r_n` the second-order remainder
/// of the slope's expansion, `(|dh| / 4) (|S_mm(n-1)| e_(n-1)^2 + |S_mm(n)| e_n^2)`. So
/// `E_n = (|1 + p_n| E_(n-1) + |tau_n| + r_n) / (1 - a_n)` bounds `|e_n|` while `1 - a_n > 0`;
/// the `e_n` in `r_n` is the envelope of a first pass without it, grown by 1%.
fn envelope(ja: &Ja, h: &[f64], m: &[f64]) -> Vec<f64> {
    let pass = |first: Option<&[f64]>| {
        let mut e = 0.0;
        let mut out = Vec::with_capacity(h.len());
        for n in 0..h.len() {
            let (hp, mp) = if n == 0 {
                (0.0, 0.0)
            } else {
                (h[n - 1], m[n - 1])
            };
            let dh = h[n] - hp;
            let delta = dh.signum();
            let tau =
                m[n] - mp - dh / 2.0 * (ja.slope(hp, mp, delta) + ja.slope(h[n], m[n], delta));
            let a = dh / 2.0 * ja.slope_m(h[n], m[n], delta);
            let p = dh / 2.0 * ja.slope_m(hp, mp, delta);
            assert!(1.0 - a > 0.0, "the envelope's premise at {n}");
            let remainder = first.map_or(0.0, |first| {
                dh.abs() / 4.0
                    * (slope_mm(ja, hp, mp, delta).abs() * e * e
                        + slope_mm(ja, h[n], m[n], delta).abs() * (1.01 * first[n]).powi(2))
            });
            e = ((1.0 + p).abs() * e + tau.abs() + remainder) / (1.0 - a);
            out.push(e);
        }
        out
    };
    let first = pass(None);
    pass(Some(&first))
}

/// Where a sampled branch crosses `level`, by linear interpolation of the two samples that
/// straddle it: `h` where `m` is `level` (`of_m`), else `m` where `h` is `level`.
fn crossing(h: &[f64], m: &[f64], level: f64, of_m: bool) -> f64 {
    let (x, y) = if of_m { (h, m) } else { (m, h) };
    for n in 1..x.len() {
        let (a, b) = (y[n - 1] - level, y[n] - level);
        if a * b <= 0.0 && a != b {
            return x[n - 1] + (x[n] - x[n - 1]) * a / (a - b);
        }
    }
    panic!("no crossing of {level}");
}

/// The Langevin function near 0 is its series, to the first term the oracle's partial sum
/// drops (the series alternates, each term smaller than the last by `(x / pi)^2` at most) and
/// the rounding of the sum. Cancellation in the closed forms would show here: `coth x - 1 / x`
/// at `x = 1e-3` loses `eps / x` absolutely, `7e-10` of `L`.
#[test]
fn the_langevin_function_near_zero_is_its_series_to_the_first_dropped_term() {
    for x in [1e-3, 1e-2, 0.1, 0.2, -1e-3, -0.2] {
        let (l, l1, l2) = langevin::<f64>(x);
        let t = x * x;
        let part_l: f64 = (0..4).map(|j| SERIES[j] * x.powi(2 * j as i32 + 1)).sum();
        let part_l1: f64 = (0..4)
            .map(|j| SERIES[j] * (2 * j + 1) as f64 * t.powi(j as i32))
            .sum();
        let part_l2: f64 = (1..4)
            .map(|j| SERIES[j] * ((2 * j + 1) * 2 * j) as f64 * x.powi(2 * j as i32 - 1))
            .sum();
        let next = SERIES[4].abs();
        let ax = x.abs();
        let bound_l = 1.1 * next * ax.powi(9) + 4.0 * EPS * ax;
        let bound_l1 = 1.1 * 9.0 * next * ax.powi(8) + 4.0 * EPS;
        let bound_l2 = 1.1 * 72.0 * next * ax.powi(7) + 4.0 * EPS * ax;
        assert!((l - part_l).abs() <= bound_l, "L({x}): {l} vs {part_l}");
        assert!(
            (l1 - part_l1).abs() <= bound_l1,
            "L'({x}): {l1} vs {part_l1}"
        );
        assert!(
            (l2 - part_l2).abs() <= bound_l2,
            "L''({x}): {l2} vs {part_l2}"
        );
    }
}

/// At the threshold the series and the closed forms are the same function: they differ by the
/// closed forms' cancellation, `4 eps / x`, `4 eps / x^2` and `8 eps / x^3` of the terms
/// `coth x`, `1 / x^2` and `1 / x^3` they subtract.
#[test]
fn the_series_and_the_closed_forms_agree_at_the_threshold() {
    let x: f64 = SERIES_BELOW;
    let (a, a1, a2) = langevin_series(x);
    let (b, b1, b2) = langevin_closed(x);
    assert!((a - b).abs() <= 4.0 * EPS / x, "L {a} {b}");
    assert!((a1 - b1).abs() <= 4.0 * EPS / (x * x), "L' {a1} {b1}");
    assert!((a2 - b2).abs() <= 8.0 * EPS / (x * x * x), "L'' {a2} {b2}");
    // coth 1 - 1 = 0.3130352854993313 (coth 1 = 1.3130352854993313).
    assert!((langevin::<f64>(1.0).0 - 0.313_035_285_499_331_3).abs() <= 4.0 * EPS);
}

/// For large `x`, `e^(-2 x)` is gone and `L = 1 - 1 / x`, `L' = 1 / x^2`, `L'' = -2 / x^3`;
/// at an `x` where `e^x` overflows the forms are still finite.
#[test]
fn the_langevin_function_saturates_without_overflow() {
    let x = 40.0;
    let (l, l1, l2) = langevin::<f64>(x);
    assert!((l - (1.0 - 1.0 / x)).abs() <= EPS);
    assert!((l1 - 1.0 / (x * x)).abs() <= EPS / (x * x));
    assert!((l2 + 2.0 / (x * x * x)).abs() <= 4.0 * EPS / (x * x * x));
    let (l, l1, l2) = langevin::<f64>(1.0e4);
    assert!(l.is_finite() && l1.is_finite() && l2.is_finite());
    assert!((l - (1.0 - 1.0e-4)).abs() <= EPS);
}

/// `L` and `L''` are odd and `L'` is even, to the bit, on both sides of the threshold.
#[test]
fn the_langevin_function_is_odd_to_the_bit() {
    for x in [1e-4, 0.1, 0.2499, 0.25, 0.7, 3.0, 50.0] {
        let (l, l1, l2) = langevin::<f64>(x);
        let (nl, nl1, nl2) = langevin::<f64>(-x);
        assert_eq!((nl, nl1, nl2), (-l, l1, -l2), "{x}");
    }
}

/// The default material (`c` 0.2, `k` 400 A/m) at +12 dB, 2048 samples a period for three
/// periods, against the model's ODE integrated by Radau at 8 substeps a sample (the field
/// moves 0.012 a sample, 0.0015 a substep): every sample is within the envelope of the
/// trapezoid's error, and the major loop's remanence (`m` where `h = 0` on the descending
/// branch) and coercivity (`h` where `m = 0`), read from the digital samples and from the
/// reference by the same interpolation, differ by at most the envelope there (the
/// coercivity by the envelope over the slope at the crossing). The slope's denominator
/// `1 - alpha' chi` stays above 0.2 along the path (it is 0.224 where `x_e = 0`).
#[test]
fn the_major_loops_remanence_and_coercivity_match_the_ode_within_the_trapezoids_envelope() {
    let (per, periods, drive) = (2_048, 3, 12.0);
    let ja = Ja::at(0.2, 0.5);
    let x = sine(per, periods);
    let h = field(&x, drive);
    let truth = reference(&ja, &h);
    let band = envelope(&ja, &h, &truth);
    let y = run(&mut default_stage(drive), &x);
    let mut worst = 0.0f64;
    let mut worst_bound = 0.0f64;
    let mut pole = 0.0f64;
    for n in 0..y.len() {
        let err = (y[n] - truth[n]).abs();
        assert!(err <= band[n] + 8.0 * EPS, "{n}: {err:e} > {:e}", band[n]);
        worst = worst.max(err);
        worst_bound = worst_bound.max(band[n]);
        let (hp, delta) = if n == 0 {
            (0.0, 1.0)
        } else {
            (h[n - 1], (h[n] - h[n - 1]).signum())
        };
        let _ = hp;
        pole = pole.max(ja.alpha * ja.chi(h[n], truth[n], delta));
    }
    assert!(pole < 0.8, "the slope's denominator: {pole}");
    assert!(
        worst > worst_bound / 100.0,
        "the envelope is tight to two decades: {worst:e} vs {worst_bound:e}"
    );
    // The descending branch of the last period: from its peak (a quarter in) to its trough.
    let last = per * (periods - 1);
    let branch = last + per / 4..last + 3 * per / 4 + 1;
    let (hb, mb, tb) = (
        &h[branch.clone()],
        &y[branch.clone()],
        &truth[branch.clone()],
    );
    let remanence_digital = crossing(hb, mb, 0.0, false);
    let remanence_truth = crossing(hb, tb, 0.0, false);
    let coercivity_digital = crossing(hb, mb, 0.0, true);
    let coercivity_truth = crossing(hb, tb, 0.0, true);
    let bound = band[branch].iter().fold(0.0f64, |m, v| m.max(*v));
    assert!(
        remanence_truth > 0.3 && coercivity_truth < -0.3,
        "a loop: {remanence_truth}, {coercivity_truth}"
    );
    assert!(
        (remanence_digital - remanence_truth).abs() <= bound + 8.0 * EPS,
        "remanence {remanence_digital} vs {remanence_truth}, bound {bound:e}"
    );
    let slope = ja.slope(coercivity_truth, 0.0, -1.0);
    assert!(
        (coercivity_digital - coercivity_truth).abs() <= bound / slope + 8.0 * EPS,
        "coercivity {coercivity_digital} vs {coercivity_truth}, bound {:e}",
        bound / slope
    );
}

/// With the loss at its lowest (`k = 400 / 16` A/m) the path follows the anhysteretic curve
/// `m = L(h + alpha' m)` (its root by bisection: `L(h + alpha' m) - m` falls from positive to
/// negative on `-1..1` since `alpha' / 3 < 1`) with a lag. Tracking the curve is `dm/dh = m_an'`,
/// where `chi = (1 - c) q / kappa + c L'(x_e) = L'(x_an)` for the pull `q`, so
/// `q = kappa L'(x_an)` to the order `kappa` (`L'(x_e)` differs from `L'(x_an)` by `alpha' L''`
/// times the lag), and `m - m_an = -q / (1 - alpha' L~')`, `L~'` the mean of `L'` between the
/// two. The lag is largest at `x_an = 0`, `L' = 1/3`: `kappa / 3 / (1 - alpha' / 3)`, and the
/// corrections are of relative order `kappa`, bounded here by `1.5 kappa`.
#[test]
fn with_the_loss_at_its_lowest_the_path_follows_the_anhysteretic_curve_by_the_pinning_lag() {
    let (per, drive) = (2_048, 12.0);
    let ja = Ja::at(0.2, 0.0);
    let x = sine(per, 2);
    let h = field(&x, drive);
    let y = run(&mut stage(0.2, 0.0, drive), &x);
    let mut worst = 0.0f64;
    for n in 0..y.len() {
        let (mut lo, mut hi) = (-1.0, 1.0);
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if lang(h[n] + ja.alpha * mid).0 - mid > 0.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        worst = worst.max((y[n] - 0.5 * (lo + hi)).abs());
    }
    let lag = ja.kappa / 3.0 / (1.0 - ja.alpha / 3.0);
    assert!(
        (worst - lag).abs() <= 1.5 * ja.kappa * lag,
        "{worst:e} vs {lag:e}"
    );
}

/// The map from the state at the start of a period to its end contracts along the loop (the
/// pull's slope `-(1 - c)(1 - alpha' L') / (kappa D^2)` is at most `-0.49` a unit of `h` over
/// the 7 units of a half period, and `1 / D^2` is above 1), so the first period, which starts
/// on the virgin curve, ends far from where it started and every later period ends at the
/// loop's own start to the rounding: `16 eps`.
#[test]
fn after_the_first_period_the_loop_is_closed_to_the_rounding() {
    let per = 2_048;
    for (c, w) in [(0.2, 0.5), (0.0, 0.0), (0.6, 0.25)] {
        let mut x = sine(per, 4);
        x.push(0.0);
        let y = run(&mut stage(c, w, 12.0), &x);
        assert!(
            (y[per] - y[0]).abs() > 0.01,
            "the first period is not closed: {c} {w}"
        );
        for k in 2..4 {
            assert!(
                (y[k * per] - y[(k - 1) * per]).abs() <= 16.0 * EPS,
                "{c} {w} period {k}: {:e}",
                y[k * per] - y[(k - 1) * per]
            );
        }
    }
}

/// The set is odd in the field (`L` is odd, `L'` even, and the pull `delta (L - m)` is even
/// under `(h, m, delta) -> -(h, m, delta)`), and every step of the solve respects it, so a
/// negated input gives exactly the negated output, bit for bit, over two tones that reverse
/// the field at 40 different amplitudes.
#[test]
fn a_negated_input_gives_the_negated_output_bit_for_bit() {
    let x: Vec<f64> = sine(500, 20)
        .iter()
        .zip(sine(137, 73))
        .map(|(a, b)| 0.8 * a + 0.5 * b)
        .collect();
    let negated: Vec<f64> = x.iter().map(|v| -v).collect();
    for (c, w) in [(0.2, 0.5), (0.0, 0.0), (1.0, 1.0)] {
        let y = run(&mut stage(c, w, 18.0), &x);
        let z = run(&mut stage(c, w, 18.0), &negated);
        assert!(y.iter().zip(&z).all(|(a, b)| *a == -*b), "{c} {w}");
        assert!(y.iter().any(|v| v.abs() > 0.5), "the signal is there");
    }
}

/// Over the whole range of `(c, w)` and of steps up to `STEP` (a random walk of full and
/// partial steps that reverses every 3000 steps and stays within `|h| <= 8`), the residual of
/// the trapezoid's equation after `HYSTERESIS_CAP` steps, read with the oracle's slope, is under
/// `RESIDUAL`. The seed is the Euler step, within `0.09` of the root at `STEP` (the largest
/// measured `e_0`, at the default material), and each step takes the error to
/// `(K / 2) e^2` with `K = sup|f''| / inf|f'|` (`f' >= 1` where the pull is active); four steps
/// leave `4e-12` at the worst corner, measured over `2e5` steps, `RESIDUAL` is 25 times that.
/// With one step fewer the same walk leaves `1.8e-6`, over the bound.
#[test]
fn the_residual_of_the_trapezoid_equation_after_the_cap_is_under_the_stated_bound() {
    use tutti_rt::SplitMix64;
    const RESIDUAL: f64 = 1e-10;
    let mut fewer = 0.0f64;
    for (c, w) in [
        (0.0, 0.0),
        (0.2, 0.5),
        (1.0, 0.5),
        (0.2, 1.0),
        (0.6, 0.0),
        (0.0, 1.0),
    ] {
        let ja = Ja::at(c, w);
        let model = Model::<f64>::new(Normalized::of(c), Normalized::of(w));
        let mut rng = SplitMix64::new(7);
        let (mut hp, mut mp, mut dir) = (0.0f64, 0.0f64, 1.0);
        let mut worst = 0.0f64;
        for n in 0..30_000 {
            if n % 3_000 == 0 {
                dir = -dir;
            }
            let part = if n % 5 == 0 {
                f64::from_fraction_bits(rng.next_u64())
            } else {
                1.0
            };
            let mut dh = dir * STEP * part;
            if (hp + dh).abs() > 8.0 {
                dir = -dir;
                dh = -dh;
            }
            let h = hp + dh;
            let delta = dh.signum();
            let residual = |m: f64| {
                (m - mp - dh / 2.0 * (ja.slope(hp, mp, delta) + ja.slope(h, m, delta))).abs()
            };
            let m = model.step(hp, mp, h);
            worst = worst.max(residual(m));
            if (c, w) == (0.2, 0.5) {
                fewer = fewer.max(residual(
                    model.step_capped::<{ HYSTERESIS_CAP - 1 }>(hp, mp, h),
                ));
            }
            hp = h;
            mp = m;
        }
        assert!(worst <= RESIDUAL, "c {c} w {w}: {worst:e}");
    }
    assert!(
        fewer > RESIDUAL,
        "one step fewer is over the bound: {fewer:e}"
    );
}

/// A move larger than `STEP` is split, so the solve starts from a seed within reach: a step
/// of the whole field from `-8` to `+8` in one sample leaves the trapezoid's equation, solved
/// in `n` substeps, within `1e-10` at its last substep, and the output is the model's
/// saturated value (`m` between 0.85 and 1).
#[test]
fn a_full_scale_jump_is_solved_in_substeps_to_the_saturated_magnetization() {
    let mut s = default_stage(18.0);
    let hi = 8.0 / amplitude(18.0);
    let y = run(&mut s, &[-hi, hi, hi, -hi]);
    assert!(y[0] < -0.85 && y[0] > -1.0, "{:?}", y);
    assert!(y[1] > 0.85 && y[1] < 1.0, "{:?}", y);
    assert_eq!(
        y[1], y[2],
        "a field that does not move leaves m where it is"
    );
    assert!(y[3] < -0.85 && y[3] > -1.0, "{:?}", y);
}

/// The material keeps its remanence: after a loop ends where `h = 0` and the signal stops, the
/// output holds the magnetization it was left at, to the bit, and it is not 0.
#[test]
fn the_output_holds_the_remanence_after_the_input_stops() {
    let mut x = sine(2_048, 2);
    // The last sample of the second period is 0 on its way up from the trough.
    x.extend(core::iter::repeat_n(0.0, 3_000));
    let y = run(&mut default_stage(12.0), &x);
    let held = y[y.len() - 1];
    assert!(held.abs() > 0.3, "the remanence: {held}");
    assert!(y[2 * 2_048..].iter().all(|v| *v == held));
}

/// `reset` demagnetizes: a loop after it is the loop of a fresh stage, to the bit.
#[test]
fn a_reset_stage_renders_as_a_fresh_one() {
    let x = sine(256, 6);
    let mut s = default_stage(12.0);
    let _ = run(&mut s, &sine(100, 3));
    s.reset();
    let y = run(&mut s, &x);
    assert_eq!(y, run(&mut default_stage(12.0), &x));
}

/// **The drive runs from -24 dB to +18 dB**, the range the solver's residual is stated over:
/// both ends are valid, and a value past either, or not a number, is refused.
#[test]
fn a_drive_takes_exactly_the_documented_range() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: HysteresisDrive::new,
        get: HysteresisDrive::get,
        of: Some(HysteresisDrive::of),
        valid: &[Db::new(-24.0), Db::new(12.0), Db::new(18.0)],
        invalid: &[
            Db::new(-24.001),
            Db::new(18.001),
            Db::new(f64::NAN),
            Db::new(f64::INFINITY),
            Db::new(f64::NEG_INFINITY),
        ],
    });
}

/// A live drive value is held in the range: past an end it reads that end, a NaN reads the
/// default, and a value in range reads as itself.
#[test]
fn a_live_drive_is_held_in_the_range() {
    let held = |db: f64| HysteresisDrive::held(Db::new(db)).get().get();
    assert_eq!(held(1.0e9), 18.0);
    assert_eq!(held(f64::INFINITY), 18.0);
    assert_eq!(held(-1.0e9), -24.0);
    assert_eq!(held(f64::NEG_INFINITY), -24.0);
    assert_eq!(held(f64::NAN), 12.0);
    assert_eq!(held(-17.5), -17.5);
}

#[test]
fn a_block_of_another_width_is_refused_before_a_sample_is_touched() {
    let mut s = Hysteresis::<f64>::new(
        ChannelLayout::STEREO,
        rate(),
        Normalized::of(0.2),
        Normalized::of(0.5),
    );
    let x = [0.5; 4];
    let input = [&x[..]];
    let mut y = [7.0; 4];
    let mut out = [&mut y[..]];
    let mut block = InputOutput::new(
        Samples::new(4),
        Planes::new(&input).unwrap(),
        PlanesMut::new(&mut out).unwrap(),
    )
    .unwrap();
    assert!(s.process(&mut block).is_err());
    assert_eq!(y, [7.0; 4]);
}
