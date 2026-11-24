use super::*;
use crate::TAIL_FLOOR;
use core::f64::consts::TAU;
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_fixtures::signal::{hann_image_leak, sine_frequency};

const RATE: SampleRate = SampleRate::SR_48K;
const C: f64 = Speed::SOUND_IN_AIR_20C.get();
/// The frames of a block in the Doppler runs: the distance is aimed once a block.
const BLOCK: usize = DISTANCE_GLIDE.get();

fn spec() -> DistanceSpec {
    DistanceSpec::new(Metres::of(100.0))
}

/// A spec whose reference is its farthest distance, so the level is 1 at every distance and
/// the output is the delayed input alone.
fn delay_only() -> DistanceSpec {
    spec().reference(ReferenceDistance::of(Metres::of(100.0)))
}

fn at(distance: f64) -> DistanceControls {
    DistanceControls {
        distance: Metres::of(distance),
    }
}

/// The distance whose delay is exactly `frames` frames at the rate.
fn distance_of(frames: usize) -> Metres {
    Metres::new(frames as f64 / RATE.get() * C).unwrap()
}

fn distance(spec: DistanceSpec, width: u16, start: &DistanceControls) -> Distance<f64> {
    Distance::new(spec, ChannelLayout::from_count(width), RATE, start)
}

/// `channels` through `d` in blocks of `block`, calling `before(k, d)` ahead of block `k`.
fn run(
    d: &mut Distance<f64>,
    channels: &[Vec<f64>],
    block: usize,
    mut before: impl FnMut(usize, &mut Distance<f64>),
) -> Vec<Vec<f64>> {
    let frames = channels[0].len();
    let mut out = vec![vec![0.0; frames]; channels.len()];
    for (k, at) in (0..frames).step_by(block).enumerate() {
        let end = (at + block).min(frames);
        before(k, d);
        let ins: Vec<Vec<f64>> = channels.iter().map(|c| c[at..end].to_vec()).collect();
        let mut outs = vec![vec![0.0; end - at]; channels.len()];
        d.process(
            &mut InputOutput::paired(
                Planes::from_vecs(&ins).unwrap(),
                PlanesMut::from_vecs(&mut outs).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        for (o, part) in out.iter_mut().zip(&outs) {
            o[at..end].copy_from_slice(part);
        }
    }
    out
}

fn impulse(frames: usize) -> Vec<f64> {
    tutti_fixtures::signal::impulse(Samples::new(frames))
}

/// **A static source is the input delayed by exactly `r / c` frames and scaled by exactly
/// `r_ref / r`**: an impulse at a distance whose delay is a whole 480 frames comes out at
/// frame 480 with the gain `r_ref / r`, for two references. The delay is held to `2^-32` of a
/// frame, so the gain and its neighbours are right to `2^-31` of it.
#[test]
fn a_static_source_is_delayed_by_r_over_c_and_scaled_by_reference_over_r() {
    let r = distance_of(480);
    for reference in [1.0, 2.0] {
        let spec = spec().reference(ReferenceDistance::of(Metres::of(reference)));
        let mut d = distance(spec, 1, &DistanceControls { distance: r });
        let y = run(&mut d, &[impulse(1000)], 1000, |_, _| {}).remove(0);
        let gain = reference / r.get();
        let slack = gain * 2f64.powi(-31);
        for (n, &v) in y.iter().enumerate() {
            let want = if n == 480 { gain } else { 0.0 };
            assert!((v - want).abs() <= slack, "frame {n}: {v} for {want}");
        }
    }
}

/// **The level is `r_ref / r` and halves per doubling of the distance** (6.02 dB): the gain at
/// `2 r` is half the gain at `r`.
#[test]
fn doubling_the_distance_halves_the_level() {
    let gain = |frames: usize| {
        let mut d = distance(
            spec(),
            1,
            &DistanceControls {
                distance: distance_of(frames),
            },
        );
        run(&mut d, &[impulse(2000)], 2000, |_, _| {})[0][frames]
    };
    assert!((gain(960) * 2.0 - gain(480)).abs() <= 2f64.powi(-31));
}

/// **A source nearer than the reference is not boosted**: a constant input at half the
/// reference distance settles at exactly 1.
#[test]
fn a_source_nearer_than_the_reference_has_unit_level() {
    let mut d = distance(spec(), 1, &at(0.5));
    let y = run(&mut d, &[vec![1.0; 400]], 400, |_, _| {}).remove(0);
    // The line is full after 0.5 m / c = 70 frames.
    assert!(y[100..].iter().all(|&v| (v - 1.0).abs() <= 2f64.powi(-31)));
}

/// **A receding source shifts down to `f c / (c + v)`.** A source at `v` m/s away is at
/// `r0 + v t` when it emits, and its sound arrives with the distance growing at
/// `u = v c / (c + v)`; a distance aimed at that line each block gives the delay slope `u / c`,
/// so a sine comes out at `f (1 - u / c) = f c / (c + v)`. The estimate is within the Hann
/// window's image leakage of the tone in each of the two segments.
#[test]
fn a_receding_source_is_heard_at_f_c_over_c_plus_v() {
    let (f, v) = (1000.0, 30.0);
    let want = f * C / (C + v);
    let heard = heard_frequency(f, v * C / (C + v), 1.0, want);
    assert!(
        heard.0.abs() <= heard.1,
        "off by {} past {}",
        heard.0,
        heard.1
    );
}

/// **An approaching source shifts up to `f c / (c - v)`**, the distance shrinking at
/// `u = v c / (c - v)`.
#[test]
fn an_approaching_source_is_heard_at_f_c_over_c_minus_v() {
    let (f, v) = (1000.0, 30.0);
    let want = f * C / (C - v);
    let heard = heard_frequency(f, -v * C / (C - v), 30.0, want);
    assert!(
        heard.0.abs() <= heard.1,
        "off by {} past {}",
        heard.0,
        heard.1
    );
}

/// A sine of `f` through a distance that starts at `r0` and changes at `u` m/s, aimed at the
/// line each block: the heard frequency's error against `want`, and the bound it must be within.
fn heard_frequency(f: f64, u: f64, r0: f64, want: f64) -> (f64, f64) {
    let frames = 30_000;
    let x: Vec<f64> = (0..frames)
        .map(|n| (TAU * f * n as f64 / RATE.get()).sin())
        .collect();
    let mut d = distance(delay_only(), 1, &at(r0));
    let y = run(&mut d, &[x], BLOCK, |k, d| {
        // The distance at the end of block `k`.
        let t = ((k + 1) * BLOCK) as f64 / RATE.get();
        d.retarget(&at(r0 + u * t));
    })
    .remove(0);
    let (measure, step) = (Samples::new(16_384), Samples::new(4096));
    // The line is full once the first delay has passed.
    let full = (r0 / C * RATE.get()) as usize + 200;
    let found = sine_frequency(&y[full..], Hz::new(want.round()), RATE, measure, step).get();
    let bound = 2.0 * hann_image_leak(Hz::new(want), measure, RATE) * RATE.get()
        / (TAU * step.get() as f64);
    (found - want, bound)
}

/// **A jump in distance moves the delay by at most `DELAY_SLEW` frames a frame**, and by that
/// much while it glides. A ramp `x[n] = n` through a linear read is `n - delay[n]`, so the
/// delay's first difference is `1 - (y[n] - y[n - 1])`. A jump from 1 m to 50 m is 6857
/// frames, which takes `ceil(6857 / 0.125)` frames, and lands on the delay of 50 m.
#[test]
fn a_jump_in_distance_glides_at_the_slew_and_never_steps() {
    let frames = 63_000;
    let x: Vec<f64> = (0..frames).map(|n| n as f64).collect();
    let mut d = distance(delay_only(), 1, &at(1.0));
    let y = run(&mut d, &[x], 1000, |k, d| {
        if k == 3 {
            d.retarget(&at(50.0));
        }
    })
    .remove(0);
    let delay: Vec<f64> = y.iter().enumerate().map(|(n, &y)| n as f64 - y).collect();
    // From frame 1000 on the line is full of the ramp.
    let steps: Vec<f64> = delay[1000..].windows(2).map(|w| w[1] - w[0]).collect();
    let fastest = steps.iter().copied().fold(0.0, f64::max);
    assert!(fastest <= DELAY_SLEW + 1e-9, "stepped {fastest}");
    assert!(
        fastest >= DELAY_SLEW - 1e-4,
        "glided at {fastest}, not at the slew"
    );
    let landed = 50.0 / C * RATE.get();
    assert!(
        (delay[frames - 1] - landed).abs() <= 1e-6,
        "landed at {}",
        delay[frames - 1]
    );
}

/// **Air absorption is the one-pole whose cutoff is `fc_ref r_ref / r`**: a sine at that
/// cutoff, at a distance whose delay is a whole 600 frames, comes out at the level `r_ref / r`
/// times the one-pole's magnitude `(1 - p) / |1 - p e^{-jw}|` for `p = exp(-w)` at `w = 2 pi
/// fc / fs`, which is not the half power point of a pole matched this way.
#[test]
fn air_absorption_is_a_one_pole_at_the_reference_cutoff_over_the_distance() {
    let r = distance_of(600);
    let fc_ref = Hz::new(9000.0);
    let fc = fc_ref.get() / r.get();
    let spec = spec().air_absorption(fc_ref);
    let mut d = distance(spec, 1, &DistanceControls { distance: r });
    let x: Vec<f64> = (0..8000)
        .map(|n| (TAU * fc * n as f64 / RATE.get()).sin())
        .collect();
    let y = run(&mut d, &[x], 8000, |_, _| {}).remove(0);
    // The amplitude over whole periods once the pole has settled and the line is full.
    let w = TAU * fc / RATE.get();
    let p = (-w).exp();
    let magnitude = (1.0 - p) / (1.0 - 2.0 * p * w.cos() + p * p).sqrt();
    let want = magnitude / r.get();
    let periods = ((4000.0 * fc / RATE.get()).floor() * RATE.get() / fc).round() as usize;
    let tone = &y[7000 - periods..7000];
    let (re, im) = tone
        .iter()
        .enumerate()
        .fold((0.0, 0.0), |(re, im), (n, &v)| {
            let a = w * (n + 7000 - periods) as f64;
            (re + v * a.sin(), im + v * a.cos())
        });
    // The sine's quadrature sums: amplitude `2 |.| / n` over a whole number of periods is
    // accurate to the one-sample edge, so the tolerance is the edge's share.
    let got = 2.0 * re.hypot(im) / periods as f64;
    assert!((got - want).abs() <= want * 1e-2, "{got} for {want}");
    // And the cutoff falls with the distance: at twice the distance the same sine is lower.
    let mut far = distance(
        spec,
        1,
        &DistanceControls {
            distance: distance_of(1200),
        },
    );
    let x: Vec<f64> = (0..8000)
        .map(|n| (TAU * fc * n as f64 / RATE.get()).sin())
        .collect();
    let y_far = run(&mut far, &[x], 8000, |_, _| {}).remove(0);
    let peak = |y: &[f64]| y[6000..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(peak(&y_far) < peak(&y) / 2.0);
}

/// **The tail covers everything the line still says after its input stops**: an impulse's
/// last sample above `TAIL_FLOOR` of its peak is within `tail`, with and without absorption.
#[test]
fn the_tail_covers_the_delay_and_the_absorption_ring_out() {
    for absorption in [None, Some(Hz::new(500.0))] {
        let spec = absorption.map_or(spec(), |fc| spec().air_absorption(fc));
        let controls = DistanceControls {
            distance: distance_of(480),
        };
        let mut d = distance(spec, 1, &controls);
        let tail = d.tail(&controls).get();
        let y = run(&mut d, &[impulse(tail + 2000)], 1000, |_, _| {}).remove(0);
        let peak = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let floor = TAIL_FLOOR.to_amplitude().get() * peak;
        let last = y.iter().rposition(|v| v.abs() > floor).unwrap();
        assert!(
            last < tail,
            "{absorption:?}: sounds at {last} past a tail of {tail}"
        );
        assert!(d.tail_ceiling().get() >= tail);
    }
}

/// **Each channel is delayed on its own line**: a left impulse at frame 0 and a right one at
/// frame 10 come out 480 frames on.
#[test]
fn every_channel_is_delayed_by_the_one_delay() {
    let mut d = distance(
        spec(),
        2,
        &DistanceControls {
            distance: distance_of(480),
        },
    );
    let mut right = vec![0.0; 1000];
    right[10] = 1.0;
    let y = run(&mut d, &[impulse(1000), right], 256, |_, _| {});
    let gain = 1.0 / distance_of(480).get();
    assert!((y[0][480] - gain).abs() <= gain * 2f64.powi(-31));
    assert!((y[1][490] - gain).abs() <= gain * 2f64.powi(-31));
    assert_eq!(y[0][490], 0.0);
}

/// **A reset silences the line**: what was in flight is gone and the next blocks are silent.
#[test]
fn a_reset_drops_what_was_in_flight() {
    let mut d = distance(spec().air_absorption(Hz::new(2000.0)), 1, &at(2.0));
    run(&mut d, &[vec![1.0; 500]], 500, |_, _| {});
    d.reset();
    let y = run(&mut d, &[vec![0.0; 500]], 500, |_, _| {}).remove(0);
    assert!(y.iter().all(|&v| v == 0.0));
}

/// **A reference distance is at least 1 mm**: the level `r_ref / r` is a ratio of lengths, so
/// zero and anything under a millimetre are refused rather than raised to it.
#[test]
fn a_reference_distance_takes_exactly_the_distances_from_a_millimetre() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: ReferenceDistance::new,
        get: ReferenceDistance::get,
        of: Some(ReferenceDistance::of),
        valid: &[Metres::of(0.001), Metres::of(1.0), Metres::of(100.0)],
        invalid: &[Metres::ZERO, Metres::of(0.000_999)],
    });
}
