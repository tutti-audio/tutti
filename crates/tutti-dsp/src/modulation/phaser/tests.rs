use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::impulse_at;
use tutti_units::Samples;
use tutti_units::{Feedback, Hz, Real, SampleRate};

use super::*;
use crate::{Complex64, allpass_complement};
use tutti_buffers::InputOutput;

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

const RATE: SampleRate = SampleRate::SR_48K;

/// A chain of `stages` on `coefficients.len()` channels, run over `audio` (one slice per
/// channel) in blocks of `block`.
/// A bus of `width` channels.
fn bus(width: usize) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::clamped(width)).expect("a test width is one bus")
}

fn run(
    stages: usize,
    coefficients: &mut [Ramp<f32>],
    feedback: &mut Ramp<f32>,
    audio: &[Vec<f32>],
    block: usize,
) -> Vec<Vec<f32>> {
    let mut chain = PhaserChain::new(Stages::of(stages), bus(coefficients.len()));
    run_on(&mut chain, coefficients, feedback, audio, block)
}

fn run_on(
    chain: &mut PhaserChain<f32>,
    coefficients: &mut [Ramp<f32>],
    feedback: &mut Ramp<f32>,
    audio: &[Vec<f32>],
    block: usize,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let ins: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        chain
            .process(
                coefficients,
                feedback,
                &mut InputOutput::paired(
                    Planes::new(&ins).unwrap(),
                    PlanesMut::new(&mut outs).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        at = end;
    }
    out
}

/// The coefficient of the textbook all-pass `(c + z^-1) / (1 + c z^-1)` tuned to `hz`:
/// `tan` of the normalised centre, folded into the all-pass form.
fn textbook_c(hz: f64, sr: f64) -> f64 {
    let t = (std::f64::consts::PI * hz / sr).tan();
    (t - 1.0) / (t + 1.0)
}

/// One stage's response at `hz`, from the textbook `H` on the unit circle.
fn stage(c: f64, hz: f64, sr: f64) -> Complex64 {
    let w = std::f64::consts::TAU * hz / sr;
    let zi = Complex64::from_polar(1.0, -w);
    (zi + c) / (Complex64::new(1.0, 0.0) + zi * c)
}

/// The closed form of a chain of `stages` equal stages with feedback `g`: `A / (1 - g z^-1 A)`,
/// `A` the `stages`-th power of a stage (the feedback reads the last frame's output).
fn textbook(stages: usize, c: f64, g: f64, hz: f64, sr: f64) -> Complex64 {
    let a = stage(c, hz, sr).powi(i32::try_from(stages).unwrap());
    let zi = Complex64::from_polar(1.0, -std::f64::consts::TAU * hz / sr);
    a / (Complex64::new(1.0, 0.0) - zi * a * g)
}

/// The response of an impulse response `h` at `hz`: its sum against `e^{-j w n}`.
fn dft(h: &[f32], hz: f64, sr: f64) -> Complex64 {
    let w = std::f64::consts::TAU * hz / sr;
    h.iter()
        .enumerate()
        .map(|(n, &x)| Complex64::from_polar(f64::from(x), -w * n as f64))
        .sum()
}

/// The complement `1 + c` of the coefficient `c` of a stage, the form a chain is given.
fn complement(c: f64) -> f32 {
    f32::from_design(1.0 + c)
}

/// The impulse response of a held chain of `stages` at the complement `a` of its coefficient
/// and feedback `g`, `len` frames long.
fn held_response(stages: usize, a: f32, g: Feedback, len: usize) -> Vec<f32> {
    run(
        stages,
        &mut [Ramp::new(a)],
        &mut Ramp::new(f32::from_unit(g)),
        &[impulse_at(Samples::new(len), Samples::new(0))],
        64,
    )
    .remove(0)
}

// ---- the figures of the definition ---------------------------------------

/// At no feedback the held chain is flat and its response is the `N`th power of a stage: the
/// DFT of its impulse response against the textbook transfer function, at coefficients solved
/// here from the centre.
#[test]
fn a_held_chain_is_flat_with_the_nth_power_of_a_stage_at_no_feedback() {
    let sr = RATE.get();
    for stages in [2usize, 3, 4, 7, 12] {
        let c = textbook_c(1_000.0, sr);
        let h = held_response(
            stages,
            allpass_complement(Hz::new(1_000.0), RATE),
            Feedback::NONE,
            8_192,
        );
        for hz in [30.0f64, 250.0, 1_000.0, 3_000.0, 11_000.0, 23_000.0] {
            let got = dft(&h, hz, sr);
            let want = textbook(stages, c, 0.0, hz, sr);
            assert!(
                (got.norm() - 1.0).abs() < 1e-4,
                "{stages} stages at {hz} Hz: |H| = {}",
                got.norm()
            );
            assert!(
                (got - want).norm() < 1e-3,
                "{stages} stages at {hz} Hz: H = {got}, want {want}"
            );
        }
    }
}

/// At the centre a stage is a quarter turn, so two stages invert a sine: the wet and the dry
/// of a half mix cancel there, `H(f0) = -1`, and the render of a sine at `f0` is the input
/// inverted.
#[test]
fn two_stages_invert_at_their_centre() {
    let sr = RATE.get();
    let c = allpass_complement(Hz::new(1_000.0), RATE);
    let h = held_response(2, c, Feedback::NONE, 8_192);
    let g = dft(&h, 1_000.0, sr);
    assert!((g + 1.0).norm() < 1e-3, "H(f0) = {g}");
    let x: Vec<f32> = (0..6_000)
        .map(|n| (std::f64::consts::TAU * 1_000.0 * n as f64 / sr).sin() as f32)
        .collect();
    let y = run(
        2,
        &mut [Ramp::new(c)],
        &mut Ramp::new(0.0),
        std::slice::from_ref(&x),
        64,
    )
    .remove(0);
    let worst = (5_000..6_000)
        .map(|n| (y[n] + x[n]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 2e-3, "wet is not the inverted input: {worst}");
}

/// The notches of a half mix are where the chain's phase is an odd multiple of `-pi`: a
/// chain of `N` stages crosses `-pi, -3 pi, ..` on the way from DC to Nyquist, so it has
/// `ceil(N / 2)` of them, an odd chain's last at Nyquist. Each is found here by bisection
/// on the textbook phase, and the chain's own response is `-1` there (wet and dry cancel).
#[test]
fn a_half_mix_of_the_chain_has_ceil_n_over_two_notches_where_the_phase_is_odd_pi() {
    let sr = RATE.get();
    for stages in 2usize..=12 {
        let c = textbook_c(700.0, sr);
        // The chain's phase, unwrapped from 0 at DC to `-N pi` at Nyquist: a stage's alone
        // is monotone, so the sum of `N` equal ones is too.
        let total = |hz: f64| {
            let mut p = stage(c, hz, sr).arg();
            if p > 1e-9 {
                p -= std::f64::consts::TAU;
            }
            stages as f64 * p
        };
        let mut notches = Vec::new();
        let mut k = 0;
        while 2 * k < stages {
            let target = -((2 * k + 1) as f64) * std::f64::consts::PI;
            let (mut lo, mut hi) = (0.0, sr / 2.0);
            for _ in 0..80 {
                let mid = 0.5 * (lo + hi);
                if total(mid) > target {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            notches.push(0.5 * (lo + hi));
            k += 1;
        }
        assert_eq!(notches.len(), stages.div_ceil(2), "{stages} stages");
        if stages % 2 == 1 {
            let last = *notches.last().unwrap();
            assert!((last - sr / 2.0).abs() < 1e-3, "{stages}: Nyquist, {last}");
        }
        let h = held_response(
            stages,
            allpass_complement(Hz::new(700.0), RATE),
            Feedback::NONE,
            8_192,
        );
        for hz in notches {
            let mix = (dft(&h, hz, sr) + 1.0) * 0.5;
            assert!(
                mix.norm() < 2e-3,
                "{stages} stages: the half mix at {hz} Hz is {}",
                mix.norm()
            );
        }
    }
}

/// The chain at its fixed points: `A = 1` at DC and `(-1)^N` at Nyquist for any
/// coefficient, so `|H| = 1 / (1 - g)` at DC and `1 / (1 + g (-1)^N)` at Nyquist, and the
/// response never exceeds the DC peak `1 / (1 - g)` (`|A| = 1` on the circle). The greatest
/// stable feedback is among the `g`: its response rings for about `D / (1 - g)` frames (`D`
/// the loop's group delay at DC), so its impulse response is taken long enough to have rung
/// out, and the figures are held to a relative tolerance.
#[test]
fn the_feedback_closes_the_loop_at_dc_and_nyquist_and_peaks_at_dc() {
    let sr = RATE.get();
    for stages in [2usize, 3, 6, 7] {
        for q in [0.0, 0.5, 0.9, Feedback::MAX_STABLE.get()] {
            let g = Feedback::new(q);
            let len = if q > 0.95 { 262_144 } else { 16_384 };
            let h = held_response(stages, allpass_complement(Hz::new(900.0), RATE), g, len);
            let nyquist = if stages % 2 == 0 {
                1.0 / (1.0 + q)
            } else {
                1.0 / (1.0 - q)
            };
            let dc_gain = 1.0 / (1.0 - q);
            let tol = 1e-3 * dc_gain.max(1.0);
            let (dc, ny) = (dft(&h, 0.0, sr), dft(&h, sr / 2.0, sr));
            assert!(
                (dc.norm() - dc_gain).abs() < tol,
                "{stages} stages, g {g}: DC {}",
                dc.norm()
            );
            assert!(
                (ny.norm() - nyquist).abs() < tol,
                "{stages} stages, g {g}: Nyquist {}",
                ny.norm()
            );
            let peak = (0..=400)
                .map(|k| dft(&h, f64::from(k) * 60.0, sr).norm())
                .fold(0.0, f64::max);
            assert!(peak <= dc_gain + tol, "{stages} stages, g {g}: peak {peak}");
        }
    }
}

/// The loop is stable at the greatest feedback: the 12-stage chain at `g = MAX_STABLE` and
/// a held coefficient of `c = +-0.9` has an impulse response that decays.
///
/// The loop is `g z^-1 A(z)` with `|A| = 1`, so it has a pole wherever the loop's phase is a
/// whole turn, and the pole at a frequency decays as `exp(-n (1 - g) / D)`, `D` the loop's
/// group delay there, `1 + N tau`. A stage's group delay `tau` is `(1 - c) / (1 + c)` at DC
/// and `(1 + c) / (1 - c)` at Nyquist, so for `c = +-0.9` it reaches 19 (at DC for
/// `c = -0.9`, at Nyquist for `c = +0.9`) and the slowest pole's time constant is
/// `D / (1 - g)` = 22,900 frames for both. The response is rendered for six time constants
/// (plus two windows), so the decay is `exp(-6)`, 2.5e-3, a factor 250 over the `f32`
/// roundoff floor (about 1e-5 for this loop), and its last window is held under a twentieth
/// of the peak over its first: a loop at `g >= 1` does not decay, and an unstable one grows.
#[test]
fn the_loop_decays_at_the_greatest_stable_feedback_with_the_time_constant_its_delay_gives() {
    const WINDOW: usize = 4_096;
    let g = Feedback::MAX_STABLE;
    let stages = 12usize;
    for c in [0.9f32, -0.9] {
        let tau = f64::from((1.0 + c.abs()) / (1.0 - c.abs()));
        let time_constant = (1.0 + stages as f64 * tau) / (1.0 - g.get());
        let len = (6.0 * time_constant) as usize + 2 * WINDOW;
        let h = held_response(stages, c, g, len);
        let peak = |w: &[f32]| w.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let (first, last) = (peak(&h[..WINDOW]), peak(&h[len - WINDOW..]));
        assert!(first > 0.5, "c {c}: the loop rings: {first}");
        assert!(
            last < first / 20.0,
            "c {c}: {last} left of {first} after {len} frames (time constant {time_constant})"
        );
    }
}

/// With feedback the response is the closed loop `A / (1 - g z^-1 A)` at every frequency:
/// the DFT of the impulse response against the textbook form.
#[test]
fn the_feedback_response_is_the_closed_loop_of_the_stages() {
    let sr = RATE.get();
    let (stages, g) = (4usize, Feedback::new(0.8));
    let c = textbook_c(2_000.0, sr);
    let h = held_response(
        stages,
        allpass_complement(Hz::new(2_000.0), RATE),
        g,
        16_384,
    );
    for hz in [0.0f64, 100.0, 1_500.0, 2_000.0, 5_000.0, 17_000.0, 24_000.0] {
        let got = dft(&h, hz, sr);
        let want = textbook(stages, c, g.get(), hz, sr);
        assert!((got - want).norm() < 2e-3, "{hz} Hz: {got}, want {want}");
    }
}

/// At a coefficient of 0 every stage is a one-frame delay, so a chain of `N` stages with
/// feedback `g` has the loop `N + 1` frames long: an impulse comes out at frame `N` and then
/// `g^k` at `N + k (N + 1)`, silence between (the loop's poles are the `N + 1` roots of
/// `g`, inside the circle for the greatest stable feedback and decaying as `g^k`).
#[test]
fn feedback_rings_as_a_train_of_the_loop_length() {
    for (stages, g) in [
        (2usize, Feedback::new(0.6)),
        (5, Feedback::new(0.9)),
        (12, Feedback::MAX_STABLE),
    ] {
        let y = held_response(stages, complement(0.0), g, 80);
        for (i, &s) in y.iter().enumerate() {
            let want = if i >= stages && (i - stages) % (stages + 1) == 0 {
                g.get()
                    .powi(i32::try_from((i - stages) / (stages + 1)).unwrap())
            } else {
                0.0
            };
            // The samples are `f32`, which sets the tolerance.
            assert!(
                (f64::from(s) - want).abs() < 1e-6,
                "{stages} stages, frame {i}: {s}"
            );
        }
    }
}

// ---- the ramps ------------------------------------------------------------------

/// A coefficient ramp is read at the step after the ones taken: an impulse at frame `n`
/// of a fresh two-stage chain with no feedback comes out as the square of that frame's
/// coefficient (`out = c (in - y1) + x1` with no history, the ramp running on `1 + c`), which
/// is the ramp's step `n + 1`
/// (`from + (to - from) (n + 1) / 16`, and `to` from the sixteenth step on), whatever the blocks.
#[test]
fn a_coefficient_ramp_is_read_a_step_ahead_and_lands_exactly() {
    let (from, to) = (0.2f32, -0.5f32);
    for n in [0usize, 1, 7, 14, 15, 16, 17, 31] {
        for block in [1, 5, 64] {
            let mut ramp = Ramp::new(complement(f64::from(from)));
            ramp.retarget_in(complement(f64::from(to)), 16);
            let y = run(
                2,
                &mut [ramp],
                &mut Ramp::new(0.0),
                &[impulse_at(Samples::new(n + 1), Samples::new(n))],
                block,
            )
            .remove(0);
            let k = u32::try_from(n + 1).unwrap().min(16);
            let c = from + (to - from) * (k as f32 / 16.0);
            assert!(
                (y[n] - c * c).abs() < 1e-6,
                "frame {n}, blocks of {block}: {} against {}",
                y[n],
                c * c
            );
        }
    }
}

/// The feedback ramps on the stream and lands exactly: at a coefficient of 0 and two stages
/// the first echo of an impulse at frame 0 leaves at frame 5 with the feedback of frame 3
/// (the loop reads it three frames after the output), the ramp's step 4 of 240 from 0.2 to
/// 0.8, and one past the ramp it is 0.8.
#[test]
fn a_feedback_ramp_is_read_a_step_ahead_and_lands_exactly() {
    for (steps, want) in [(240u32, 0.2 + 0.6 * 4.0 / 240.0), (2, 0.8)] {
        let mut fb = Ramp::new(0.2f32);
        fb.retarget_in(0.8, steps);
        let y = run(
            2,
            &mut [Ramp::new(complement(0.0))],
            &mut fb,
            &[impulse_at(Samples::new(8), Samples::new(0))],
            3,
        )
        .remove(0);
        assert!(
            (f64::from(y[5]) - want).abs() < 1e-6,
            "{steps} steps: {}, want {want}",
            y[5]
        );
    }
}

// ---- the channels -------------------------------------------------------------

/// A silent channel stays silent, exactly 0.0, whatever its neighbours carry (full-scale
/// noise, the most feedback): each channel's all-pass state is its own, and the feedback is
/// its own output's. And channels at one coefficient and fed one signal give one output.
#[test]
fn a_silent_channel_stays_silent_and_each_channel_keeps_its_own_state() {
    let noise = |seed: usize| -> Vec<f32> {
        (0..4_096)
            .map(|n| (((n + seed) * 2_654_435_761usize) % 1_000) as f32 / 500.0 - 1.0)
            .collect()
    };
    let x = noise(5);
    let audio = [
        x.clone(),
        x.clone(),
        x.clone(),
        vec![0.0f32; 4_096],
        x.clone(),
        noise(6),
    ];
    let a = allpass_complement(Hz::new(1_000.0), RATE);
    let b = allpass_complement(Hz::new(2_500.0), RATE);
    let mut coefficients = [a, b, a, a, a, a].map(Ramp::new);
    let out = run(6, &mut coefficients, &mut Ramp::new(0.99), &audio, 64);
    assert!(out[3].iter().all(|&s| s == 0.0), "no bleed into a silence");
    assert_eq!(out[4], out[0], "one coefficient, one signal, one output");
    assert_eq!(out[2], out[0]);
    assert_ne!(out[1], out[0], "another coefficient sounds otherwise");
    assert!(out[5].iter().any(|&s| s != 0.0));
}

/// Reset is silence: after a loud stretch and a reset, silence in is exactly silence out.
#[test]
fn reset_empties_the_chain() {
    let c = allpass_complement(Hz::new(1_000.0), RATE);
    let mut chain = PhaserChain::new(Stages::of(6), bus(2));
    let mut coefficients = [Ramp::new(c); 2];
    let mut fb = Ramp::new(0.9);
    let loud = vec![vec![1.0f32; 300]; 2];
    let y = run_on(&mut chain, &mut coefficients, &mut fb, &loud, 64);
    assert!(y[0].iter().any(|&s| s != 0.0));
    chain.reset();
    let y = run_on(
        &mut chain,
        &mut coefficients,
        &mut fb,
        &vec![vec![0.0f32; 300]; 2],
        64,
    );
    assert!(y.iter().flatten().all(|&s| s == 0.0));
}

/// A clone is the chain with its history: it carries on where the original is.
#[test]
fn a_clone_continues_where_the_original_is() {
    let c = allpass_complement(Hz::new(1_000.0), RATE);
    let mut chain = PhaserChain::new(Stages::of(4), bus(1));
    let mut coefficients = [Ramp::new(c)];
    let mut fb = Ramp::new(0.5);
    run_on(
        &mut chain,
        &mut coefficients,
        &mut fb,
        &[impulse_at(Samples::new(64), Samples::new(0))],
        64,
    );
    let mut copy = chain.clone();
    let silence = [vec![0.0f32; 64]];
    let a = run_on(&mut chain, &mut coefficients, &mut fb, &silence, 64);
    let b = run_on(&mut copy, &mut coefficients, &mut fb, &silence, 64);
    assert_eq!(a, b);
    assert!(a[0].iter().any(|&s| s != 0.0));
}

// ---- the checked constants --------------------------------------------------
//
/// `Stages` keeps the range-checked shape: `new` refuses a count outside
/// `2..=12` and never clamps.
#[test]
fn stages_keep_the_range_checked_shape() {
    let c = tutti_fixtures::RangeContract {
        new: Stages::new,
        get: Stages::get,
        of: Some(Stages::of),
        valid: &[2, 3, 7, 11, 12],
        invalid: &[0, 1, 13, 20, usize::MAX],
    };
    tutti_fixtures::check_range_contract(&c);
    assert_eq!(Stages::try_from(13), Err(StagesError(13)));
    assert_eq!((Stages::MIN, Stages::MAX), (2, 12));
}

/// A [`SweepRange`] refuses what a sweep cannot hold and never clamps: a bound that is not
/// finite, a bottom under 20 Hz and a bottom above the top; the edge of each rule is in. The
/// 0.90-of-Nyquist cap is not here: it depends on the rate, and is the member's.
#[test]
fn a_sweep_range_refuses_what_a_sweep_cannot_hold() {
    let ok = |lo: f64, hi: f64| SweepRange::try_from(Hz::new(lo)..=Hz::new(hi));
    assert!(ok(20.0, 20.0).is_ok(), "the floor, a range of one value");
    assert!(ok(200.0, 4_000.0).is_ok());
    assert!(ok(20.0, 1e9).is_ok(), "the top is the rate's to cap");
    assert_eq!(
        ok(19.999, 4_000.0),
        Err(SweepRangeError::BelowFloor(Hz::new(19.999)))
    );
    assert_eq!(
        ok(-5.0, 100.0),
        Err(SweepRangeError::BelowFloor(Hz::new(-5.0)))
    );
    assert_eq!(
        ok(300.0, 299.0),
        Err(SweepRangeError::Order {
            min: Hz::new(300.0),
            max: Hz::new(299.0)
        })
    );
    for (lo, hi) in [
        (f64::NAN, 100.0),
        (100.0, f64::NAN),
        (100.0, f64::INFINITY),
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, 100.0),
    ] {
        assert!(
            matches!(ok(lo, hi), Err(SweepRangeError::NotFinite { .. })),
            "{lo}..{hi}"
        );
    }
    let r = SweepRange::DEFAULT;
    assert_eq!((r.min(), r.max()), (Hz::new(200.0), Hz::new(4_000.0)));
    assert_eq!(SweepRange::default(), SweepRange::DEFAULT);
}

/// `of` panics where `new` refuses, the constant's spelling.
#[test]
#[should_panic(expected = "a sweep range is finite")]
fn a_sweep_range_of_a_transposed_pair_panics_where_it_is_a_constant() {
    let _ = SweepRange::of(Hz::new(4_000.0), Hz::new(200.0));
}

/// An `f64` chain keeps its state and its coefficient in `f64`: with the complement `a` held
/// (a design in `f64`, which an `f32` cannot hold) and `c = a - 1`, three stages in cascade with
/// feedback `g` are, per frame, `u[n] = x[n] + g y[n - 1]` through `s[n] = c (u[n] - s[n - 1])
/// + u[n - 1]` three times, the stage's own difference equation computed in `f64`, to 2^-45.
#[test]
fn an_f64_chain_is_the_all_pass_difference_equation_in_f64() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let (stages, len, g) = (3usize, 300usize, 0.5f64);
    let a: f64 = allpass_complement(Hz::new(1_000.0), RATE);
    let c = a - 1.0;
    assert_ne!(
        a,
        f64::from(a as f32),
        "the coefficient needs more than f32 holds"
    );
    let x = wide_signal(len);
    let mut want = vec![0.0f64; len];
    let mut state = vec![(0.0f64, 0.0f64); stages];
    let mut last = 0.0;
    for n in 0..len {
        let mut u = x[n] + g * last;
        for (x1, y1) in &mut state {
            let s = c * (u - *y1) + *x1;
            (*x1, *y1) = (u, s);
            u = s;
        }
        last = u;
        want[n] = u;
    }
    let mut chain = PhaserChain::<f64>::new(Stages::of(stages), bus(1));
    let mut coefficients = [Ramp::new(a)];
    let mut feedback = Ramp::new(g);
    let mut y = vec![0.0f64; len];
    chain
        .process(
            &mut coefficients,
            &mut feedback,
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut y[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_close_f64(&y, &want, 2.0f64.powi(-45));
}

/// A held two-stage `f32` chain at its lowest sweeps, 20 Hz ([`SweepRange::FLOOR`]) and up to
/// 40 Hz, at 192 kHz inverts a sine at the centre: a stage is a quarter turn there, so `H(f0)
/// = -1`, and the steady state is `y = -x`. The centre is within `2 t` of -1 in the coefficient
/// (`t = tan(pi f0 / 192000) = 3.3e-4` at 20 Hz, `a = 2 t / (1 + t)` = 6.5e-4), so an `f32`
/// coefficient `c` rounds to `2^-25` of 6.5e-4, a relative 4.6e-5 of the pole offset at worst.
/// The chain holds the complement `a`, designed in `f64`, and the stage `y1 + ((x1 - x) + a (x -
/// y1))` adds its small terms first.
///
/// The bound is the roundoff noise: each stage's final sum rounds to within half an ulp of a
/// value under 1, at most `2^-25` (the ulp of `[0.5, 1)` is `2^-24`). The bound takes the error
/// uniform on `+-2^-24`, a full ulp either way, so it is conservative by a factor of 2, with
/// deviation `sigma_r = 2^-24 / sqrt(3)`; the pole at `1 - a` carries that into the state with
/// the gain `1 / (a (2 - a))` in variance, and the two stages add. The worst of `N = 19 200`
/// frames is `1.5 sqrt(2 ln N)` deviations (`sqrt(2 ln N)` is the Gaussian maximum of `N`
/// samples, and 1.5 covers the noise being coloured by the pole, whose samples are correlated
/// and not Gaussian), plus `4 * 2^-24` for the coefficient and the input's
/// own rounding. The old stored form `c`, swept over the same centres, is over twice the
/// largest bound at its worst (measured 6.4e-5 against a bound of 9.2e-6; the chain's own worst
/// is 2.3e-6).
#[test]
fn an_f32_chain_at_its_lowest_sweeps_at_192_khz_inverts_a_sine_at_the_centre() {
    let rate = SampleRate::SR_192K;
    let (settle, n) = (32_000, 19_200);
    let (mut worst_new, mut worst_old, mut worst_bound) = (0.0f64, 0.0f64, 0.0f64);
    for k in 0..12 {
        let f0 = Hz::new(SweepRange::FLOOR.get() * (1.0 + f64::from(k) / 11.0));
        let x: Vec<f32> = (0..settle + n)
            .map(|i| {
                f32::from_design(libm::sin(
                    std::f64::consts::TAU * f0.get() * i as f64 / rate.get(),
                ))
            })
            .collect();
        let a: f32 = allpass_complement(f0, rate);
        let y = run(
            2,
            &mut [Ramp::new(a)],
            &mut Ramp::new(0.0),
            std::slice::from_ref(&x),
            512,
        )
        .remove(0);
        let new = (settle..settle + n)
            .map(|i| f64::from(y[i] + x[i]).abs())
            .fold(0.0, f64::max);

        let a64 = f64::from(a);
        let sigma_r = 2f64.powi(-24) / 3f64.sqrt();
        let sigma = (2.0 * sigma_r * sigma_r / (a64 * (2.0 - a64))).sqrt();
        let bound = 1.5 * (2.0 * (n as f64).ln()).sqrt() * sigma + 4.0 * 2f64.powi(-24);
        assert!(
            new <= bound,
            "{f0:?}: the chain's worst {new} over its bound {bound}"
        );

        // The textbook form: the coefficient `c = (t - 1) / (t + 1)` stored in `f32`, the stage
        // `c (x - y1) + x1`.
        let t = libm::tan(std::f64::consts::PI * f0.get() / rate.get());
        let c = ((t - 1.0) / (t + 1.0)) as f32;
        let mut state = [(0.0f32, 0.0f32); 2];
        let old = x
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                let mut v = x;
                for (x1, y1) in &mut state {
                    let out = c * (v - *y1) + *x1;
                    (*x1, *y1) = (v, out);
                    v = out;
                }
                (i, f64::from(v + x).abs())
            })
            .filter(|&(i, _)| i >= settle)
            .fold(0.0, |w, (_, e)| w.max(e));
        (worst_new, worst_old, worst_bound) = (
            worst_new.max(new),
            worst_old.max(old),
            worst_bound.max(bound),
        );
    }
    assert!(
        worst_old > 2.0 * worst_bound,
        "the stored coefficient's worst error {worst_old} against the bound {worst_bound} (chain {worst_new})"
    );
}

/// A chain takes a bus's widest width, and a bus is no wider: a chain at
/// `MAX_PHASER_CHANNELS` is built and a bus past it is refused by its type.
#[test]
fn the_widest_chain_is_a_bus_and_a_wider_bus_is_refused() {
    assert!(
        ChannelTopology::default_for(ChannelLayout::clamped(MAX_PHASER_CHANNELS + 1)).is_none()
    );
    let c = allpass_complement(Hz::new(1_000.0), RATE);
    let mut chain = PhaserChain::new(Stages::of(2), bus(MAX_PHASER_CHANNELS));
    let mut coefficients = [Ramp::new(c); MAX_PHASER_CHANNELS];
    let mut fb = Ramp::new(0.0);
    let x = vec![vec![1.0f32; 8]; MAX_PHASER_CHANNELS];
    let y = run_on(&mut chain, &mut coefficients, &mut fb, &x, 8);
    assert_eq!(y.len(), MAX_PHASER_CHANNELS);
}
