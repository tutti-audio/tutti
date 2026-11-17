use tutti_buffers::{Planes, PlanesMut};
use tutti_types::{Hz, Q, SampleRate, Seconds, Semitones};

use super::*;
use crate::testing::amplitude_at;
use crate::{Complex64, Lti};

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

/// A bank of `ty` at `RATE` aimed at `controls`.
fn bank(ty: SvfType, controls: &EnvelopeFilterControls) -> EnvelopeFilterBank {
    let mut bank = EnvelopeFilterBank::new(ChannelLayout::MONO, ty, RATE);
    bank.aim(controls);
    bank.settle();
    bank
}

/// `x` through `bank` in calls of at most `block` frames.
fn run(bank: &mut EnvelopeFilterBank, x: &[f32], block: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; x.len()];
    for (xs, ys) in x.chunks(block).zip(out.chunks_mut(block)) {
        bank.process(
            Planes::new(&[xs]).unwrap(),
            PlanesMut::new(&mut [ys]).unwrap(),
        );
    }
    out
}

/// The magnitude of the SVF `ty` at `cutoff` and `q`, at `hz`, from its transfer function.
fn magnitude(ty: SvfType, cutoff: f64, q: Q, hz: f64) -> f64 {
    let model = SvfModel::new(
        SvfControls {
            ty,
            freq: Hz(cutoff as f32),
            q,
            gain: tutti_types::Db(0.0),
        },
        RATE,
    );
    let mut h = [Complex64::new(0.0, 0.0)];
    model.frequency_response(0, &[Hz(hz as f32)], &mut h);
    h[0].norm()
}

/// The gain of the settled tail of `y` against `x` at `TONE`: the last whole periods.
fn measured_gain(x: &[f32], y: &[f32]) -> f64 {
    let tail = 4_800; // 100 periods of 1 kHz at 48 kHz
    let from = x.len() - tail;
    amplitude_at(&y[from..], TONE, RATE.get()) / amplitude_at(&x[from..], TONE, RATE.get())
}

fn controls(base: f32, depth: f32, q: f32) -> EnvelopeFilterControls {
    EnvelopeFilterControls {
        base: Hz(base),
        depth: Semitones(depth),
        q: Q(q),
        attack: Seconds(0.05),
        release: Seconds(0.05),
    }
}

/// A steady sine of amplitude `A` has the mean magnitude `2A/pi` (long times smooth the
/// rectified ripple away), so the cutoff settles at `base * 2^(depth/12 * 2A/pi)` and the
/// tone's gain is the SVF's analytic magnitude there.
#[test]
fn the_settled_cutoff_is_base_times_two_to_depth_times_the_mean_level() {
    for ty in [SvfType::LowPass, SvfType::BandPass] {
        for (base, depth, amplitude) in [
            (500.0, 24.0, 0.5),
            (500.0, 24.0, 1.0),
            (900.0, -12.0, 0.8),
            (400.0, 12.0, 0.25),
        ] {
            let q = Q(2.0);
            let mut b = bank(ty, &controls(base, depth, q.get()));
            let x = sine(TONE, amplitude, 96_000);
            let y = run(&mut b, &x, 480);
            let env = 2.0 * amplitude / core::f64::consts::PI;
            let cutoff = f64::from(base) * (f64::from(depth) / 12.0 * env).exp2();
            let want = magnitude(ty, cutoff, q, TONE);
            let got = measured_gain(&x, &y);
            assert!(
                (got - want).abs() < 3e-3 * want.max(1.0),
                "{ty:?} base {base} depth {depth} A {amplitude}: gain {got} vs {want} at {cutoff} Hz"
            );
            // The cutoff moved: the gain is not the fixed filter's.
            let fixed = magnitude(ty, f64::from(base), q, TONE);
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
    let q = Q(2.0);
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
        &controls(f32::NAN, f32::INFINITY, f32::NAN),
    );
    let y = run(&mut b, &sine(TONE, 0.5, 2_000), 128);
    assert!(y.iter().all(|s| s.is_finite()));
    assert_eq!(b.cutoff_at(1.0), EnvelopeFilterControls::DEFAULT.base);
    // Ten octaves above 1 kHz is past the Nyquist: the cutoff stops just short of it.
    let wide = bank(SvfType::LowPass, &controls(1_000.0, 1.0e9, 0.707));
    assert_eq!(wide.cutoff_at(1.0), RATE.nyquist_scaled(0.998));
    assert_eq!(wide.cutoff_at(0.0), Hz(1_000.0));
}
