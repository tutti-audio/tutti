use super::*;
use crate::Complex64;
use tutti_buffers::InputOutput;
use tutti_buffers::{Planes, PlanesMut};

const RATE: SampleRate = SampleRate::SR_48K;

fn run(blocker: &mut DcBlocker<f64>, input: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; input[0].len()]; input.len()];
    blocker
        .process(
            &mut InputOutput::paired(
                Planes::from_vecs(input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    out
}

fn at(corner: f64, rate: SampleRate) -> DcBlocker<f64> {
    let mut b = DcBlocker::<f64>::new(ChannelLayout::MONO, rate);
    b.retarget(&DcBlockerControls {
        corner: Hz::new(corner),
    });
    b.jump_to_target();
    b
}

/// The pole is `exp(-2 pi fc / fs)` at each rate: at 10 Hz and 48 kHz it is
/// `exp(-0.0013089969...)`, and a higher rate is nearer 1.
#[test]
fn the_pole_is_the_exponential_of_the_corner_over_the_rate() {
    for rate in [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K] {
        for fc in [1.0f64, 10.0, 20.0, 100.0] {
            let want = (-2.0 * std::f64::consts::PI * fc / rate.get()).exp();
            assert_eq!(dc_pole(Hz::new(fc), rate), want, "{fc} Hz at {rate:?}");
        }
    }
    assert!((dc_pole(DC_CORNER, RATE) - 0.998_691_8).abs() < 1e-7);
    assert!(dc_pole(DC_CORNER, SampleRate::SR_96K) > dc_pole(DC_CORNER, RATE));
}

/// A corner under the floor reads as the floor, one past Nyquist as Nyquist, and one that
/// is not finite as the default.
#[test]
fn the_corner_is_held_within_its_bounds() {
    assert_eq!(dc_pole(Hz::ZERO, RATE), dc_pole(MIN_DC_CORNER, RATE));
    assert_eq!(dc_pole(Hz::new(-5.0), RATE), dc_pole(MIN_DC_CORNER, RATE));
    assert_eq!(
        dc_pole(Hz::new(1e9), RATE),
        dc_pole(Hz::new(24_000.0), RATE)
    );
    assert_eq!(dc_pole(Hz::new(f64::NAN), RATE), dc_pole(DC_CORNER, RATE));
}

/// A constant input `c` decays as `y[n] = c R^n` (the step response of the difference
/// equation from zero state), at each channel alone.
#[test]
fn a_constant_decays_as_the_pole_to_the_n() {
    let mut b = DcBlocker::<f64>::new(ChannelLayout::STEREO, RATE);
    b.retarget(&DcBlockerControls {
        corner: Hz::new(20.0),
    });
    b.jump_to_target();
    let r = b.pole();
    let out = run(&mut b, &[vec![0.5; 2000], vec![-1.0; 2000]]);
    for n in [0usize, 1, 2, 10, 500, 1999] {
        let rn = r.powi(n as i32);
        assert!((out[0][n] - 0.5 * rn).abs() < 1e-12, "left {n}");
        assert!((out[1][n] + rn).abs() < 1e-12, "right {n}");
    }
}

/// The settled gain of a sine at `f` is `|1 - e^{-jw}| / |1 - R e^{-jw}|`, the difference
/// equation's own response, at frequencies a decade under the corner to well above it.
#[test]
fn the_gain_is_the_closed_form_of_the_difference_equation() {
    let fs = RATE.get();
    for corner in [10.0f64, 40.0] {
        for f in [1.0f64, 10.0, 40.0, 200.0, 1000.0, 10_000.0] {
            let mut b = at(corner, RATE);
            let r = b.pole();
            let w = std::f64::consts::TAU * f / fs;
            // Two seconds to settle, then one second (a whole number of cycles of every
            // `f` here) is correlated with the sine and cosine: the amplitude does not
            // depend on where the samples fall on the crest.
            let n = (fs * 3.0) as usize;
            let x: Vec<f64> = (0..n).map(|i| (w * i as f64).sin()).collect();
            let y = &run(&mut b, &[x])[0];
            let start = 2 * fs as usize;
            let (mut si, mut co) = (0.0, 0.0);
            for (i, v) in y[start..].iter().enumerate() {
                let phase = w * (start + i) as f64;
                si += v * phase.sin();
                co += v * phase.cos();
            }
            let peak = 2.0 / (n - start) as f64 * si.hypot(co);
            let (c, s) = (w.cos(), w.sin());
            let num = ((1.0 - c).powi(2) + s * s).sqrt();
            let den = ((1.0 - r * c).powi(2) + (r * s).powi(2)).sqrt();
            let want = num / den;
            assert!(
                (peak - want).abs() < 1e-12,
                "{f} Hz, corner {corner}: {peak} against {want}"
            );
        }
    }
}

/// The ring-out `tail` covers the worst stopped input: `-A` held until settled, then `+A`
/// (for one frame, or held), then silence. Past the first tap the response is one sign, so the
/// output after the stop is `A R^(n-1)`, under `TAIL_FLOOR` of `A` from `tail` frames on and
/// still over it a few frames before.
#[test]
fn the_tail_covers_the_worst_stopped_input_to_the_floor() {
    let floor = 10f64.powf(-3.0);
    for corner in [1.0f64, 10.0, 100.0] {
        let controls = DcBlockerControls {
            corner: Hz::new(corner),
        };
        let n = controls.tail(RATE).get();
        let settle = 20 * n;
        for hold in [1usize, 1000] {
            let mut x = vec![-1.0; settle];
            x.extend(vec![1.0; hold]);
            let stop = x.len();
            x.extend(vec![0.0; n + 64]);
            let y = &run(&mut at(corner, RATE), &[x])[0];
            assert!(
                y[stop + n..].iter().all(|v| v.abs() < floor),
                "{corner} Hz, hold {hold}"
            );
            if hold == 1 {
                assert!(
                    y[stop + n - 4].abs() >= floor,
                    "{corner} Hz: the bound is long by more than a few frames"
                );
            }
        }
    }
}

/// A pole move ramps over `COEFF_INTERVAL` frames. Jumping the corner from 10 Hz to 1 kHz at
/// the crest of a 100 Hz unit sine, the first frame after the jump steps by at most
/// `omega + (1 - r1)`: the input's largest step (`omega = 2 pi f / fs`, amplitude 1) plus the
/// recursion's `(r1 - 1) y` with `|y| <= 1` (the high-pass gain at 100 Hz is under 1), where
/// `r1` is the pole one ramp step along. Swapping the pole at once steps by the pole's
/// change times `y`, about 0.12, over this bound of about 0.022.
#[test]
fn a_corner_jump_steps_the_output_by_the_ramp_share() {
    let (f, fs) = (100.0f64, RATE.get());
    let w = std::f64::consts::TAU * f / fs;
    let crest = 120; // a quarter of the 480-frame period
    let jump = 480 * 10 + crest;
    let x: Vec<f64> = (0..jump + 64).map(|i| (w * i as f64).sin()).collect();
    let mut b = at(10.0, RATE);
    let first = run(&mut b, &[x[..jump].to_vec()]);
    b.retarget(&DcBlockerControls {
        corner: Hz::new(1000.0),
    });
    let second = run(&mut b, &[x[jump..].to_vec()]);
    let (r0, r1) = (dc_pole(Hz::new(10.0), RATE), dc_pole(Hz::new(1000.0), RATE));
    let r1_step = r0 + (r1 - r0) / COEFF_INTERVAL as f64;
    let bound = w + (1.0 - r1_step);
    let y_before = *first[0].last().unwrap();
    assert!(y_before > 0.9, "the jump falls on y's crest: {y_before}");
    let step = (second[0][0] - y_before).abs();
    assert!(step <= bound + 1e-9, "{step} against {bound}");
    let steps = second[0].windows(2).map(|p| (p[1] - p[0]).abs());
    assert!(steps.fold(0.0, f64::max) <= w + (1.0 - r1) + 1e-9);
}

/// `reset` drops the state: a blocker that has settled on 1 renders a held 0 as 0 after
/// it, and as `-R^n` before.
#[test]
fn reset_clears_the_state() {
    let mut b = at(20.0, RATE);
    run(&mut b, &[vec![1.0; 100]]);
    assert_ne!(run(&mut b, &[vec![0.0; 4]])[0][0], 0.0);
    b.reset();
    assert!(run(&mut b, &[vec![0.0; 4]])[0].iter().all(|&v| v == 0.0));
}

/// An `f64` blocker is the difference equation `y[n] = x[n] - x[n-1] + R y[n-1]` in `f64`
/// at the pole `R = exp(-2 pi fc / fs)` computed here from the corner: the pole is not an
/// `f32` value and the signal has more than 24 bits, so a state narrowed to `f32` misses
/// it by far more than 2^-45.
#[test]
fn an_f64_blocker_is_the_difference_equation_in_f64() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let (fc, len) = (10.0f64, 400usize);
    let r = (-std::f64::consts::TAU * fc / RATE.get()).exp();
    assert_ne!(r, f64::from(r as f32), "the pole needs more than f32 holds");
    let x = wide_signal(len);
    let mut want = vec![0.0f64; len];
    let (mut x1, mut y1) = (0.0f64, 0.0f64);
    for n in 0..len {
        y1 = x[n] - x1 + r * y1;
        x1 = x[n];
        want[n] = y1;
    }
    let y = run(&mut at(fc, RATE), &[x]).remove(0);
    assert_close_f64(&y, &want, 2.0f64.powi(-45));
}

/// A 1 Hz corner at 192 kHz, the blocker's range extreme, run in `f32`: the gain at the corner
/// is `|1 - e^{-jw}| / |1 - R e^{-jw}|` for `w = 2 pi fc / fs` and the pole `R = exp(-w)`.
///
/// The blocker stores `c = 1 - R = 3.3e-5`, rounded to `f32` by at most `2^-24` of itself; at the
/// corner `d ln |H| / d ln c = -1/2`, so the gain is off by at most `2^-25 = 3.0e-8`. Storing `R`
/// instead rounds by up to `2^-25` absolute, `9.1e-4` of `c` and `4.6e-4` of the gain (computed
/// below as `old`): the bound here is under a tenth of that.
///
/// Noise: the state `y` (below 1, so an ulp of `2^-24` at most `2^-23`) takes about three
/// roundings a step, each uniform within half an ulp, standard deviation `2^-24 / sqrt 12 =
/// 1.7e-8`, and leaks at `w` a frame, so they add as a random walk of `1 / (2 w) = 15 000`
/// frames: the output deviates by `1.7e-8 * sqrt(3 * 15 000) = 3.6e-6`. The bound is the
/// coefficient's `3.0e-8` and 7.4 of those, `2.7e-5`: the test measures `N = 192 000` frames,
/// whose expected maximum is `sqrt(2 ln N) = 4.9` standard deviations, and 1.5 times that allows
/// for the non-Gaussian rounding tails and the correlated state noise.
#[test]
fn a_one_hz_corner_at_192_khz_holds_its_gain_and_noise_in_f32() {
    use crate::testing::{sine_fit, steady_deviation};
    const FS: f64 = 192_000.0;
    let rate = SampleRate::of(FS);
    let whole = FS as usize;
    // The gain at the corner `corner`, from the closed form with the pole `r`.
    let closed = |corner: Hz, r: f64| {
        let w = std::f64::consts::TAU * corner.get() / FS;
        let z = Complex64::from_polar(1.0, -w);
        (Complex64::new(1.0, 0.0) - z) / (Complex64::new(1.0, 0.0) - z * r)
    };
    let omega = |corner: Hz| std::f64::consts::TAU * corner.get() / FS;
    let w = omega(MIN_DC_CORNER);
    let sigma = 2f64.powi(-24) / 12f64.sqrt() * (3.0 / (2.0 * w)).sqrt();
    let bound = 2f64.powi(-25) + 1.5 * (2.0 * (whole as f64).ln()).sqrt() * sigma;

    // The blocker at the 1 Hz corner.
    let h = closed(MIN_DC_CORNER, dc_pole(MIN_DC_CORNER, rate));
    let input = |corner: Hz| -> Vec<f32> {
        (0..4 * whole)
            .map(|n| libm::sin(omega(corner) * n as f64) as f32)
            .collect()
    };
    let x = input(MIN_DC_CORNER);
    let mut blocker = DcBlocker::<f32>::new(ChannelLayout::MONO, rate);
    blocker.retarget(&DcBlockerControls {
        corner: MIN_DC_CORNER,
    });
    blocker.jump_to_target();
    let mut out = vec![0.0f32; x.len()];
    blocker
        .process(
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let y: Vec<f64> = out.iter().map(|&v| f64::from(v)).collect();
    // The sine's phase is `h`'s: the fit is `a sin + b cos` with `H = a + j b`.
    let fit = sine_fit(&y, w, 3 * whole);
    assert!(
        (fit.norm() - h.norm()).abs() < bound,
        "gain {fit} against {h}"
    );
    let worst = steady_deviation(&y, w, 3 * whole, h);
    assert!(worst < bound, "deviation {worst:e} over {bound:e}");

    // The old recursion `y = x - x1 + R y1` with `R` stored in `f32`. Rounding `R` moves the
    // gain at the corner by up to `old = 4.6e-4`; the sweep of corners from 1 to 2 Hz finds the
    // one where the rounding is worst, and there the stored pole's measured gain error is past
    // three times the bound while the complement's is inside it.
    let old = 0.5 * 2f64.powi(-25) / (1.0 - dc_pole(MIN_DC_CORNER, rate));
    let worst_corner = (0..=100)
        .map(|i| Hz::new(1.0 + f64::from(i) * 0.01))
        .max_by(|&a, &b| {
            let off = |c: Hz| {
                let r = dc_pole(c, rate);
                (closed(c, f64::from(r as f32)).norm() - closed(c, r).norm()).abs()
            };
            off(a).total_cmp(&off(b))
        })
        .unwrap();
    let (r, w) = (dc_pole(worst_corner, rate), omega(worst_corner));
    let (r32, h) = (r as f32, closed(worst_corner, r));
    let x = input(worst_corner);
    let (mut x1, mut y1) = (0.0f32, 0.0f32);
    let stored: Vec<f64> = x
        .iter()
        .map(|&x| {
            let out = x - x1 + r32 * y1;
            (x1, y1) = (x, out);
            f64::from(out)
        })
        .collect();
    let stored_error = (sine_fit(&stored, w, 3 * whole).norm() - h.norm()).abs();
    let mut blocker = DcBlocker::<f32>::new(ChannelLayout::MONO, rate);
    blocker.retarget(&DcBlockerControls {
        corner: worst_corner,
    });
    blocker.jump_to_target();
    blocker
        .process(
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let y: Vec<f64> = out.iter().map(|&v| f64::from(v)).collect();
    let complement_error = (sine_fit(&y, w, 3 * whole).norm() - h.norm()).abs();
    assert!(
        stored_error > 3.0 * bound && stored_error <= old + bound,
        "stored pole {stored_error:e}, bound {bound:e}, worst case {old:e}"
    );
    assert!(complement_error < bound, "complement {complement_error:e}");
}
