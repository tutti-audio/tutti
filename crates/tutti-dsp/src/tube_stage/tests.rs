use super::*;
use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::{amplitude_at, lock_in};
use tutti_units::Hz;
use tutti_units::SampleRate;

/// The rate the stage runs at under the default oversampling: 4 times 48 kHz.
const FS: f64 = 192_000.0;
/// The frames of one period of 1 kHz at [`FS`].
const PERIOD: usize = 192;

/// The error in the plate voltage from the seed alone after the cap, volts: measured worst
/// `2.8e-10` over the grid voltages `+/- 35` V, rounded up.
const SEED_ERROR: f64 = 5e-10;

fn rate() -> SampleRate {
    SampleRate::new(FS).expect("a rate")
}

/// A mono stage at `drive` decibels, landed.
fn stage(drive: f64) -> TubeStage<f64> {
    let mut s = TubeStage::new(ChannelLayout::MONO, rate());
    s.retarget(&TubeStageControls {
        drive: TubeDrive::of(Db::new(drive)),
    });
    s.jump_to_target();
    s
}

fn run(s: &mut TubeStage<f64>, x: &[f64]) -> Vec<f64> {
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

fn sine(f: f64, amplitude: f64, n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| amplitude * (core::f64::consts::TAU * f * i as f64 / FS).sin())
        .collect()
}

/// Koren's plate current written from the paper with the textbook `ln(1 + e^z)` (past `z = 30`
/// it is `z` to `1e-13`, which keeps `e^z` finite), independent of the stage's own form.
fn koren(vpk: f64, vgk: f64) -> f64 {
    let z = KP * (1.0 / MU + vgk / (KVB + vpk * vpk).sqrt());
    let softplus = if z > 30.0 { z } else { (1.0 + z.exp()).ln() };
    let e1 = vpk / KP * softplus;
    if e1 > 0.0 {
        2.0 * e1.powf(EX) / KG1
    } else {
        0.0
    }
}

/// The root of `V - B+ + R_p I_p(V, vg)` by bisection over `0..=B+`, to the width of `f64`.
fn load_line(vg: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, SUPPLY);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if mid - SUPPLY + PLATE_LOAD * koren(mid, vg) > 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The self-biased rest point `(V_gk, V_pk)`: `V_gk = -R_k I_p` on the load line, by bisection
/// of `I_p R_k - V_k` over `0..=5` V.
fn rest_point() -> (f64, f64) {
    let (mut lo, mut hi) = (0.0, 5.0);
    for _ in 0..80 {
        let vk = 0.5 * (lo + hi);
        if koren(load_line(-vk), -vk) * CATHODE_RESISTOR > vk {
            lo = vk;
        } else {
            hi = vk;
        }
    }
    let grid = -0.5 * (lo + hi);
    (grid, load_line(grid))
}

/// The first derivative of `f` at `x` by the five-point stencil, error `h^4 f^(5) / 30`.
fn derivative(f: impl Fn(f64) -> f64, x: f64, h: f64) -> f64 {
    (-f(x + 2.0 * h) + 8.0 * f(x + h) - 8.0 * f(x - h) + f(x - 2.0 * h)) / (12.0 * h)
}

/// The second and fourth derivatives of `f` at `x` by five-point stencils, error `O(h^4)`
/// and `O(h^2)`.
fn even_derivatives(f: impl Fn(f64) -> f64, x: f64, h: f64) -> (f64, f64) {
    let (m2, m1, c, p1, p2) = (f(x - 2.0 * h), f(x - h), f(x), f(x + h), f(x + 2.0 * h));
    (
        (-p2 + 16.0 * p1 - 30.0 * c + 16.0 * m1 - m2) / (12.0 * h * h),
        (p2 - 4.0 * p1 + 6.0 * c - 4.0 * m1 + m2) / h.powi(4),
    )
}

/// The rest point is the self-bias point of the load line (`V_gk = -R_k I_p`, solved here by
/// bisection on the paper's formula) to the two bisections' width and the rounding of `f64`
/// (`1e-9` V is orders above both), and a stage at rest holds the plate there: the solve's error
/// is under [`SEED_ERROR`], so the output, that distance per volt, is under [`SEED_ERROR`] per
/// `|A|`.
#[test]
fn the_rest_point_is_the_self_bias_point_of_the_load_line() {
    let (grid, plate) = rest_point();
    let found = Operating::solve();
    assert!(
        (found.grid - grid).abs() <= 1e-9,
        "{} vs {grid}",
        found.grid
    );
    assert!((found.plate - plate).abs() <= 1e-9);
    assert!(
        (koren(plate, grid) * CATHODE_RESISTOR + grid).abs() <= 1e-9,
        "the cathode voltage is the bias"
    );
    assert!(grid < -0.5 && grid > -3.0, "a bias of a few volts: {grid}");
    let mut s = stage(0.0);
    let y = run(&mut s, &vec![0.0; 2_000]);
    assert!(y.iter().all(|v| v.abs() <= SEED_ERROR * s.per_volt));
}

/// The stage's gain for a -80 dBFS sine is the small-signal `A = -g_m (r_p || R_p)` at the rest
/// point, `g_m = dI_p / dV_gk` and `1 / r_p = dI_p / dV_pk` by the five-point stencil on the
/// paper's formula (error `h^4 f^(5) / 30`, under `1e-10` of the value at `h = 1e-3`), so the
/// normalized output has gain `|H|`, the coupling high-pass's `|1 - e^(-jw)| / |1 - R e^(-jw)|`
/// with `R = exp(-2 pi f_c / f_s)` (the normalization is `1 / |A|`). The error is the
/// plate's cubic term, `(A3 / A1) a^2 3 / 4` under `1e-8` at `a = 1e-4` V, and the Newton
/// error, smooth in the grid voltage, at worst `SEED_ERROR / (|A| a) = 8e-8` against a plate swing
/// of `|A| a`, the sum rounded up to `1e-7`. The sine settles for 420 periods, 27 time constants of the coupling.
#[test]
fn a_minus_80_dbfs_sine_has_the_small_signal_gain_of_the_model() {
    let (grid, plate) = rest_point();
    let gm = derivative(|g| koren(plate, g), grid, 1e-3);
    let gv = derivative(|v| koren(v, grid), plate, 1e-3);
    let a = gm / (gv + 1.0 / PLATE_LOAD);
    assert!(a > 30.0 && a < 100.0, "a 12AX7 stage's gain is tens: {a}");
    let f = 1_000.0;
    let w = core::f64::consts::TAU * f / FS;
    let r = (-core::f64::consts::TAU * DC_CORNER.get() / FS).exp();
    let highpass = ((1.0 - w.cos()).hypot(w.sin())) / ((1.0 - r * w.cos()).hypot(r * w.sin()));
    let settle = 420 * PERIOD;
    let mut s = stage(0.0);
    let y = run(&mut s, &sine(f, 1e-4, settle + 16 * PERIOD));
    let gain = amplitude_at(&y[settle..], Hz::new(f), SampleRate::of(FS)) / 1e-4;
    let bound = 1e-7 * highpass;
    assert!(
        (gain - highpass).abs() <= bound,
        "{gain} vs {highpass}, bound {bound:e}"
    );
    assert!(
        (a * s.per_volt - 1.0).abs() <= 1e-8,
        "the normalization is 1 / |A|: {}",
        a * s.per_volt
    );
}

/// A triode is asymmetric: its transfer from grid to plate has curvature `A2 = d^2 V_pk / d
/// V_gk^2` at the rest point, so a sine of amplitude `a` gives a second harmonic. Expanding the
/// normalized output `z = (V_p0 - V_pk) / |A|` in the grid swing `d = a sin(wt)`, `z = d - (A2 / 2|A|) d^2
/// + ...`, and `sin^2 = (1 - cos 2wt) / 2`, the `cos 2wt` coefficient is `A2 a^2 / (4 |A|)`
/// plus the next even term, `A4 a^4 / (48 |A|)` (`A4` the fourth derivative). `A2` and `A4` are
/// the five-point stencils of the load-line root solved by bisection (errors under `1e-6` of
/// the value at `h = 0.05`); the test checks the sign (that of `A2`), and the size within twice
/// the fourth-order term plus `0.1%` for the sixth.
#[test]
fn a_moderate_sine_has_a_second_harmonic_of_the_models_curvature() {
    let (grid, _) = rest_point();
    let amplitude = 0.1;
    let (a2, a4) = even_derivatives(load_line, grid, 0.05);
    let first = derivative(load_line, grid, 0.05).abs();
    assert!(a2.abs() > 1.0, "the curvature is there: {a2}");
    let expected = a2 * amplitude * amplitude / (4.0 * first);
    let fourth = (a4 * amplitude.powi(4) / (48.0 * first)).abs();
    let f = 1_000.0;
    let settle = 420 * PERIOD;
    let mut s = stage(20.0 * amplitude.log10());
    let y = run(&mut s, &sine(f, 1.0, settle + 16 * PERIOD));
    let (cos2, sin2) = lock_in(&y[settle..], Hz::new(2.0 * f), SampleRate::of(FS));
    // The coupling high-pass `H = (1 - e^(-jw)) / (1 - R e^(-jw))` at 2f turns the `cos 2wt`
    // coefficient into `(Re H, Im H)` times itself.
    let w = core::f64::consts::TAU * 2.0 * f / FS;
    let r = (-core::f64::consts::TAU * DC_CORNER.get() / FS).exp();
    let (nr, ni) = (1.0 - w.cos(), w.sin());
    let (dr, di) = (1.0 - r * w.cos(), r * w.sin());
    let norm = dr * dr + di * di;
    let h = ((nr * dr + ni * di) / norm, (ni * dr - nr * di) / norm);
    assert_eq!(cos2.signum(), a2.signum(), "{cos2} vs {expected}");
    let bound = 2.0 * fourth + 1e-3 * expected.abs();
    assert!(
        (cos2 - expected * h.0).abs() <= bound && (sin2 - expected * h.1).abs() <= bound,
        "({cos2}, {sin2}) vs {expected} x {h:?}, bound {bound:e}"
    );
}

/// The analytic slope is the derivative of the paper's `I_p` (central difference, error
/// `h^2 f''' / 6`, `1e-6` of the value at `h = 1e-4` V) and is non-negative: `I_p` rises with
/// `V_pk`, which makes the load line increasing with a slope of at least 1 and its root unique.
#[test]
fn the_plate_current_rises_with_the_plate_voltage_at_the_slope_newton_uses() {
    let triode = Triode::<f64>::new();
    for vg in [-30.0, -5.0, -2.0, -1.0, 0.0, 1.0, 5.0, 30.0] {
        for k in 1..=100 {
            let v = 2.5 * f64::from(k);
            let (_, slope) = triode.current(v, vg);
            let h = 1e-4;
            let fd = (koren(v + h, vg) - koren(v - h, vg)) / (2.0 * h);
            assert!(slope >= 0.0, "{v} V, {vg} V: {slope}");
            assert!(
                (slope - fd).abs() <= 1e-6 * fd.abs() + 1e-15,
                "{v} V, {vg} V: {slope} vs {fd}"
            );
        }
    }
}

/// The solve reaches the load line's root, found by bisection, within [`SEED_ERROR`] over the
/// grid voltages `+/- 35` V in 0.05 V steps. The residual is under the error times the slope
/// (`f' >= 1` makes the error at most the residual), and the root stays between ground and the
/// supply. A fourth step is needed: after three the error reaches `4e-4` V.
#[test]
fn the_residual_after_the_cap_is_under_the_bound_over_the_grid_range() {
    let triode = Triode::<f64>::new();
    let mut three_steps = 0.0f64;
    for k in -700..=700 {
        let vg = 0.05 * f64::from(k);
        let root = load_line(vg);
        assert!(root > 0.0 && root <= SUPPLY, "{vg}: {root}");
        let v = triode.solve(vg);
        let (residual, slope) = triode.residual(vg, v);
        assert!((v - root).abs() <= SEED_ERROR, "{vg} V: {v} vs {root}");
        assert!(
            residual.abs() <= SEED_ERROR * slope,
            "{vg} V: residual {residual:e}"
        );
        let mut v = triode.seed(vg);
        for _ in 0..TUBE_NEWTON_CAP - 1 {
            let (value, slope) = triode.residual(vg, v);
            v -= value / slope;
        }
        three_steps = three_steps.max((v - root).abs());
    }
    assert!(
        three_steps > 1e-4,
        "one step less leaves {three_steps:e}, the cap is the least"
    );
}

/// The static stage against the load line solved for every sample by bisection (exact to the
/// width of `f64`) and carried through the coupling high-pass `y[n] = R y[n-1] + x[n] - x[n-1]`
/// written from its pole. The stage's solve leaves an error of at most [`SEED_ERROR`] volts per
/// sample, `SEED_ERROR / |A|` normalized; the high-pass's impulse response is `1, -(1-R), -(1-R) R,
/// ...`, of absolute sum under 2, so the output differs by at most twice that, plus the
/// rounding of the recursion. At +6 dB and +24 dB (the grid swings to +14.5 V, into the
/// saturated plate).
#[test]
fn a_hot_sine_follows_the_load_line_solved_by_bisection() {
    let (grid, plate) = rest_point();
    let r = (-core::f64::consts::TAU * DC_CORNER.get() / FS).exp();
    for drive in [6.0, 24.0] {
        let mut s = stage(drive);
        let x = sine(1_000.0, 1.0, 6 * PERIOD);
        let y = run(&mut s, &x);
        let amp = 10f64.powf(drive / 20.0) * FULL_SCALE;
        let (mut shaped_prev, mut out_prev) = (0.0, 0.0);
        let mut worst = 0.0f64;
        let mut swing = 0.0f64;
        for (&x, &got) in x.iter().zip(&y) {
            let shaped = (plate - load_line(grid + amp * x)) * s.per_volt;
            let out = r * out_prev + shaped - shaped_prev;
            (shaped_prev, out_prev) = (shaped, out);
            worst = worst.max((got - out).abs());
            swing = swing.max(out.abs());
        }
        let bound = 2.0 * SEED_ERROR * s.per_volt;
        assert!(worst <= bound, "{drive} dB: {worst:e} > {bound:e}");
        assert!(swing > 0.5, "{drive} dB: the signal is there: {swing}");
    }
}

/// The render does not depend on where blocks fall: the drive is designed on a grid counted
/// from the reset, so a moving drive renders the same bit for bit in blocks of 1, 7 and 192.
#[test]
fn the_render_is_the_same_in_any_blocks() {
    let x = sine(900.0, 1.0, 1_000);
    let render = |size: usize| {
        let mut s = stage(0.0);
        s.retarget(&TubeStageControls {
            drive: TubeDrive::of(Db::new(24.0)),
        });
        x.chunks(size)
            .flat_map(|c| run(&mut s, c))
            .collect::<Vec<f64>>()
    };
    let whole = render(x.len());
    assert!(whole.iter().any(|v| v.abs() > 0.5), "the signal is there");
    for size in [1, 7, 192] {
        assert_eq!(render(size), whole, "blocks of {size}");
    }
}

/// **The drive runs from -60 dB to +30 dB**, the range the solver's residual is stated over:
/// both ends are valid, and a value past either, or not a number, is refused.
#[test]
fn a_drive_takes_exactly_the_documented_range() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: TubeDrive::new,
        get: TubeDrive::get,
        of: Some(TubeDrive::of),
        valid: &[Db::new(-60.0), Db::new(6.0), Db::new(30.0)],
        invalid: &[
            Db::new(-60.001),
            Db::new(30.001),
            Db::new(f64::NAN),
            Db::new(f64::INFINITY),
            Db::new(f64::NEG_INFINITY),
        ],
    });
}

/// A live drive value (a control signal has no caller to refuse it to) is held in the
/// range: past an end it reads that end, a NaN reads the default, and a value in range
/// reads as itself.
#[test]
fn a_live_drive_is_held_in_the_range() {
    let held = |db: f64| TubeDrive::held(Db::new(db)).get().get();
    assert_eq!(held(1.0e9), 30.0);
    assert_eq!(held(f64::INFINITY), 30.0);
    assert_eq!(held(-1.0e9), -60.0);
    assert_eq!(held(f64::NEG_INFINITY), -60.0);
    assert_eq!(held(f64::NAN), 6.0);
    assert_eq!(held(-17.5), -17.5);
}

#[test]
fn a_block_of_another_width_is_refused_before_a_sample_is_touched() {
    let mut s = TubeStage::<f64>::new(ChannelLayout::STEREO, rate());
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
