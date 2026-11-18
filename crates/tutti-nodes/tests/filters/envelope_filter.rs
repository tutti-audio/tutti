//! The envelope filter node: with a steady sine of amplitude `A` the envelope settles at the
//! mean magnitude `2A/pi`, the cutoff at `base * 2^(depth/12 * 2A/pi)`, and the tone's gain is
//! the SVF's closed-form magnitude there: for a low-pass `1 / hypot(1 - w^2, w / q)` with the
//! prewarped `w = tan(pi f / fs) / tan(pi fc / fs)`. The attack and release set through the
//! node's params are told apart by a square wave, whose magnitude is constant.

use core::f64::consts::PI;

use tutti_buffers::Planes;
use tutti_dsp::SvfType;
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::{EnvelopeFilterConfig, EnvelopeFilterNode};
use tutti_types::ChannelLayout;
use tutti_units::{Hz, Q, SampleRate, Seconds, Semitones, UnitParam};

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
        EnvelopeFilterNode::<f32>::from(
            EnvelopeFilterConfig::new(ChannelLayout::MONO, Hz(500.0), SvfType::LowPass)
                .depth(Semitones(depth))
                .q(Q(q))
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

/// Equal times make the follower linear on the rectified sine, so its mean is `2A/pi` exactly;
/// the 2 kHz ripple is about 1e-3 of it, which bounds the relative gain error under 3e-3.
#[test]
fn the_settled_cutoff_follows_the_envelope_by_the_closed_form() {
    let q = 2.0;
    for (depth, amplitude) in [(24.0, 0.5), (24.0, 1.0), (-12.0, 0.8)] {
        let env = 2.0 * amplitude / PI;
        let cutoff = 500.0 * (depth / 12.0 * env).exp2();
        let want = lowpass_magnitude(TONE, cutoff, q);
        let got = settled_gain(depth, amplitude, q);
        assert!(
            (got - want).abs() < 3e-3 * want,
            "depth {depth} A {amplitude}: {got} vs {want} at {cutoff} Hz"
        );
    }
}

/// The fundamental of `x` over 480 frames from `from`: the lock-in amplitude at `TONE`.
fn fundamental(x: &[f32], from: usize) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (n, v) in x[from..from + 480].iter().enumerate() {
        let (s, c) = (2.0 * PI * TONE * (from + n) as f64 / FS).sin_cos();
        re += f64::from(*v) * c;
        im += f64::from(*v) * s;
    }
    re.hypot(im)
}

/// A square wave's magnitude is its level, so the envelope is `L (1 - a^n)` after a step up with
/// the attack's `a` and `L2 + (L - L2) a^n` after a step down with the release's (`a = exp(-1 /
/// (tau rate))`). With the params set to attack 5 ms and release 200 ms (the config's are 10 and
/// 100 ms), the low-pass gain of the fundamental 65 ms after the rise and 100 ms after the fall
/// is the closed-form magnitude at the cutoff of that envelope, and differs by about 40 percent
/// from what swapped params would give. Tolerance 2e-2 of the gain: the oracle averages over the
/// window the gain, which moves with the envelope, with the design lagging half a period.
#[test]
fn the_attack_and_release_params_set_the_rise_and_the_fall() {
    let (attack, release, high, low) = (0.005, 0.2, 0.8, 0.2);
    let (rise, fall) = (14_400, 9_600);
    let (base, depth, q) = (400.0, 36.0, 2.0);
    let mut node = prepared(
        EnvelopeFilterNode::<f32>::from(
            EnvelopeFilterConfig::new(ChannelLayout::MONO, Hz(base), SvfType::LowPass)
                .depth(Semitones(depth))
                .q(Q(q)),
        ),
        SR,
        480,
    );
    assert!(node.param_set().set(UnitParam::Attack, attack as f32));
    assert!(node.param_set().set(UnitParam::Release, release as f32));
    let x: Vec<f32> = (0..rise + fall)
        .map(|n| {
            let level = if n < rise { high } else { low };
            let phase = 2.0 * PI * TONE * (n as f64 + 0.5) / FS;
            (level * phase.sin().signum()) as f32
        })
        .collect();
    let y: Vec<f32> = x
        .chunks(480)
        .flat_map(|b| drive(&mut node, SR, Planes::new(&[b]).unwrap(), &[]).remove(0))
        .collect();
    let pole = |tau: f64| (-1.0 / (tau * FS)).exp();
    let gain_at = |env: f64| lowpass_magnitude(TONE, base * (depth / 12.0 * env).exp2(), q);
    let mean = |envs: &dyn Fn(i32) -> f64, from: i32| {
        (from..from + 480)
            .step_by(16)
            .map(|i| gain_at(envs(i - 7)))
            .sum::<f64>()
            / 30.0
    };
    let up = 2_880;
    let want_up = mean(&|n| high * (1.0 - pole(attack).powi(n)), up);
    let got_up = fundamental(&y, up as usize) / fundamental(&x, up as usize);
    assert!(
        (got_up - want_up).abs() < 2e-2 * want_up,
        "up {got_up} vs {want_up}"
    );
    let down = 4_560;
    let want_down = mean(&|n| low + (high - low) * pole(release).powi(n), down);
    let got_down = fundamental(&y, rise + down as usize) / fundamental(&x, rise + down as usize);
    assert!(
        (got_down - want_down).abs() < 2e-2 * want_down,
        "down {got_down} vs {want_down}"
    );
}
