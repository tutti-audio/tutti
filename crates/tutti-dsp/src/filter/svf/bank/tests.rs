use std::f64::consts::PI;

use crate::Complex64;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Db, Hz, Q, SampleRate};

use super::*;
use crate::design::prewarp;
use crate::filter::svf::model::tests::{TYPES, rbj};
use crate::testing::{sine_fit, steady_deviation};
use crate::{Lerp, SvfControls, SvfDesign, SvfModel, SvfType};
use tutti_buffers::InputOutput;

const RATE: SampleRate = SampleRate::SR_48K;

fn coefficients(ty: SvfType, fc: f64, q: f64, gain: f64) -> SvfCoefficients<f64> {
    let controls = SvfControls {
        ty,
        freq: Hz::new(fc),
        q: Q::new(q),
        gain: Db::new(gain),
    };
    SvfCoefficients::from(&SvfModel::new(controls, RATE))
}

/// Runs `input` (one `Vec` per channel) through `bank` in blocks of the lengths in
/// `blocks`, cycled, and returns the output per channel.
fn run(
    bank: &mut SvfBank<f64>,
    ramp: &mut Ramp<SvfCoefficients<f64>>,
    input: &[Vec<f64>],
    blocks: &[usize],
) -> Vec<Vec<f64>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0f64; frames]; input.len()];
    let mut at = 0;
    for &len in blocks.iter().cycle() {
        if at >= frames {
            break;
        }
        let end = (at + len).min(frames);
        let ins: Vec<&[f64]> = input.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            ramp,
            &mut InputOutput::paired(
                Planes::new(&ins).expect("equal planes"),
                PlanesMut::new(&mut outs).expect("equal planes"),
            )
            .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// The held bank's response is the RBJ cookbook's closed form: the DFT of its impulse
/// response, long enough to have decayed below 1e-12, matches the cookbook biquad of the
/// same controls at every probe frequency. A step makes at most 16 roundings of `2^-53` of a
/// state (at most `Σ|y|`) and an error reaches the DFT through the response, again `Σ|y|`:
/// the independent errors add as a random walk, which `16 * 2^-53 * (Σ|y|)^2` exceeds. The
/// cookbook's own coefficients and polynomials carry 8 roundings of `2^-53` of their terms, over
/// the denominator `|D(e^jw)|` that a low cutoff and a high Q make small: that figure is added.
#[test]
fn the_held_response_is_the_rbj_closed_form() {
    const N: usize = 1 << 15;
    for ty in TYPES {
        for (fc, q, gain) in [(1_000.0f64, 0.707f64, 9.0f64), (250.0, 4.0, -12.0)] {
            let mut bank = SvfBank::new(ChannelLayout::MONO);
            let mut ramp = Ramp::new(coefficients(ty, fc, q, gain));
            let mut x = vec![0.0f64; N];
            x[0] = 1.0;
            let y = run(&mut bank, &mut ramp, &[x], &[N]).remove(0);
            let l1 = y.iter().map(|v| v.abs()).sum::<f64>();
            let rounding = 16.0 * 2f64.powi(-53) * l1 * l1;
            let want = rbj(ty, fc, q, gain, RATE.get());
            for f in [50.0, fc, 3_000.0, 15_000.0] {
                let w = 2.0 * PI * f / RATE.get();
                let got: Complex64 = y
                    .iter()
                    .enumerate()
                    .map(|(n, &v)| Complex64::from_polar(v, -w * n as f64))
                    .sum();
                let h = want.response(w);
                let z = Complex64::from_polar(1.0, -w);
                let d = Complex64::new(1.0, 0.0) + z * want.a1 + z * z * want.a2;
                let terms = want.b0.abs() + want.b1.abs() + want.b2.abs();
                let oracle = 8.0
                    * 2f64.powi(-53)
                    * (terms + h.norm() * (1.0 + want.a1.abs() + want.a2.abs()))
                    / d.norm();
                assert!(
                    (got - h).norm() <= rounding + oracle,
                    "{ty:?} fc {fc} Q {q} {gain} dB at {f} Hz: {got} against {h}"
                );
            }
        }
    }
}

/// Block independence: a ramp in flight, rendered in uneven blocks, gives the same
/// frames bit for bit as one block, because frame `i` reads step `taken + 1 + i` however
/// the frames are cut.
#[test]
fn any_split_renders_the_frames_of_one_block() {
    let input: Vec<Vec<f64>> = (0..3)
        .map(|c| {
            (0..997)
                .map(|i| ((i * 7 + c * 13) % 101) as f64 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |blocks: &[usize]| {
        let mut bank = SvfBank::new(ChannelLayout::from(3u16));
        let mut ramp = Ramp::new(coefficients(SvfType::LowPass, 300.0, 0.707, 0.0)).steps(16);
        ramp.retarget(coefficients(SvfType::LowPass, 6_000.0, 2.0, 0.0));
        run(&mut bank, &mut ramp, &input, blocks)
    };
    let whole = render(&[997]);
    for blocks in [&[1usize][..], &[5, 3, 64], &[15, 1, 1, 200], &[16]] {
        assert_eq!(render(blocks), whole, "blocks {blocks:?}");
    }
}

/// A blended block lands on the new set at its last step: an impulse on the ramp's 16th
/// frame, through a bank with no state, comes out as the new set's `a3` (with no state a
/// low-pass's output is `a3 x`), and the ramp has no steps left.
#[test]
fn a_ramp_arrives_on_the_new_set_at_its_last_frame() {
    let new = coefficients(SvfType::LowPass, 5_000.0, 0.707, 0.0);
    let mut blended = Ramp::new(coefficients(SvfType::LowPass, 500.0, 0.707, 0.0)).steps(16);
    blended.retarget(new);
    let mut x = vec![0.0f64; 16];
    x[15] = 1.0;
    let mut bank = SvfBank::new(ChannelLayout::MONO);
    let y = run(&mut bank, &mut blended, &[x], &[16]).remove(0);
    assert_eq!(y[15].to_bits(), new.a3.to_bits());
    assert!(y[..15].iter().all(|&v| v == 0.0));
    assert!(!blended.moving());
}

/// Channels are independent: a bank of any width gives each channel exactly what a
/// one-channel bank gives it, held or ramping, whatever group the width falls into.
#[test]
fn each_channel_is_its_own_filter() {
    for width in [1u16, 2, 5, 8, 9, 16] {
        let input: Vec<Vec<f64>> = (0..width)
            .map(|c| {
                (0..300)
                    .map(|i| (((i + 1) * (c as usize + 3)) % 37) as f64 / 18.0 - 1.0)
                    .collect()
            })
            .collect();
        let ramp = || {
            let mut g = Ramp::new(coefficients(SvfType::Bell, 800.0, 1.5, 6.0)).steps(16);
            g.retarget(coefficients(SvfType::Bell, 2_000.0, 0.8, -9.0));
            g
        };
        let mut bank = SvfBank::new(ChannelLayout::from(width));
        let all = run(&mut bank, &mut ramp(), &input, &[300]);
        for (c, ch) in input.iter().enumerate() {
            let mut one = SvfBank::new(ChannelLayout::MONO);
            let alone = run(&mut one, &mut ramp(), std::slice::from_ref(ch), &[300]).remove(0);
            assert_eq!(all[c], alone, "width {width}, channel {c}");
        }
    }
}

/// `reset` is silence: after it, a zero input gives exactly zero output.
#[test]
fn a_reset_bank_is_silent() {
    let mut bank = SvfBank::new(ChannelLayout::STEREO);
    let mut ramp = Ramp::new(coefficients(SvfType::BandPass, 300.0, 50.0, 0.0));
    let noise: Vec<f64> = (0..512)
        .map(|i| ((i * 31) % 17) as f64 / 8.0 - 1.0)
        .collect();
    run(&mut bank, &mut ramp, &[noise.clone(), noise], &[512]);
    bank.reset();
    let out = run(&mut bank, &mut ramp, &[vec![0.0; 64], vec![0.0; 64]], &[64]);
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}

/// A ramp's midpoint between two reachable designs is itself stable: an impulse
/// through the half-way coefficients decays (the last thousand frames of 150,000 peak
/// below half of the first thousand's). A biquad's stable region in `(a1, a2)` is convex,
/// so a blend of two stable sets stays inside; a ramp that extrapolated would leave it.
#[test]
fn a_ramps_midpoint_is_stable() {
    const FRAMES: usize = 150_000;
    let mut designs = Vec::new();
    for ty in TYPES {
        for (fc, q, gain) in [
            (20.0f64, 0.5f64, -24.0f64),
            (1_000.0, 0.707, 0.0),
            (12_000.0, 20.0, 24.0),
        ] {
            designs.push(coefficients(ty, fc, q, gain));
        }
    }
    for a in &designs {
        for b in &designs {
            let mid = a.lerp(b, 8, 16);
            let mut ramp = Ramp::new(mid);
            let mut bank = SvfBank::new(ChannelLayout::MONO);
            let mut x = vec![0.0f64; FRAMES];
            x[0] = 1.0;
            let y = run(&mut bank, &mut ramp, &[x], &[FRAMES]).remove(0);
            let peak = |r: core::ops::Range<usize>| y[r].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            let (first, last) = (peak(0..1_000), peak(FRAMES - 1_000..FRAMES));
            assert!(
                first.is_finite() && last.is_finite() && last <= 0.5 * first,
                "{a:?} -> {b:?}: first {first}, last {last}"
            );
        }
    }
}

const EXTREME_RATE: SampleRate = SampleRate::SR_192K;

/// The closed form of the bilinear Butterworth high-pass of cutoff `fc` at `EXTREME_RATE`, `H =
/// -W^2 / (1 - W^2 + j k W)` at `W = tan(w / 2) / g`, `g = tan(pi fc / fs)` and `k = sqrt 2`,
/// at the angular frequency `w` (per frame).
fn butterworth_high_pass(fc: Hz, w: f64) -> Complex64 {
    let g = prewarp(fc, EXTREME_RATE);
    let warped = libm::tan(w / 2.0) / g;
    -Complex64::new(warped * warped, 0.0)
        / Complex64::new(1.0 - warped * warped, core::f64::consts::SQRT_2 * warped)
}

/// A 1 s sine at `fc` through a high-pass of cutoff `fc` in `f32` by `step` (the state `[ic1,
/// ic2]`, the input `v0` and the taps `v1`, `v2` in, the output out), after a second that
/// settles it: the relative error of the fitted amplitude against `|H(fc)| = 1 / sqrt 2` and
/// the largest deviation of the second from the steady state.
fn high_pass_at_the_cutoff(fc: Hz, step: impl Fn(&mut [f32; 2], f32) -> f32) -> (f64, f64) {
    let w = 2.0 * PI * fc.get() / EXTREME_RATE.get();
    let whole = EXTREME_RATE.get() as usize;
    let h = butterworth_high_pass(fc, w);
    let mut state = [0.0f32; 2];
    let y: Vec<f64> = (0..2 * whole)
        .map(|n| f64::from(step(&mut state, libm::sin(w * n as f64) as f32)))
        .collect();
    let fit = sine_fit(&y, w, whole);
    (
        (fit.norm() / h.norm() - 1.0).abs(),
        steady_deviation(&y, w, whole, h),
    )
}

/// The f32 high-pass of the SVF bank: the library's own step at the f32 coefficients.
fn complement_step(fc: Hz) -> impl Fn(&mut [f32; 2], f32) -> f32 {
    let design = SvfDesign::new(
        SvfType::HighPass,
        fc,
        Q::BUTTERWORTH,
        Db::UNITY,
        EXTREME_RATE,
    );
    let c = SvfCoefficients::<f32>::from(&design);
    move |state, x| svf_step(&c, state, x)
}

/// Simper's `v1 = a1 ic1 + a2 v3` with `a1 = 1 / (1 + g (g + k))` stored in `f32`, the
/// recurrence otherwise the bank's: what storing `a1` and not its complement leaves.
fn stored_a1_step(fc: Hz) -> impl Fn(&mut [f32; 2], f32) -> f32 {
    let design = SvfDesign::new(
        SvfType::HighPass,
        fc,
        Q::BUTTERWORTH,
        Db::UNITY,
        EXTREME_RATE,
    );
    let a1 = (1.0 / (1.0 + design.g * (design.g + design.k))) as f32;
    let [m1, a2, a3] = [-design.k, design.a2, design.a3].map(|v| v as f32);
    move |s, v0| {
        let v3 = v0 - s[1];
        let v1 = a1 * s[0] + a2 * v3;
        let v2 = s[1] + (a2 * s[0] + a3 * v3);
        *s = [v1 + v1 - s[0], v2 + v2 - s[1]];
        v0 + m1 * v1 - v2
    }
}

/// The sweep of cutoffs about the documented extreme, 4 to 6 Hz in steps of 10 mHz.
fn extreme_cutoffs() -> impl Iterator<Item = Hz> {
    (0..=200).map(|i| Hz::new(4.0 + f64::from(i) * 0.01))
}

/// Storing `a1 = 1 / (1 + g (g + k))` in `f32` rounds it by up to half an ulp below 1, `2^-25 =
/// 3.0e-8`, which is `3e-4` of the damping `d = 1 - a1 = 1e-4` of a 5 Hz section at 192 kHz;
/// storing `d` rounds it by `2^-24` of itself, whatever the cutoff. The sweep of cutoffs about
/// 5 Hz reaches the former's worst, and never the latter's.
#[test]
fn the_complement_keeps_the_damping_of_a_low_cutoff_to_the_f32_ulp() {
    let (mut a1_worst, mut d_worst) = (0.0f64, 0.0f64);
    for fc in extreme_cutoffs() {
        let design = SvfDesign::new(
            SvfType::HighPass,
            fc,
            Q::BUTTERWORTH,
            Db::UNITY,
            EXTREME_RATE,
        );
        let stored_d = f64::from(SvfCoefficients::<f32>::from(&design).d);
        d_worst = d_worst.max((stored_d - design.d).abs() / design.d);
        let a1 = f64::from((1.0 / (1.0 + design.g * (design.g + design.k))) as f32);
        a1_worst = a1_worst.max(((1.0 - a1) - design.d).abs() / design.d);
    }
    assert!(d_worst <= 2f64.powi(-24), "d: {d_worst:e}");
    assert!(a1_worst > 2e-4, "a1: {a1_worst:e}");
}

/// A 5 Hz high-pass at 192 kHz, the SVF's documented range extreme, run in `f32`.
///
/// The oracle is the bilinear Butterworth high-pass's closed form (`butterworth_high_pass`):
/// at its cutoff `W = 1` and `|H| = 1 / k = 1 / sqrt 2`.
///
/// Noise: the step rounds each state word to the `f32` grid, an error uniform within half an
/// ulp, `2^-24` for words in `[0.5, 1)` (they peak at `1 / sqrt 2` here), with standard
/// deviation `2^-24 / sqrt 12 = 1.7e-8`. The pair's poles sit at radius `r = 1 - g k / 2`
/// (`g = tan(pi fc / fs) = 8.2e-5`), so an error injected in a word decays over `tau = 1 / (g
/// k) = 8 643` frames and the injections add as a random walk: the output's deviation has
/// standard deviation `1.7e-8 * sqrt((k^2 + 1) tau) = 2.8e-6` (`k^2 + 1` the squared gain from
/// the states to the output `v0 - k v1 - v2`), `-112 dB` against the signal. The bound is
/// 7.4 of those, `2.1e-5` (`N = 192 000` frames are measured, their expected maximum is
/// `sqrt(2 ln N) = 4.9` standard deviations, and 1.5 times that allows for the non-Gaussian
/// rounding tails and the correlated state noise); no sample of the second leaves it and the
/// fitted gain, an average over it, is within it. A step that adds `a3 v3` (`0.1` ulp of the
/// state) to the state on its own loses it every frame, and misses the bound by a factor of 3.
///
/// The `a1` storage's damping error shows in the gain: at the sweep's worst cutoff (4.03 Hz) its
/// gain is off by the damping's `3e-4` relative, the complement's by the noise alone.
#[test]
fn a_five_hz_high_pass_at_192_khz_holds_its_gain_and_noise_in_f32() {
    let whole = EXTREME_RATE.get() as usize;
    let tau = 1.0 / (prewarp(Hz::new(5.0), EXTREME_RATE) * core::f64::consts::SQRT_2);
    let sigma = 2f64.powi(-24) / 12f64.sqrt() * (3.0 * tau).sqrt();
    let bound = 1.5 * (2.0 * (whole as f64).ln()).sqrt() * sigma;
    let (error, worst) = high_pass_at_the_cutoff(Hz::new(5.0), complement_step(Hz::new(5.0)));
    assert!(
        error * core::f64::consts::FRAC_1_SQRT_2 < bound,
        "|H(fc)| error {error:e}"
    );
    assert!(worst < bound, "deviation {worst:e} over {bound:e}");

    let worst_cutoff = Hz::new(4.03);
    let (with_d, _) = high_pass_at_the_cutoff(worst_cutoff, complement_step(worst_cutoff));
    let (with_a1, _) = high_pass_at_the_cutoff(worst_cutoff, stored_a1_step(worst_cutoff));
    assert!(
        with_d * core::f64::consts::FRAC_1_SQRT_2 < bound,
        "d: {with_d:e}"
    );
    assert!(
        with_a1 * core::f64::consts::FRAC_1_SQRT_2 > 3.0 * bound,
        "a1: {with_a1:e}"
    );
}
