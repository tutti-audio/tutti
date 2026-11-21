use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Hz, Q, SampleRate, Seconds, Semitones};

use super::*;
use crate::testing::amplitude_at;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
const TONE: f64 = 1_000.0;

/// A sine of `hz` and `amplitude`, `frames` long, from phase 0.
fn sine(hz: f64, amplitude: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            (amplitude * (2.0 * core::f64::consts::PI * hz * n as f64 / RATE.get()).sin()) as f32
        })
        .collect()
}

/// A bank of `ty` at `RATE` set to `controls`.
fn bank(ty: SvfType, controls: &EnvelopeFilterControls) -> EnvelopeFilterBank<f32> {
    let mut bank = EnvelopeFilterBank::<f32>::new(ChannelLayout::MONO, ty, RATE);
    bank.retarget(controls);
    bank.jump_to_target();
    bank
}

/// `x` through `bank` in calls of at most `block` frames.
fn run(bank: &mut EnvelopeFilterBank<f32>, x: &[f32], block: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; x.len()];
    for (xs, ys) in x.chunks(block).zip(out.chunks_mut(block)) {
        bank.process(
            &mut InputOutput::paired(
                Planes::new(&[xs]).unwrap(),
                PlanesMut::new(&mut [ys]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    }
    out
}

/// The magnitude of the SVF `ty` (low-pass or band-pass) at `cutoff` and `q`, at `hz`, from
/// the analog prototype under the bilinear map: with `w = tan(pi hz / fs) / tan(pi cutoff / fs)`,
/// the low-pass is `1 / hypot(1 - w^2, w / q)` and the band-pass (peak gain `q`) is `w` over
/// the same denominator.
fn magnitude(ty: SvfType, cutoff: f64, q: Q, hz: f64) -> f64 {
    let pi = core::f64::consts::PI;
    let w = (pi * hz / RATE.get()).tan() / (pi * cutoff / RATE.get()).tan();
    let denominator = (1.0 - w * w).hypot(w / q.get());
    match ty {
        SvfType::LowPass => 1.0 / denominator,
        SvfType::BandPass => w / denominator,
        other => unreachable!("{other:?} has no closed form here"),
    }
}

/// The gain of the settled tail of `y` against `x` at `TONE`: the last whole periods.
fn measured_gain(x: &[f32], y: &[f32]) -> f64 {
    let tail = 4_800; // 100 periods of 1 kHz at 48 kHz
    let from = x.len() - tail;
    amplitude_at(&y[from..], TONE, RATE.get()) / amplitude_at(&x[from..], TONE, RATE.get())
}

fn controls(base: f64, depth: f64, q: f64) -> EnvelopeFilterControls {
    EnvelopeFilterControls {
        base: Hz::new(base),
        depth: Semitones::new(depth),
        q: Q::new(q),
        attack: Seconds::new(0.05),
        release: Seconds::new(0.05),
    }
}

/// A steady sine of amplitude `A` has the mean magnitude `2A/pi`, so the cutoff settles at
/// `base * 2^(depth/12 * 2A/pi)` and the tone's gain is the SVF's analytic magnitude there.
///
/// Equal attack and release make the follower a linear one-pole on the rectified sine, so its
/// mean is `2A/pi` exactly (unequal times would settle between the mean and the peak). The
/// rectified fundamental (`4A / 3 pi`, at twice the tone) is attenuated by
/// `1 / (2 pi 2000 0.05) = 1.6e-3`, a ripple of about 1e-3 of `env`; at depth up to 2 octaves
/// that moves the cutoff by under 0.14 percent and the gain by under 0.3 percent, so the
/// tolerance is 3e-3 of the gain (relative, since the gains run from 0.1 to 2).
#[test]
fn the_settled_cutoff_is_base_times_two_to_depth_times_the_mean_level() {
    for ty in [SvfType::LowPass, SvfType::BandPass] {
        for (base, depth, amplitude) in [
            (500.0, 24.0, 0.5),
            (500.0, 24.0, 1.0),
            (900.0, -12.0, 0.8),
            (400.0, 12.0, 0.25),
        ] {
            let q = Q::new(2.0);
            let mut b = bank(ty, &controls(base, depth, q.get()));
            let x = sine(TONE, amplitude, 96_000);
            let y = run(&mut b, &x, 480);
            let env = 2.0 * amplitude / core::f64::consts::PI;
            let cutoff = base * (depth / 12.0 * env).exp2();
            let want = magnitude(ty, cutoff, q, TONE);
            let got = measured_gain(&x, &y);
            assert!(
                (got - want).abs() < 3e-3 * want,
                "{ty:?} base {base} depth {depth} A {amplitude}: gain {got} vs {want} at {cutoff} Hz"
            );
            // The cutoff moved: the gain is not the fixed filter's.
            let fixed = magnitude(ty, base, q, TONE);
            assert!(
                (want - fixed).abs() > 0.02,
                "{ty:?}: the case moves the gain"
            );
        }
    }
}

/// At depth 0 the cutoff stays at `base` whatever the level: the gain is the fixed SVF's
/// analytic magnitude at two amplitudes.
#[test]
fn zero_depth_is_a_fixed_svf_at_base() {
    let q = Q::new(2.0);
    for amplitude in [0.1, 0.9] {
        let mut b = bank(SvfType::LowPass, &controls(700.0, 0.0, q.get()));
        let x = sine(TONE, amplitude, 48_000);
        let y = run(&mut b, &x, 333);
        let want = magnitude(SvfType::LowPass, 700.0, q, TONE);
        let got = measured_gain(&x, &y);
        assert!((got - want).abs() < 1e-3, "A {amplitude}: {got} vs {want}");
    }
}

/// The grid is the stream's: any cut of a stream into blocks renders the same samples.
#[test]
fn the_render_does_not_depend_on_where_blocks_fall() {
    let x = sine(TONE, 0.7, 6_000);
    let reference = run(
        &mut bank(SvfType::BandPass, &controls(600.0, 24.0, 4.0)),
        &x,
        6_000,
    );
    for block in [1, 7, 16, 33, 480] {
        let y = run(
            &mut bank(SvfType::BandPass, &controls(600.0, 24.0, 4.0)),
            &x,
            block,
        );
        assert_eq!(reference, y, "blocks of {block}");
    }
}

/// A depth, a base or a Q that is not a number is the default's, and a depth is held within
/// ten octaves: the cutoff stays finite and inside the SVF's range.
#[test]
fn the_controls_are_held_in_range() {
    let mut b = bank(
        SvfType::LowPass,
        &controls(f64::NAN, f64::INFINITY, f64::NAN),
    );
    let y = run(&mut b, &sine(TONE, 0.5, 2_000), 128);
    assert!(y.iter().all(|s| s.is_finite()));
    assert_eq!(b.cutoff_at(1.0), EnvelopeFilterControls::DEFAULT.base);
    // Ten octaves above 1 kHz is past the Nyquist: the cutoff stops just short of it.
    let wide = bank(SvfType::LowPass, &controls(1_000.0, 1.0e9, 0.707));
    assert_eq!(
        wide.cutoff_at(1.0),
        RATE.nyquist_scaled(crate::filter::NYQUIST_MARGIN)
    );
    assert_eq!(wide.cutoff_at(0.0), Hz::new(1_000.0));
}

/// A tone over full scale reads as 1 in the envelope: `min(|x|, 1)` of a sine of amplitude 2
/// has the mean `(4 (1 - sqrt(3)/2) + 2 pi / 3) / pi` (the sine is clipped for the third of each
/// half-cycle above `|sin| = 1/2`), 0.8372, where an unclamped level would give `4 / pi`. The
/// gain is the SVF's analytic magnitude at `base * 2^(depth/12 * 0.8372)`, tolerance as the
/// settled test's.
#[test]
fn a_level_over_full_scale_reads_as_one() {
    let q = Q::new(2.0);
    let sqrt3 = 3.0f64.sqrt();
    let env =
        (4.0 * (1.0 - sqrt3 / 2.0) + 2.0 * core::f64::consts::PI / 3.0) / core::f64::consts::PI;
    let mut b = bank(SvfType::LowPass, &controls(500.0, 24.0, q.get()));
    let x = sine(TONE, 2.0, 96_000);
    let y = run(&mut b, &x, 480);
    let cutoff = 500.0 * (24.0 / 12.0 * env).exp2();
    let want = magnitude(SvfType::LowPass, cutoff, q, TONE);
    let unclamped = magnitude(
        SvfType::LowPass,
        500.0 * (24.0 / 12.0 * 4.0 / core::f64::consts::PI).exp2(),
        q,
        TONE,
    );
    let got = measured_gain(&x, &y);
    assert!((got - want).abs() < 3e-3 * want, "gain {got} vs {want}");
    assert!(
        (want - unclamped).abs() > 0.05 * want,
        "the clamp moves the gain"
    );
}

/// A square wave of `level` has a constant magnitude `level` and a fundamental of
/// `4 level / pi`; frame `n` is read at its midpoint so no sample is 0.
fn square(levels: &[(usize, f64)]) -> Vec<f32> {
    let mut x = Vec::new();
    for &(frames, level) in levels {
        for _ in 0..frames {
            let n = x.len();
            let phase = 2.0 * core::f64::consts::PI * TONE * (n as f64 + 0.5) / RATE.get();
            x.push((level * phase.sin().signum()) as f32);
        }
    }
    x
}

/// The attack and the release are told apart. A square wave has a constant magnitude `L`, so the
/// envelope is `L (1 - a_attack^n)` after a step up and `L2 + (L - L2) a_release^n` after a
/// step down, `a = exp(-1 / (tau rate))`; the fundamental's gain at a window is the SVF's
/// closed-form magnitude at the cutoff of that envelope. With attack 5 ms and release 200 ms the
/// two windows (13 attack times after the rise, and 100 ms after the fall) differ by 30 to 40
/// percent from what swapped times, or swapped branches in the follower, would give.
///
/// Tolerance 2e-2 of the gain: the window is 10 periods of a gain that moves with `exp(-n / 9600)`
/// (so the oracle averages the gain over the window, the design lagging the envelope by half a
/// 16-frame period on average) and the lock-in of the fundamental sees the harmonics' leakage
/// only through the ramp.
#[test]
fn the_attack_and_the_release_are_told_apart() {
    let (attack, release) = (Seconds::new(0.005), Seconds::new(0.2));
    let (high, low) = (0.8, 0.2);
    let (rise, fall) = (14_400, 9_600);
    let pole = |tau: Seconds| (-1.0 / (tau.get() * RATE.get())).exp();
    let q = Q::new(2.0);
    for ty in [SvfType::LowPass, SvfType::BandPass] {
        let (base, depth) = (400.0, 36.0);
        let mut b = bank(
            ty,
            &EnvelopeFilterControls {
                base: Hz::new(base),
                depth: Semitones::new(depth),
                q,
                attack,
                release,
            },
        );
        let x = square(&[(rise, high), (fall, low)]);
        let y = run(&mut b, &x, 480);
        let cutoff = |env: f64| base * (depth / 12.0 * env).exp2();
        let gain = |from: usize| {
            amplitude_at(&y[from..from + 480], TONE, RATE.get())
                / amplitude_at(&x[from..from + 480], TONE, RATE.get())
        };
        // Up: the window centred 65 ms after the step.
        let up = 2_880;
        let want_up = (up..up + 480)
            .step_by(16)
            .map(|i| {
                let env = high * (1.0 - pole(attack).powi(i as i32 - 7));
                magnitude(ty, cutoff(env), q, TONE)
            })
            .sum::<f64>()
            / 30.0;
        let got_up = gain(up);
        assert!(
            (got_up - want_up).abs() < 2e-2 * want_up,
            "{ty:?} up: {got_up} vs {want_up}"
        );
        // Down: the window centred 100 ms after the fall.
        let down = rise + 4_560;
        let want_down = (0..480)
            .step_by(16)
            .map(|k| {
                let n = 4_560 + k - 7;
                let env = low + (high - low) * pole(release).powi(n);
                magnitude(ty, cutoff(env), q, TONE)
            })
            .sum::<f64>()
            / 30.0;
        let got_down = gain(down);
        assert!(
            (got_down - want_down).abs() < 2e-2 * want_down,
            "{ty:?} down: {got_down} vs {want_down}"
        );
    }
}

/// At depth 0 the cutoff never moves, so an `f64` bank is a fixed state-variable filter at the
/// base: on a signal of more than 24 bits its output is the RBJ cookbook biquad of the same
/// response, run in `f64` direct form I from rest, frame for frame to 1e-12.
#[test]
fn an_f64_bank_at_depth_zero_is_the_cookbook_filter_at_its_base() {
    use crate::filter::svf::model::tests::rbj;
    use crate::testing::direct_form;
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(4_800);
    for ty in [SvfType::LowPass, SvfType::BandPass] {
        let mut b = EnvelopeFilterBank::<f64>::new(ChannelLayout::MONO, ty, RATE);
        b.retarget(&controls(1_000.0, 0.0, 0.707));
        b.jump_to_target();
        let mut out = vec![0.0f64; x.len()];
        b.process(
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let want = direct_form(&rbj(ty, 1_000.0, 0.707, 0.0, RATE.get()), &x);
        assert_close_f64(&out, &want, 1e-12);
    }
}
