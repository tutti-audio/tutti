use super::*;
use tutti_buffers::{Planes, PlanesMut};

/// The rate the clipper runs at under the default oversampling: 4 times 48 kHz.
const FS: f64 = 192_000.0;

fn rate() -> SampleRate {
    SampleRate::new(FS).expect("a rate")
}

/// A mono clipper at `cutoff` hertz and `drive` decibels, landed.
fn clipper(cutoff: f64, drive: f64) -> DiodeClipper<f64> {
    let mut c = DiodeClipper::new(ChannelLayout::MONO, rate());
    c.retarget(&DiodeClipperControls {
        cutoff: Hz::new(cutoff),
        drive: ClipperDrive::of(Db::new(drive)),
    });
    c.jump_to_target();
    c
}

fn run(c: &mut DiodeClipper<f64>, x: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0; x.len()];
    let input = [x];
    let mut out = [&mut y[..]];
    let mut block = InputOutput::new(
        Samples::new(x.len()),
        Planes::new(&input).unwrap(),
        PlanesMut::new(&mut out).unwrap(),
    )
    .unwrap();
    c.process(&mut block).unwrap();
    y
}

fn sine(f: f64, amplitude: f64, n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| amplitude * (core::f64::consts::TAU * f * i as f64 / FS).sin())
        .collect()
}

/// The complex Fourier coefficient `2 / N sum y e^(-j w n)` of `y` at `f`, for a window of
/// whole periods of `f`.
fn coefficient(y: &[f64], f: f64) -> (f64, f64) {
    let (mut re, mut im) = (0.0, 0.0);
    for (i, &v) in y.iter().enumerate() {
        let a = core::f64::consts::TAU * f * i as f64 / FS;
        re += v * a.cos();
        im -= v * a.sin();
    }
    let k = 2.0 / y.len() as f64;
    (re * k, im * k)
}

fn magnitude(y: &[f64], f: f64) -> f64 {
    let (re, im) = coefficient(y, f);
    re.hypot(im)
}

/// `sinh` and `cosh` of the oracle, independent of the clipper's own.
fn sinh_cosh(u: f64) -> (f64, f64) {
    (u.sinh(), u.cosh())
}

/// A small signal sees the linear RC low-pass with its corner moved to `(1 + beta) f_c` (the
/// diodes' small-signal conductance is `beta` times the resistor's) and a DC gain of
/// `1 / (1 + beta)`: `A(f) = 1 / (1 + beta) / sqrt(1 + (f / f_eff)^2)`. The trapezoid prewarped at
/// `f_c` differs from the analog circuit by the tangent's warp of the frequency ratio,
/// `delta = tan(pi f / fs) f_eff / (tan(pi f_eff / fs) f) - 1` (about `(pi^2 / 3)(f^2 -
/// f_eff^2) / fs^2`), and the gain moves by at most `max R / (1 + R^2)^(3/2) = 0.385` times it.
/// The sinh's cubic term adds `beta u^2 / 6` relative (under `1e-12` at `-80 dBFS`), and the
/// window of whole periods leaves the rounding of the sum.
#[test]
fn a_minus_80_dbfs_sine_sees_the_linear_rc_low_pass_with_the_diodes_conductance() {
    let (cutoff, window) = (1_000.0, 3_072);
    let f_eff = (1.0 + BETA) * cutoff;
    for f in [250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0] {
        let mut c = clipper(cutoff, 0.0);
        let x = sine(f, 1e-4, 2 * window);
        let y = run(&mut c, &x);
        let gain = magnitude(&y[window..], f) / 1e-4;
        let ratio = f / f_eff;
        let analog = 1.0 / (1.0 + BETA) / (1.0 + ratio * ratio).sqrt();
        let pi = core::f64::consts::PI;
        let warp = ((pi * f / FS).tan() * f_eff / ((pi * f_eff / FS).tan() * f) - 1.0).abs();
        let bound = 0.385 * warp + 1e-12;
        assert!(
            (gain - analog).abs() <= bound,
            "{f} Hz: {gain} vs {analog}, bound {bound:e}"
        );
    }
}

/// At a cutoff far above the signal the capacitor is a short and `x = u + beta sinh u`; with
/// `sinh u ~ e^u / 2` its solution is `u = x - omega(x + ln(beta / 2))`, off by at most `beta / 2`
/// in `u` for `u >= 0`. The lag of the RC at the signal is `w / w_c` of the input's slope (the
/// slope of `u` in `x` is under 1), at most `(f / f_c) A` in `u`; the sum is the bound.
#[test]
fn a_slow_sine_follows_the_wright_omega_static_curve_when_the_cutoff_is_far_above_it() {
    let (f, amplitude_u) = (20.0, 12.0);
    let drive_db = 20.0 * (amplitude_u / SCALE).log10();
    let mut c = clipper(1.0e6, drive_db);
    let x = sine(f, 1.0, 3 * 9_600);
    let y = run(&mut c, &x);
    let cutoff = 0.4 * FS;
    let bound = BETA / 2.0 + f / cutoff * amplitude_u;
    let mut worst = 0.0f64;
    for (&y, &x) in y.iter().zip(&x) {
        let xu = x * amplitude_u;
        let static_u = xu.signum() * (xu.abs() - wright_omega(xu.abs() + (BETA / 2.0).ln()));
        worst = worst.max((y * SCALE - static_u).abs());
    }
    assert!(worst <= bound, "{worst:e} > {bound:e}");
    assert!(worst > 1e-5, "the lag is there to see: {worst:e}");
}

/// Radau IIA of three stages and order 5 on `u' = w (x(t) - u - beta sinh u)`, stage equations
/// by Newton on the 3x3 system to `1e-13` in `K`. Its local error is `O((w h)^6)`, under `1e-15`
/// at `w h = 2.5e-3`, so the reference is exact to the Newton tolerance.
struct Radau {
    w: f64,
    amplitude: f64,
    f: f64,
}

impl Radau {
    fn x(&self, t: f64) -> f64 {
        self.amplitude * (core::f64::consts::TAU * self.f * t).sin()
    }

    fn rhs(&self, t: f64, u: f64) -> f64 {
        self.w * (self.x(t) - u - BETA * u.sinh())
    }

    fn step(&self, t: f64, u: f64, h: f64) -> f64 {
        let r6 = 6.0f64.sqrt();
        let c = [(4.0 - r6) / 10.0, (4.0 + r6) / 10.0, 1.0];
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
        let mut k = [self.rhs(t, u); 3];
        for _ in 0..50 {
            let stage: [f64; 3] =
                core::array::from_fn(|i| u + h * (0..3).map(|j| a[i][j] * k[j]).sum::<f64>());
            let g: [f64; 3] = core::array::from_fn(|i| self.rhs(t + c[i] * h, stage[i]) - k[i]);
            let fu: [f64; 3] = core::array::from_fn(|i| -self.w * (1.0 + BETA * stage[i].cosh()));
            // (I - h A diag(fu)) dk = g, by elimination.
            let mut m = [[0.0; 4]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    m[i][j] = f64::from(u8::from(i == j)) - h * a[i][j] * fu[j];
                }
                m[i][3] = g[i];
            }
            for p in 0..3 {
                for i in p + 1..3 {
                    let r = m[i][p] / m[p][p];
                    for j in p..4 {
                        m[i][j] -= r * m[p][j];
                    }
                }
            }
            let mut dk = [0.0; 3];
            for i in (0..3).rev() {
                dk[i] = (m[i][3] - (i + 1..3).map(|j| m[i][j] * dk[j]).sum::<f64>()) / m[i][i];
            }
            for i in 0..3 {
                k[i] += dk[i];
            }
            if dk.iter().all(|d| d.abs() <= 1e-13) {
                break;
            }
        }
        u + h * (0..3).map(|j| a[2][j] * k[j]).sum::<f64>()
    }
}

/// A 1 kHz sine at +20 dB into a 5 kHz clipper (`u` swings to about 17 against an input of 220)
/// against the circuit's ODE, integrated at 64 times the clipper's rate. The clipper is the
/// trapezoid of step `h = 1 / fs` on the ODE with the corner `w_c (1 + d)`,
/// `d = (w_c h)^2 / 12` (the prewarp), so its error `e` against the ODE at `w_c` obeys
/// `e' = -lambda e + phi`, `phi = (h^2 / 12) u3 + d u1` (the trapezoid's local error rate and
/// the corner's shift; `u1` and `u3` the first and third derivatives of `u`) and `lambda >= w_c
/// (1 + beta cosh(|u| - 0.5))` while `|e| < 0.5` (the slope of the force; asserted). The
/// envelope `E' = -lambda E + |phi|` bounds `|e|` at every sample, stepped along the reference
/// with `u1` and `u3` from the ODE, and the RMS error over one steady-state period is under the
/// envelope's.
#[test]
fn a_hot_1_khz_sine_matches_the_circuit_ode_integrated_at_64_times_the_rate() {
    let (f, cutoff, drive_db) = (1_000.0, 5_000.0, 20.0);
    let amplitude = (drive_db / 20.0 * core::f64::consts::LN_10).exp() * SCALE;
    let w = core::f64::consts::TAU * cutoff;
    let radau = Radau { w, amplitude, f };
    let period = 192;
    let periods = 6;
    let n = period * periods;
    let mut c = clipper(cutoff, drive_db);
    let y = run(&mut c, &sine(f, 1.0, n));
    let h = 1.0 / FS;
    let substeps = 64;
    let hs = h / f64::from(substeps);
    let d = (w * h) * (w * h) / 12.0;
    let (mut u, mut t, mut envelope) = (0.0, 0.0, 0.0f64);
    let (mut sum, mut sum_envelope, mut worst) = (0.0, 0.0, 0.0f64);
    for (i, &sample) in y.iter().enumerate() {
        // The reference at the sample `i`, then 64 steps to the next.
        if i >= (periods - 1) * period {
            let err = sample * SCALE - u;
            sum += err * err;
            sum_envelope += envelope * envelope;
            worst = worst.max(err.abs());
        }
        for _ in 0..substeps {
            let (sinh, cosh) = sinh_cosh(u);
            let u1 = radau.rhs(t, u);
            let xt = core::f64::consts::TAU * f;
            let x1 = amplitude * xt * (xt * t).cos();
            let x2 = -amplitude * xt * xt * (xt * t).sin();
            let u2 = w * (x1 - (1.0 + BETA * cosh) * u1);
            let u3 = w * (x2 - (1.0 + BETA * cosh) * u2 - BETA * sinh * u1 * u1);
            let phi = (h * h / 12.0) * u3.abs() + d * u1.abs();
            let lambda = w * (1.0 + BETA * (u.abs() - 0.5).max(0.0).cosh());
            let decay = (-lambda * hs).exp();
            envelope = envelope * decay + phi * (1.0 - decay) / lambda;
            u = radau.step(t, u, hs);
            t += hs;
        }
    }
    let rms = (sum / period as f64).sqrt();
    let bound = (sum_envelope / period as f64).sqrt();
    assert!(worst < 0.5, "the envelope's premise: {worst}");
    assert!(rms <= bound, "rms {rms:e} > bound {bound:e}");
    assert!(
        rms > bound / 10.0,
        "the bound is tight to a decade: rms {rms:e}, bound {bound:e}"
    );
}

/// The trapezoid's discretized equation `u[n] - u[n-1] = g (F[n-1] + F[n])`, `F = x - u - beta
/// sinh u`, read back from the clipper's own outputs and inputs. Its residual after the cap is
/// the arithmetic's: the cap leaves a Newton error under `1e-24` (see the module), so the
/// residual is the sum of the roundings of the terms, `8 eps` of their sum of magnitudes.
/// Over the drive range, at the default oversampling's rate, at low, mid and the highest
/// cutoff (`g` from `3e-3` to 3), and at `-60 dB` where `u` is near 0, `1e-12` above the
/// floor were the cap one step.
#[test]
fn the_residual_of_the_trapezoid_equation_after_the_cap_is_rounding() {
    let eps = f64::EPSILON;
    for cutoff in [200.0, 5_000.0, 40_000.0, 0.4 * FS] {
        let g = (core::f64::consts::PI * cutoff / FS).tan();
        for drive_db in [-60.0, -20.0, 0.0, 20.0, 40.0, 60.0] {
            let amp = 10f64.powf(drive_db / 20.0);
            let gain = amp * SCALE;
            for f in [100.0, 3_000.0, 12_000.0] {
                let mut c = clipper(cutoff, drive_db);
                let x_in = sine(f, 1.0, 2_000);
                let y = run(&mut c, &x_in);
                let x: Vec<f64> = x_in.iter().map(|v| v * gain).collect();
                let u: Vec<f64> = y.iter().map(|v| v * SCALE).collect();
                let force = |n: usize| x[n] - u[n] - BETA * u[n].sinh();
                for n in 1..x.len() {
                    let rho = u[n] - u[n - 1] - g * (force(n - 1) + force(n));
                    let size = u[n].abs()
                        + u[n - 1].abs()
                        + g * (x[n].abs()
                            + x[n - 1].abs()
                            + u[n].abs()
                            + u[n - 1].abs()
                            + BETA
                                * (u[n].sinh().abs()
                                    + u[n - 1].sinh().abs()
                                    + u[n].cosh() * u[n].abs()
                                    + u[n - 1].cosh() * u[n - 1].abs()));
                    assert!(
                        rho.abs() <= 8.0 * eps * size,
                        "fc {cutoff}, {drive_db} dB, {f} Hz, n {n}: {rho:e} vs {:e}",
                        8.0 * eps * size
                    );
                }
            }
        }
    }
}

/// The antiparallel pair is odd: a negated input gives exactly the negated output, bit for
/// bit (every step of the solve is odd in its input), over a two-tone hot signal.
#[test]
fn a_negated_input_gives_the_negated_output_bit_for_bit() {
    let x: Vec<f64> = sine(700.0, 0.9, 4_000)
        .iter()
        .zip(sine(4_300.0, 0.4, 4_000))
        .map(|(a, b)| a + b)
        .collect();
    let negated: Vec<f64> = x.iter().map(|v| -v).collect();
    let y = run(&mut clipper(3_000.0, 30.0), &x);
    let z = run(&mut clipper(3_000.0, 30.0), &negated);
    assert!(y.iter().zip(&z).all(|(a, b)| *a == -*b));
    assert!(y.iter().any(|v| v.abs() > 0.1), "the signal is there");
}

/// A sine's steady state is odd under a half period (`u(t + T / 2) = -u(t)`), so it has no
/// even harmonics; the transient decays as `e^(-w_c t)`, gone in the window, and what remains
/// is rounding, `1e-12` of the fundamental.
#[test]
fn a_hot_sine_has_no_even_harmonics() {
    let (f, window) = (1_000.0, 192 * 8);
    let mut c = clipper(5_000.0, 30.0);
    let y = run(&mut c, &sine(f, 1.0, 2 * window));
    let y = &y[window..];
    let fundamental = magnitude(y, f);
    assert!(
        magnitude(y, 3.0 * f) > 0.01 * fundamental,
        "the odd harmonics are there"
    );
    for k in [2.0, 4.0, 6.0, 8.0, 10.0] {
        assert!(magnitude(y, k * f) <= 1e-12 * fundamental, "harmonic {k}");
    }
}

/// With `g <= 1` the linear stage's response is a positive kernel (`(1 - g) / (1 + g) >= 0`)
/// of unit sum, so it never exceeds the input's peak, and the diodes only draw current away.
/// The output is the input's drive times the signal's peak, to the rounding of the scaling,
/// over a sine and a square wave (a step is the worst case for a trapezoid).
#[test]
fn the_output_never_exceeds_the_inputs_peak_when_the_prewarp_is_at_most_one() {
    for cutoff in [500.0, 5_000.0, 40_000.0] {
        for drive_db in [-20.0, 0.0, 20.0, 40.0] {
            let amp = 10f64.powf(drive_db / 20.0);
            let sine_wave = sine(1_500.0, 0.8, 3_000);
            let square: Vec<f64> = (0..3_000)
                .map(|i| if (i / 96) % 2 == 0 { 0.8 } else { -0.8 })
                .collect();
            for x in [sine_wave, square] {
                let y = run(&mut clipper(cutoff, drive_db), &x);
                let peak = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                assert!(
                    peak <= amp * 0.8 * (1.0 + 8.0 * f64::EPSILON),
                    "{cutoff} Hz {drive_db} dB: {peak}"
                );
            }
        }
    }
}

/// The output decays to the tail floor within `tail` frames of the input stopping: a
/// signal that has settled at DC decays as `c^n`, `c = (1 - g') / (1 + g')`, from
/// `u / (1 + g')`, and `tail` is `ceil(ln floor / ln c) + 1`.
#[test]
fn the_tail_is_the_frames_the_linear_decay_takes_to_reach_the_floor() {
    let controls = DiodeClipperControls {
        cutoff: Hz::new(300.0),
        drive: ClipperDrive::MIN,
    };
    let mut c = clipper(300.0, -60.0);
    let steady = run(&mut c, &vec![1.0; 20_000]);
    let held = *steady.last().unwrap();
    let tail = controls.tail(rate()).get();
    let silent = run(&mut c, &vec![0.0; tail]);
    let floor = TAIL_FLOOR.to_amplitude().get();
    assert!(
        silent[tail - 1].abs() <= floor * held.abs(),
        "{} vs {}",
        silent[tail - 1],
        floor * held
    );
    assert!(
        silent[tail / 2].abs() > floor * held.abs(),
        "the floor is not reached in half the tail"
    );
}

/// A cutoff past 0.4 of the rate is held there, one below the lowest at the lowest; a value
/// that is not finite reads as the default's.
#[test]
fn the_cutoff_is_held_in_its_documented_range() {
    let held = |cutoff: f64| {
        DiodeClipperControls {
            cutoff: Hz::new(cutoff),
            ..DiodeClipperControls::DEFAULT
        }
        .held_cutoff(rate())
        .get()
    };
    assert_eq!(held(1.0e9), 0.4 * FS);
    assert_eq!(held(-5.0), 20.0);
    assert_eq!(held(f64::NAN), 5_000.0);
}

/// **The drive runs from -60 dB to +60 dB**, the range the solver's residual is stated over:
/// both ends are valid, and a value past either, or not a number, is refused.
#[test]
fn a_drive_takes_exactly_the_documented_range() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: ClipperDrive::new,
        get: ClipperDrive::get,
        of: Some(ClipperDrive::of),
        valid: &[Db::new(-60.0), Db::new(12.0), Db::new(60.0)],
        invalid: &[
            Db::new(-60.001),
            Db::new(60.001),
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
    let held = |db: f64| ClipperDrive::held(Db::new(db)).get().get();
    assert_eq!(held(1.0e9), 60.0);
    assert_eq!(held(f64::INFINITY), 60.0);
    assert_eq!(held(-1.0e9), -60.0);
    assert_eq!(held(f64::NEG_INFINITY), -60.0);
    assert_eq!(held(f64::NAN), 12.0);
    assert_eq!(held(-17.5), -17.5);
}

#[test]
fn a_block_of_another_width_is_refused_before_a_sample_is_touched() {
    let mut c = DiodeClipper::<f64>::new(ChannelLayout::STEREO, rate());
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
    assert!(c.process(&mut block).is_err());
    assert_eq!(y, [7.0; 4]);
}
