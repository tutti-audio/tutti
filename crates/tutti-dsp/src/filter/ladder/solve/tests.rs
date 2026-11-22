use core::f64::consts::TAU;

use super::*;
use crate::LadderDesign;
use crate::newton::NEWTON_CAP;
use crate::testing::{radau5_step, uniform};
use tutti_rt::SplitMix64;
use tutti_units::{Hz, Resonance, SampleRate};

const RATE: SampleRate = SampleRate::SR_48K;

fn coefficients(fc: f64, res: f64) -> LadderCoefficients<f64> {
    LadderCoefficients::from(&LadderDesign::new(Hz::new(fc), Resonance::new(res), RATE))
}

/// `n` seeded uniform values in `-1..1`.
fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..n).map(|_| uniform(&mut rng, -1.0, 1.0)).collect()
}

/// The bound on `|F''|` of [`loop_residual`]: each factor `1 - tanh^2 y_i` of `F'` has
/// derivative at most `4 / (3 sqrt 3)` in `y_i`, and `dy_i / du <= G^i`.
fn second_derivative_bound(c: &LadderCoefficients<f64>) -> f64 {
    let g = c.g1;
    c.k * g.powi(4) * (4.0 / (3.0 * 3.0f64.sqrt())) * (1.0 + g + g * g + g.powi(3))
}

/// The residual of `F` after [`NEWTON_CAP`] steps from a start with `|F| = start`:
/// `(1 + k G^4) (M / 2)^3 start^4` for the cap of two (the module's derivation).
fn residual_bound(c: &LadderCoefficients<f64>, start: f64) -> f64 {
    assert_eq!(NEWTON_CAP, 2, "the bound is derived for two steps");
    (1.0 + c.k * c.g1.powi(4)) * (second_derivative_bound(c) / 2.0).powi(3) * start.powi(4)
}

/// After the cap the loop equation's residual is the module's derived bound, `(1 + k G^4)
/// (M / 2)^3 F(start)^4`, plus the rounding of `F`'s terms (`8 eps` of their magnitudes), at
/// every sample of a loud noise through every corner of the controls: the drive 0.1 to 8
/// (`x` up to 8), the resonance 0 to 1 (`k` 0 to 4), the cutoff from 100 Hz to a quarter of
/// the rate (`G` up to 1/2, the range `M` is derived for), and the start the better of the
/// seed and the warm start. One Newton step leaves `(1 + k G^4) (M / 2) F(start)^2`, which
/// exceeds the bound where `F(start)` is not small: at drive 8, `k = 4` and 12 kHz the start
/// has `F` near 1 and a single step is over the bound by a factor of the order of `1/F^2`;
/// the test counts the samples where the bound is under the one-step residual's bound's half
/// (so a cap of one fails it) and requires them to be common.
#[test]
fn the_loop_residual_after_the_cap_is_the_derived_bound() {
    let eps = f64::EPSILON;
    let mut informative = 0usize;
    let mut total = 0usize;
    for fc in [100.0, 1_000.0, 6_000.0, 12_000.0] {
        for res in [0.0, 0.5, 0.9, 1.0] {
            for drive in [0.1, 1.0, 8.0] {
                let c = coefficients(fc, res);
                let mut state = LadderState::<f64>::silent();
                for (i, v) in noise(4_000, 1 + i64::from(drive as i32) as u64)
                    .iter()
                    .enumerate()
                {
                    let x = v * drive;
                    let seed = linear_seed(&c, &state.s, x);
                    let at_seed = loop_residual(&c, &state.s, x, seed).0.abs();
                    let at_warm = loop_residual(&c, &state.s, x, state.u).0.abs();
                    let start = at_seed.min(at_warm);
                    let u = solve_loop(&c, &state.s, x, state.u);
                    let (residual, _) = loop_residual(&c, &state.s, x, u);
                    let chain = Chain::at(c.g1, &state.s, u);
                    let size = u.abs()
                        + x.abs()
                        + c.k
                            * (0..4)
                                .map(|n| chain.y[n].abs() + chain.v[n].abs() + state.s[n].abs())
                                .sum::<f64>();
                    let bound = residual_bound(&c, start) + 8.0 * eps * size;
                    assert!(
                        residual.abs() <= bound,
                        "fc {fc} res {res} drive {drive} sample {i}: {residual:e} over {bound:e}"
                    );
                    let one_step = (1.0 + c.k * c.g1.powi(4)) * second_derivative_bound(&c) / 2.0
                        * start
                        * start;
                    total += 1;
                    if one_step > 4.0 * bound {
                        informative += 1;
                    }
                    nonlinear_ladder_step(&c, LadderType::LP24, &mut state, x);
                }
            }
        }
    }
    assert!(
        informative * 20 > total,
        "the bound is binding where a cap of one would miss it: {informative} of {total}"
    );
}

/// The saturators bound the taps. A trapezoidal stage `G (1 + z^-1) / (1 - p z^-1)`,
/// `p = 1 - 2 G`, has an impulse response of l1 norm 1 for `G <= 1/2` (`g <= 1`, a cutoff up
/// to a quarter of the rate), so a stage output is at most the peak of its input: the loop
/// input `tanh u` is under 1, so `|y2| <= tanh 1` (the 12 dB tap, after the second stage's
/// saturator) and `|y4| <= tanh tanh tanh 1 = 0.5655` (the 24 dB tap, behind three). At
/// drive 1000 and `k = 4` a held input gives the loop input `tanh u = 1` and each tap its
/// bound at rest (to 1%), and a loud noise after it stays under them (a stage without its
/// `tanh` would pass the 24 dB bound: 1 against 0.5655).
#[test]
fn the_taps_are_bounded_by_the_saturators_before_them() {
    let t1 = 1.0f64.tanh();
    let t3 = t1.tanh().tanh();
    assert!((t3 - 0.5655).abs() < 1e-3, "{t3}");
    for (ty, bound) in [(LadderType::LP12, t1), (LadderType::LP24, t3)] {
        for fc in [50.0, 1_000.0, 12_000.0] {
            let c = coefficients(fc, 1.0);
            let mut state = LadderState::<f64>::silent();
            let mut rest = 0.0;
            for _ in 0..40_000 {
                rest = nonlinear_ladder_step(&c, ty, &mut state, 1_000.0);
            }
            assert!(
                (rest - bound).abs() < 0.01 * bound,
                "{ty:?} {fc} Hz: {rest} at rest against {bound}"
            );
            let peak = noise(20_000, 5).iter().fold(0.0f64, |p, v| {
                p.max(nonlinear_ladder_step(&c, ty, &mut state, 1_000.0 * v).abs())
            });
            assert!(peak <= bound, "{ty:?} {fc} Hz: {peak} over {bound}");
        }
    }
}

/// The derivatives `y_n'` and `y_n'''` of the continuous ladder's state at `t`, from the
/// jets of each signal in time (its value and first three derivatives), the stage equations
/// `y_n^(m+1) = w (a_(n-1)^(m) - y_n^(m))` taken order by order, with `a = tanh v`,
/// `s = 1 - a^2`, `a' = s v'`, `a'' = s v'' - 2 a s v'^2` and
/// `a''' = s v''' - 6 a s v' v'' + (4 a^2 - 2 s) s v'^3`.
fn state_jets(ode: &Ode, t: f64, y: &[f64; 4]) -> [[f64; 4]; 4] {
    let omega = TAU * ode.f;
    let a = ode.amplitude;
    let x = [
        a * (omega * t).sin(),
        a * omega * (omega * t).cos(),
        -a * omega * omega * (omega * t).sin(),
        -a * omega.powi(3) * (omega * t).cos(),
    ];
    let mut yj = [[0.0f64; 4]; 4];
    for n in 0..4 {
        yj[n][0] = y[n];
    }
    for m in 0..3 {
        for n in 0..4 {
            let v: [f64; 4] = if n == 0 {
                core::array::from_fn(|q| x[q] - ode.k * yj[3][q])
            } else {
                yj[n - 1]
            };
            let th = v[0].tanh();
            let s = 1.0 - th * th;
            let aj = [
                th,
                s * v[1],
                s * v[2] - 2.0 * th * s * v[1] * v[1],
                s * v[3] - 6.0 * th * s * v[1] * v[2]
                    + (4.0 * th * th - 2.0 * s) * s * v[1].powi(3),
            ];
            yj[n][m + 1] = ode.w * (aj[m] - yj[n][m]);
        }
    }
    yj
}

/// The continuous nonlinear ladder `y1' = w (tanh(x - k y4) - y1)`, `yn' = w (tanh y(n-1) -
/// yn)`, driven by `x(t) = amplitude sin(2 pi f t)`.
struct Ode {
    w: f64,
    k: f64,
    amplitude: f64,
    f: f64,
}

impl Ode {
    fn x(&self, t: f64) -> f64 {
        self.amplitude * (TAU * self.f * t).sin()
    }

    fn rhs(&self, t: f64, y: &[f64; 4]) -> [f64; 4] {
        let a = [
            (self.x(t) - self.k * y[3]).tanh(),
            y[0].tanh(),
            y[1].tanh(),
            y[2].tanh(),
        ];
        core::array::from_fn(|n| self.w * (a[n] - y[n]))
    }

    fn jacobian(&self, t: f64, y: &[f64; 4]) -> [[f64; 4]; 4] {
        let u = self.x(t) - self.k * y[3];
        let sech2 = |v: f64| 1.0 - v.tanh() * v.tanh();
        let mut j = [[0.0; 4]; 4];
        for (n, row) in j.iter_mut().enumerate() {
            row[n] = -self.w;
        }
        j[0][3] = -self.w * self.k * sech2(u);
        for n in 1..4 {
            j[n][n - 1] = self.w * sech2(y[n - 1]);
        }
        j
    }
}

/// A hot sine through the 24 dB ladder against the continuous ladder ODE integrated by
/// Radau IIA at 64 times the rate (`1e-13` relative Newton tolerance, local error `O((w h)^6)` with
/// `w h < 1e-3` per substep: exact to the tolerance): `fc` 400 Hz, `k = 2.4`, a 250 Hz sine
/// of amplitude 3 at 48 kHz (`u` swings past 2: the saturators bite).
///
/// The ladder is the trapezoidal rule of step `h = 1 / fs` on the ODE with the corner
/// `w' = w (1 + d)`, `d = tan(w h / 2) / (w h / 2) - 1` (the prewarp). Its error `e` against
/// the ODE obeys, to the next order in `h`, `e' = J e + phi` with `J` the ODE's Jacobian along
/// its solution and `phi_n = (h^2 / 12) y_n''' + d y_n'` (the trapezoid's local error rate
/// and the corner's shift). The test integrates that variational equation along the same
/// Radau run, `y_n'''` and `y_n'` from the derivative jets of the ODE ([`state_jets`]), and
/// the ladder's difference from the reference at each of 480 samples is the predicted
/// error to `8 (w h)^2` of the prediction's peak (the neglected terms are a factor `(w h)^2`
/// smaller, `w h = 0.052`; 8 is the constant they carry).
#[test]
fn a_hot_sine_matches_the_continuous_ladder_ode_integrated_at_64_times_the_rate() {
    let (fc, res, amplitude, f) = (400.0, 0.6, 3.0, 250.0);
    let ode = Ode {
        w: TAU * fc,
        k: 4.0 * res,
        amplitude,
        f,
    };
    let c = coefficients(fc, res);
    let n = 480;
    let h = 1.0 / RATE.get();
    let wh = ode.w * h;
    let d = (wh / 2.0).tan() / (wh / 2.0) - 1.0;
    let substeps = 64;
    let hs = h / f64::from(substeps);

    let mut state = LadderState::<f64>::silent();
    let ladder: Vec<f64> = (0..n)
        .map(|i| nonlinear_ladder_step(&c, LadderType::LP24, &mut state, ode.x(i as f64 * h)))
        .collect();

    // The reference and the predicted error together: the 8-state system `(y, e)`.
    let rhs = |t: f64, z: &[f64; 8]| -> [f64; 8] {
        let y: [f64; 4] = core::array::from_fn(|i| z[i]);
        let e: [f64; 4] = core::array::from_fn(|i| z[4 + i]);
        let fy = ode.rhs(t, &y);
        let jets = state_jets(&ode, t, &y);
        let j = ode.jacobian(t, &y);
        core::array::from_fn(|i| {
            if i < 4 {
                fy[i]
            } else {
                let q = i - 4;
                (0..4).map(|p| j[q][p] * e[p]).sum::<f64>()
                    + h * h / 12.0 * jets[q][3]
                    + d * jets[q][1]
            }
        })
    };
    // The Newton iteration's Jacobian omits the coupling of `e'` to `y` (of the size of `e`,
    // about 1e-3): the iteration converges to the same stage slopes.
    let jac = |t: f64, z: &[f64; 8]| -> [[f64; 8]; 8] {
        let y: [f64; 4] = core::array::from_fn(|i| z[i]);
        let j = ode.jacobian(t, &y);
        let mut m = [[0.0; 8]; 8];
        for r in 0..4 {
            for q in 0..4 {
                m[r][q] = j[r][q];
                m[4 + r][4 + q] = j[r][q];
            }
        }
        m
    };
    let mut z = [0.0f64; 8];
    let mut t = 0.0;
    let mut predicted = Vec::with_capacity(n);
    let mut reference = Vec::with_capacity(n);
    let mut u_peak = 0.0f64;
    for _ in 0..n {
        reference.push(z[3]);
        predicted.push(z[7]);
        u_peak = u_peak.max((ode.x(t) - ode.k * z[3]).abs());
        for _ in 0..substeps {
            z = radau5_step(&rhs, &jac, t, &z, hs);
            t += hs;
        }
    }
    assert!(u_peak > 2.0, "the saturators bite: {u_peak}");
    let peak_error = predicted.iter().fold(0.0f64, |p, e| p.max(e.abs()));
    assert!(
        peak_error > 1e-5,
        "the discretization error is there to see: {peak_error:e}"
    );
    let tolerance = 8.0 * wh * wh * peak_error;
    let worst = (0..n)
        .map(|i| (ladder[i] - reference[i] - predicted[i]).abs())
        .fold(0.0f64, f64::max);
    assert!(
        worst <= tolerance,
        "ladder - ode - predicted error: {worst:e} over {tolerance:e} (peak error {peak_error:e})"
    );
}
