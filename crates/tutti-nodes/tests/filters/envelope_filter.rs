//! The envelope filter node: with a steady sine of amplitude `A` the envelope settles at the
//! mean magnitude `2A/pi`, the cutoff at `base * 2^(depth/12 * 2A/pi)`, and the tone's gain is
//! the SVF's closed-form magnitude there: for a low-pass `1 / hypot(1 - w^2, w / q)` with the
//! prewarped `w = tan(pi f / fs) / tan(pi fc / fs)`.

use core::f64::consts::PI;

use tutti_buffers::Planes;
use tutti_dsp::SvfType;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{EnvelopeFilterConfig, EnvelopeFilterNode};
use tutti_types::{ChannelLayout, Hz, Q, SampleRate, Seconds, Semitones};

const SR: SampleRate = SampleRate::SR_48K;
const FS: f64 = 48_000.0;
const TONE: f64 = 1_000.0;

fn lowpass_magnitude(f: f64, fc: f64, q: f64) -> f64 {
    let w = (PI * f / FS).tan() / (PI * fc / FS).tan();
    1.0 / (1.0 - w * w).hypot(w / q)
}

/// The settled gain of the node at `TONE` for a sine of `amplitude`.
fn settled_gain(depth: f64, amplitude: f64, q: f64) -> f64 {
    let mut node = prepared(
        EnvelopeFilterNode::from(
            EnvelopeFilterConfig::new(ChannelLayout::MONO, Hz(500.0), SvfType::LowPass)
                .depth(Semitones(depth as f32))
                .q(Q(q as f32))
                .attack(Seconds(0.05))
                .release(Seconds(0.05)),
        ),
        SR,
        480,
    );
    let x: Vec<f32> = (0..96_000)
        .map(|i| (amplitude * (2.0 * PI * TONE * i as f64 / FS).sin()) as f32)
        .collect();
    let y: Vec<f32> = x
        .chunks(480)
        .flat_map(|b| drive(&mut node, SR, Planes::new(&[b]).unwrap(), &[]).remove(0))
        .collect();
    let tail = 4_800;
    let (mut yr, mut yi, mut xr, mut xi) = (0.0, 0.0, 0.0, 0.0);
    for (n, (a, b)) in x[x.len() - tail..]
        .iter()
        .zip(&y[y.len() - tail..])
        .enumerate()
    {
        let (s, c) = (2.0 * PI * TONE * n as f64 / FS).sin_cos();
        xr += f64::from(*a) * c;
        xi += f64::from(*a) * s;
        yr += f64::from(*b) * c;
        yi += f64::from(*b) * s;
    }
    yr.hypot(yi) / xr.hypot(xi)
}

#[test]
fn the_settled_cutoff_follows_the_envelope_by_the_closed_form() {
    let q = 2.0;
    for (depth, amplitude) in [(24.0, 0.5), (24.0, 1.0), (-12.0, 0.8)] {
        let env = 2.0 * amplitude / PI;
        let cutoff = 500.0 * (depth / 12.0 * env).exp2();
        let want = lowpass_magnitude(TONE, cutoff, q);
        let got = settled_gain(depth, amplitude, q);
        assert!(
            (got - want).abs() < 3e-3 * want.max(1.0),
            "depth {depth} A {amplitude}: {got} vs {want} at {cutoff} Hz"
        );
    }
}

#[test]
fn zero_depth_is_the_fixed_svf_at_base_at_any_level() {
    let want = lowpass_magnitude(TONE, 500.0, 2.0);
    for amplitude in [0.1, 0.9] {
        let got = settled_gain(0.0, amplitude, 2.0);
        assert!((got - want).abs() < 1e-3, "A {amplitude}: {got} vs {want}");
    }
}
