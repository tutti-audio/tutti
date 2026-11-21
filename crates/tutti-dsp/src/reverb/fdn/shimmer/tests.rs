use core::f64::consts::TAU;

use tutti_units::{Cents, Hz, SampleRate, Samples};

use super::*;
use crate::{Noise, NoiseColour};

const RATE: SampleRate = SampleRate::SR_48K;

/// The shifter's output for `input` at `pitch`, in chunks, with zeros after it to run its
/// delay out.
fn run(pitch: f64, input: &[f32]) -> Vec<f32> {
    let mut s = Shimmer::<f32>::new(RATE);
    s.retarget(Cents::new(pitch));
    let total = (input.len() + 6000).next_multiple_of(CHUNK);
    let mut out = Vec::with_capacity(total);
    for c in 0..total / CHUNK {
        for i in 0..CHUNK {
            s.push(i, input.get(c * CHUNK + i).copied().unwrap_or(0.0));
            out.push(s.back(i));
        }
        s.end_chunk();
    }
    out
}

fn l2(x: &[f32]) -> f64 {
    x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>().sqrt()
}

/// An FFT roundtrip's relative error in `f32`: each transform grows rounding like
/// `eps * log2(window)`, and a frame is one forward and one inverse.
fn f32_transform_error() -> f64 {
    2.0 * 2048.0f64.log2() * f64::from(f32::EPSILON)
}

fn sine(f: f64, amplitude: f64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| (amplitude * (TAU * f * n as f64 / RATE.get()).sin()) as f32)
        .collect()
}

/// The amplitude of the sine at `f` in `x`: the Hann-windowed DFT at exactly `f` over its
/// window sum.
fn amplitude(x: &[f32], f: f64) -> f64 {
    let x: Vec<f64> = x.iter().map(|&v| v.into()).collect();
    let (re, im) = tutti_fixtures::signal::hann_dft(&x, Hz::new(f), RATE);
    4.0 * re.hypot(im) / x.len() as f64
}

/// The shifter's gain over `l2` is at most 1 (the overlap-add's bound) whatever the signal and
/// the shift: measured as the energy ratio over noise, a sine, impulses, a chirp and noise
/// bursts, at every 400 cents of the two octaves either way. It is what keeps the shifted
/// share of the feedback under [`GAIN_BOUND`]. The bound is 1 to the `f32` transform's error.
#[test]
fn the_shifter_gain_over_l2_is_at_most_one_at_every_shift() {
    let n = 48_000;
    let mut noise = Noise::<f32>::new(NoiseColour::White, 7);
    let white: Vec<f32> = (0..n).map(|_| noise.next_sample()).collect();
    let mut impulses = vec![0.0f32; n];
    impulses.iter_mut().step_by(777).for_each(|v| *v = 1.0);
    let chirp: Vec<f32> = (0..n)
        .map(|i| (1e-6 * (i as f64).powi(2)).sin() as f32)
        .collect();
    let bursts: Vec<f32> = white
        .iter()
        .enumerate()
        .map(|(i, &v)| if (i / 3000) % 2 == 0 { v } else { 0.0 })
        .collect();
    let signals = [white, sine(1100.0, 1.0, n), impulses, chirp, bursts];
    for cents in (-2400..=2400).step_by(400) {
        for (k, x) in signals.iter().enumerate() {
            let gain = l2(&run(f64::from(cents), x)) / l2(x);
            assert!(
                gain <= 1.0 + f32_transform_error(),
                "signal {k} at {cents} cents: gain {gain}"
            );
        }
    }
    assert!(GAIN_BOUND > 1.0 + f32_transform_error());
}

/// At no shift the shifter returns what it was given, delayed by [`Shimmer::delay`]: an
/// impulse in frame `p` of the second chunk comes back as an impulse of the same height at
/// `p + delay`, and nothing else, to the transform's error.
#[test]
fn at_zero_cents_an_impulse_returns_after_the_delay() {
    let at = CHUNK + 5;
    let mut x = vec![0.0f32; 8000];
    x[at] = 0.8;
    let y = run(0.0, &x);
    let delay = Shimmer::<f32>::new(RATE).delay().get();
    let bound = f32_transform_error();
    for (i, &v) in y.iter().enumerate() {
        let want = if i == at + delay { 0.8 } else { 0.0 };
        assert!(
            (f64::from(v) - want).abs() <= bound,
            "frame {i}: {v} for {want}"
        );
    }
}

/// A tone through the shimmer's loop at +1200 cents, each pass scaled by `k / B` as the bank
/// writes it back, lands an octave higher each pass and falls by `k / B` a pass: the third pass
/// of a 1500 Hz tone is at 12 kHz at `(k / B)^3` of its amplitude. The frequency is held to the
/// estimator's own leakage bound, and the amplitude to 1% a pass (the cubic read's
/// attenuation of a tone below a quarter of the rate is far under it).
#[test]
fn a_tone_climbs_an_octave_and_falls_by_the_loop_gain_each_pass() {
    const MEASURE: usize = 8192;
    let (f0, a0) = (1500.0, 0.5);
    let k = 0.99;
    let mut x = sine(f0, a0, 60_000);
    for pass in 1..=3 {
        x = run(1200.0, &x)
            .iter()
            .map(|&v| v * (k / GAIN_BOUND) as f32)
            .collect();
        let f = f0 * f64::from(1 << pass);
        let seg: Vec<f64> = x[30_000..30_000 + MEASURE + 4096]
            .iter()
            .map(|&v| v.into())
            .collect();
        let got = tutti_fixtures::signal::sine_frequency(
            &seg,
            Hz::new(f),
            RATE,
            Samples::new(MEASURE),
            Samples::new(4096),
        )
        .get();
        let leak = tutti_fixtures::signal::hann_image_leak(Hz::new(f), Samples::new(MEASURE), RATE);
        let bound = 2.0 * leak * RATE.get() / (TAU * 4096.0);
        assert!(
            (got - f).abs() < bound,
            "pass {pass}: {got} Hz for {f} (bound {bound})"
        );
        let want = a0 * (k / GAIN_BOUND).powi(pass);
        let have = amplitude(&x[30_000..30_000 + MEASURE], f);
        assert!(
            (have / want) < 1.001 && (have / want) > 0.99f64.powi(pass),
            "pass {pass}: amplitude {have} for {want}"
        );
    }
}

/// A non-finite sample in the shifter's input does not stay in it: the vocoder is clean once
/// the frames whose windows held it have passed.
#[test]
fn a_non_finite_sample_does_not_latch_in_the_shifter() {
    let mut x = sine(900.0, 0.5, 30_000);
    x[1000] = f32::NAN;
    let y = run(700.0, &x);
    let delay = Shimmer::<f32>::new(RATE).delay().get();
    // The window's frames that held it, at the slowest step, are done a few windows on.
    let clean = 1000 + delay + 4 * 2048 + 4096;
    assert!(y[clean..].iter().all(|v| v.is_finite()));
    assert!(l2(&y[clean..]) > 1.0, "and the tone is back");
}
