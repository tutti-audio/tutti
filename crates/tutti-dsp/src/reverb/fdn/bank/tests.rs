use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::impulse;
use tutti_units::{
    Bipolar, Cents, Db, Feedback, Hz, Mix, ReadPosition, SampleRate, Samples, Seconds, StereoWidth,
    Tail,
};

use super::*;
use crate::reverb::fdn::design::MAX_PRE_DELAY;
use crate::reverb::network::{Fdn, Network, NetworkStage};
use crate::reverb::tail_probe;
use crate::{FreezeMode, MaxDecay};
use tutti_buffers::InputOutput;

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.
const RATE: SampleRate = SampleRate::SR_48K;

fn controls(f: impl FnOnce(&mut FdnControls)) -> FdnControls {
    let mut c = FdnControls::DEFAULT;
    f(&mut c);
    c
}

/// No modulation, so every line reads at its whole length.
fn still(f: impl FnOnce(&mut FdnControls)) -> FdnControls {
    controls(|c| {
        c.modulation = Bipolar::of(0.0);
        f(c);
    })
}

fn layout(lines: usize, freeze: FreezeMode) -> FdnLayout {
    FdnLayout::new(lines, MaxDecay::DEFAULT, freeze).unwrap()
}

/// A bank of `layout` at `rate`, at rest on `c`.
fn at_rest(layout: &FdnLayout, rate: SampleRate, c: &FdnControls) -> (FdnBank<f32>, FdnRamps<f32>) {
    let mut bank = FdnBank::<f32>::new(layout, rate);
    let mut ramps = bank.ramps();
    bank.retarget(&mut ramps, c);
    ramps.jump_to_target();
    (bank, ramps)
}

/// The design of `c`.
fn design(layout: &FdnLayout, rate: SampleRate, c: &FdnControls) -> Box<Network> {
    let mut network = Box::new(Network::new(rate));
    layout.design_into(c, rate, &mut network);
    network
}

fn fdn_of(n: &Network) -> &Fdn {
    let Some(NetworkStage::Fdn(f)) = n.stages.first() else {
        panic!("one FDN stage")
    };
    f
}

/// `frames` of a deterministic noise in `-0.5..0.5`.
fn noise(seed: u32, frames: usize) -> Vec<f32> {
    tutti_fixtures::noise::<f32>(seed, frames)
        .iter()
        .map(|v| 0.5 * v)
        .collect()
}

/// `x` through the bank in calls of `block` frames; the two output channels.
fn run(
    bank: &mut FdnBank<f32>,
    ramps: &mut FdnRamps<f32>,
    x: &[f32],
    block: usize,
) -> [Vec<f32>; 2] {
    let (mut l, mut r) = (vec![0.0f32; x.len()], vec![0.0f32; x.len()]);
    let mut at = 0;
    for piece in x.chunks(block) {
        let end = at + piece.len();
        bank.process(
            ramps,
            &mut InputOutput::paired(
                Planes::new(&[piece]).unwrap(),
                PlanesMut::new(&mut [&mut l[at..end], &mut r[at..end]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        at = end;
    }
    [l, r]
}

/// The Schroeder backward integral of `h` squared, in dB below its total.
fn schroeder_db(h: &[f64]) -> Vec<f64> {
    let mut total = 0.0;
    let mut e = vec![0.0; h.len()];
    for (i, x) in h.iter().enumerate().rev() {
        total += x * x;
        e[i] = total;
    }
    let all = e[0];
    e.iter().map(|x| 10.0 * (x / all).log10()).collect()
}

/// The RT60 of a decay curve in dB at `rate`, from the time it takes to fall from -20 dB to
/// -50 dB (30 dB, past the first echoes, which a few trips make a staircase), doubled.
fn rt60_of(curve: &[f64], rate: SampleRate) -> f64 {
    let at = |db: f64| {
        curve
            .iter()
            .position(|&x| x <= db)
            .expect("the curve falls")
    };
    2.0 * (at(-50.0) - at(-20.0)) as f64 / rate.get()
}

/// The decay time of the loop of one line of `length` frames whose per-trip gain is `gain`:
/// the time `gain^(t * rate / length)` takes to fall 60 dB.
fn rt60_of_trip_gain(gain: f64, length: f64, rate: SampleRate) -> f64 {
    60.0 * length / (rate.get() * -20.0 * gain.log10())
}

/// The per-trip gain of a line of `length` frames that loses 60 dB in `rt60` seconds.
fn trip_gain(length: f64, rt60: f64, rate: SampleRate) -> f64 {
    10f64.powf(-3.0 * length / (rate.get() * rt60))
}

/// A frequency-independent absorption gives every mode of the network the same decay, for
/// any orthogonal mix, since scaling every line of length `L` by `g^L` moves every pole of the
/// lossless network radially to `g`: the Schroeder integral of the impulse response falls
/// 60 dB in the designed RT60, from parallel combs (diffusion 0) to the Householder matrix.
#[test]
fn the_schroeder_integral_falls_60_db_in_the_designed_rt60() {
    let l = layout(8, FreezeMode::Decaying);
    for rt60 in [0.5, 1.0, 2.0] {
        for diffusion in [0.0, 1.0] {
            let c = still(|c| {
                c.decay = [Seconds::new(rt60); 3];
                c.diffusion = Mix::new(diffusion);
            });
            let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
            let frames = (2.0 * rt60 * RATE.get()) as usize;
            let [left, right] = run(&mut bank, &mut ramps, &impulse(Samples::new(frames)), 64);
            // The two channels' energy, summed frame for frame.
            let h: Vec<f64> = left
                .iter()
                .zip(&right)
                .map(|(&l, &r)| libm::hypot(f64::from(l), f64::from(r)))
                .collect();
            let measured = rt60_of(&schroeder_db(&h), RATE);
            assert!(
                (measured - rt60).abs() < 0.04 * rt60,
                "RT60 {rt60} s, diffusion {diffusion}: measured {measured}"
            );
        }
    }
}

/// The response of the three-band absorption of one trip at `w` rad per frame: the gains
/// `g = [low, mid, high]` joined by two cascaded one-poles at each crossover, `g_high +
/// (g_mid - g_high) L2 + (g_low - g_mid) L1` with `L(w) = ((1 - p) / (1 - p e^-jw))^2` and
/// `p = exp(-2 pi fc / fs)`.
fn absorption_at(g: [f64; 3], w: f64, rate: SampleRate) -> f64 {
    let l = |fc: f64| {
        let p = libm::exp(-std::f64::consts::TAU * fc / rate.get());
        let den = (1.0 - p * libm::cos(w), p * libm::sin(w));
        let one = ((1.0 - p) * den.0, -(1.0 - p) * den.1);
        let norm = den.0 * den.0 + den.1 * den.1;
        let one = (one.0 / norm, one.1 / norm);
        (one.0 * one.0 - one.1 * one.1, 2.0 * one.0 * one.1)
    };
    let (l1, l2) = (l(300.0), l(4_000.0));
    let re = g[2] + (g[1] - g[2]) * l2.0 + (g[0] - g[1]) * l1.0;
    let im = (g[1] - g[2]) * l2.1 + (g[0] - g[1]) * l1.1;
    libm::hypot(re, im)
}

/// The magnitude of the DTFT of `block` at `w` rad per frame.
fn dtft(block: &[f32], w: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &x) in block.iter().enumerate() {
        re += f64::from(x) * libm::cos(w * n as f64);
        im -= f64::from(x) * libm::sin(w * n as f64);
    }
    libm::hypot(re, im)
}

/// Each band decays at its own designed RT60. Two diagonal lines are two independent combs,
/// the left channel the first line alone: its echo `k` (the `k`-th trip of its loop, `L`
/// frames after the last) is the impulse filtered by the absorption `k` times, so the DTFT of
/// one trip over the one before it is the absorption's magnitude at that frequency, and the
/// RT60 that gain over `L` frames gives is the decay at the frequency. At DC the absorption is
/// the low band's gain exactly, and the decay is the low band's RT60; at the middle of the
/// crossovers and at Nyquist it is the three bands' response there, which the test computes
/// from the one-poles. The decay falls with the frequency as the bands say (3 s, 1.5 s, 0.6 s).
#[test]
fn each_band_decays_at_its_designed_rt60() {
    let l = layout(2, FreezeMode::Decaying);
    let c = still(|c| {
        c.decay = [Seconds::new(1.5), Seconds::new(3.0), Seconds::new(0.6)];
        c.diffusion = Mix::new(0.0);
        c.pre_delay = Seconds::ZERO;
        c.width = StereoWidth::new(1.0);
        c.wet = Db::UNITY;
    });
    let length = fdn_of(&design(&l, RATE, &c)).lines[0].length.get();
    assert_eq!(length.fract(), 0.0, "a whole number of frames");
    let trip = length as usize;
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
    let [left, _] = run(&mut bank, &mut ramps, &impulse(Samples::new(8 * trip)), 64);

    let g = [3.0, 1.5, 0.6].map(|t| trip_gain(length, t, RATE));
    // The crossovers (300 Hz and 4 kHz) are the design's, restated here: the test's own copy
    // of the figures the absorption's response is written in.
    let mid_hz = libm::sqrt(300.0 * 4_000.0);
    let probes = [
        ("DC", 0.0),
        ("the middle", std::f64::consts::TAU * mid_hz / RATE.get()),
        ("Nyquist", std::f64::consts::PI),
    ];
    let mut measured = Vec::new();
    for (name, w) in probes {
        let expected = absorption_at(g, w, RATE);
        let mut ratios = Vec::new();
        for k in 0..5 {
            let echo = |k: usize| &left[(k + 1) * trip..(k + 2) * trip];
            let ratio = dtft(echo(k + 1), w) / dtft(echo(k), w);
            assert!(
                (ratio - expected).abs() < 2e-3 * expected,
                "{name}, echo {k}: {ratio} against the absorption's {expected}"
            );
            ratios.push(ratio);
        }
        // The decay the impulse response shows, from its own echoes.
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        measured.push(rt60_of_trip_gain(mean, length, RATE));
    }
    // At DC the absorption is the low band's gain, so the decay is the designed 3 s.
    assert!((measured[0] - 3.0).abs() < 0.005 * 3.0, "{measured:?}");
    // At Nyquist it is the high band's, a little raised by the mid band's tail of the upper
    // one-pole.
    assert!((measured[2] - 0.6).abs() < 0.06 * 0.6, "{measured:?}");
    assert!(
        measured[0] > measured[1] && measured[1] > measured[2],
        "{measured:?}"
    );
}

/// A frozen network is lossless: every gain exactly 1, the input shut and the modulation off,
/// so the lines hold their energy, `||(I - 2 v v^T) d|| = ||d||` for the unit `v` each frame.
/// The energy the lines hold (their whole state while frozen) is the same 5 s apart to
/// 0.01 dB; the matrix is applied once to each sample a trip, so rounding the reflection
/// vector to `f32` moves it 1e-4 dB at most.
#[test]
fn a_frozen_network_keeps_its_energy() {
    let l = layout(8, FreezeMode::Freezable);
    let c = controls(|c| c.diffusion = Mix::new(1.0));
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
    run(&mut bank, &mut ramps, &noise(1, 24_000), 64);
    bank.retarget(
        &mut ramps,
        &controls(|c| {
            c.diffusion = Mix::new(1.0);
            c.freeze = true;
        }),
    );
    // Past the ramp to the frozen gains.
    run(&mut bank, &mut ramps, &vec![0.0; 4_800], 64);
    let before = bank.state_energy(&ramps);
    assert!(before > 0.0, "the lines hold a tail");
    run(&mut bank, &mut ramps, &vec![0.0; 240_000], 64);
    let db = 10.0 * (bank.state_energy(&ramps) / before).log10();
    assert!(db.abs() < 0.01, "the frozen energy moved {db:.5} dB in 5 s");
}

/// A frozen tail keeps its energy through a diffusion ramp from 0 to 1: the reflection is a
/// unit vector at every step, not a straight line between two (which dips to `|v|^2` of 0.68
/// and is a contraction). The energy the lines hold before and after the 20 ms ramp agrees
/// to 0.01 dB. (The output's level would not do: it is the lines' energy over their lengths,
/// and mixing moves energy between lines of different length.)
#[test]
fn a_diffusion_ramp_keeps_a_frozen_tails_energy() {
    let l = layout(8, FreezeMode::Freezable);
    let at = |diffusion: f64, freeze: bool| {
        still(|c| {
            c.diffusion = Mix::new(diffusion);
            c.freeze = freeze;
        })
    };
    let (mut bank, mut ramps) = at_rest(&l, RATE, &at(0.0, false));
    run(&mut bank, &mut ramps, &noise(5, 48_000), 64);
    bank.retarget(&mut ramps, &at(0.0, true));
    run(&mut bank, &mut ramps, &vec![0.0; 24_000], 64);
    let before = bank.state_energy(&ramps);
    bank.retarget(&mut ramps, &at(1.0, true));
    run(&mut bank, &mut ramps, &vec![0.0; 9_600], 64);
    let db = 10.0 * (bank.state_energy(&ramps) / before).log10();
    assert!(
        db.abs() < 0.01,
        "the frozen energy moved {db:.4} dB through the ramp"
    );
}

/// At diffusion 0 the lines do not couple: each is its own comb with a positive loop gain, so
/// the response's sum (its value at DC) is `wet * sum_j c_j b_j / (1 - g_j)`, with `g_j` the
/// line's DC loop gain. A sign on one line's loop would make its term `c b / (1 + g)`, a small
/// fraction of the figure at a long decay.
#[test]
fn a_diagonal_network_is_the_identity_a_comb_a_line_with_a_positive_sign() {
    let l = layout(8, FreezeMode::Freezable);
    let c = still(|c| {
        c.decay = [Seconds::new(2.0); 3];
        c.diffusion = Mix::new(0.0);
        c.pre_delay = Seconds::ZERO;
        c.width = StereoWidth::new(1.0);
    });
    let net = design(&l, RATE, &c);
    let f = fdn_of(&net);
    let expected: f64 = (0..f.lines.len())
        .map(|j| {
            let b = f.input[j].get();
            let c = f.output.rows[0][j].get();
            let g = f.lines[j].absorption.gain(Band::Low).get();
            b * c / (1.0 - g)
        })
        .sum::<f64>()
        * net.wet.to_amplitude().get();
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
    let [left, _] = run(
        &mut bank,
        &mut ramps,
        &impulse(Samples::new(10 * 48_000)),
        64,
    );
    let sum: f64 = left.iter().map(|&v| f64::from(v)).sum();
    assert!(
        (sum - expected).abs() < 0.01 * expected.abs(),
        "the response sums to {sum}, the identity's comb bank to {expected}"
    );
}

/// The first echo is the shortest line's: the impulse through the pre-delay, the input gain
/// `1 / sqrt(n)` and the output gain `sqrt(2 / n)` of an `n`-line network, the wet gain, and
/// nothing before it. Width 1 leaves the line on its own ear and width 0 puts half of it on
/// both.
#[test]
fn the_first_echo_follows_the_pre_delay_the_wet_gain_and_the_width() {
    let n = 8;
    let l = layout(n, FreezeMode::Decaying);
    let echo = |wet: Db, width: f64| {
        let c = still(|c| {
            c.pre_delay = Seconds::new(0.1);
            c.wet = wet;
            c.width = StereoWidth::new(width);
        });
        let net = design(&l, RATE, &c);
        let first = fdn_of(&net)
            .lines
            .iter()
            .map(|line| line.length.get() as usize)
            .min()
            .unwrap();
        let at = 4_800 + first;
        let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
        let [left, right] = run(&mut bank, &mut ramps, &impulse(Samples::new(at + 64)), 64);
        assert!(
            left[..at].iter().chain(&right[..at]).all(|&x| x == 0.0),
            "silent until the pre-delay and the shortest line have gone round"
        );
        (left[at], right[at])
    };
    let unit = (1.0 / (n as f32).sqrt()) * (2.0 / n as f32).sqrt();
    let (l0, r0) = echo(Db::UNITY, 1.0);
    assert!(
        (l0 - unit).abs() < 1e-6 && r0 == 0.0,
        "{l0} {r0} against {unit}"
    );
    let (l6, _) = echo(Db::new(-6.0206), 1.0);
    assert!((l6 - 0.5 * unit).abs() < 1e-6, "-6 dB halves it: {l6}");
    let (lw, rw) = echo(Db::UNITY, 0.0);
    assert!(
        lw == rw && (lw - 0.5 * unit).abs() < 1e-6,
        "mono: {lw} {rw}"
    );
    let (silent, _) = echo(Db::new(f64::NEG_INFINITY), 1.0);
    assert_eq!(silent, 0.0);
}

/// What is heard does not depend on how the stream is cut into calls: a new target at a chunk's
/// start and one inside a chunk each give the same bits whether the input goes in whole, in
/// calls of a frame, a chunk, a chunk and one, or a prime, because the chunks are counted
/// from the last reset and not from a call's edge.
#[test]
fn any_split_of_the_stream_gives_the_same_bits() {
    let l = layout(8, FreezeMode::Freezable);
    let a = controls(|_| {});
    let b = controls(|c| {
        c.decay = [Seconds::new(4.0), Seconds::new(5.0), Seconds::new(2.0)];
        c.size = Mix::new(0.8);
        c.diffusion = Mix::new(0.3);
        c.pre_delay = Seconds::new(0.01);
        c.rate = Hz::new(3.0);
        c.width = StereoWidth::new(0.5);
        c.wet = Db::new(-6.0);
    });
    let x = noise(7, 6_000);
    for at in [1_024, 1_000] {
        let render = |block: usize| {
            let (mut bank, mut ramps) = at_rest(&l, RATE, &a);
            let mut out = run(&mut bank, &mut ramps, &x[..at], block);
            bank.retarget(&mut ramps, &b);
            let tail = run(&mut bank, &mut ramps, &x[at..], block);
            for (o, t) in out.iter_mut().zip(tail) {
                o.extend(t);
            }
            out
        };
        let whole = render(x.len());
        assert!(whole[0].iter().any(|&s| s != 0.0), "something is heard");
        for block in [1, 7, 31, 32, 33, 100, 1_000] {
            assert_eq!(render(block), whole, "calls of {block}, a retarget at {at}");
        }
    }
}

/// A reset bank is a fresh one: after a noise burst and a reset, an impulse renders the same
/// bits as it does on a bank that never ran (the modulation's phases and the chunk count
/// start over with the lines).
#[test]
fn a_reset_bank_renders_as_a_fresh_one() {
    let l = layout(8, FreezeMode::Decaying);
    let c = controls(|c| c.modulation = Bipolar::of(0.8));
    let (mut used, mut used_ramps) = at_rest(&l, RATE, &c);
    run(&mut used, &mut used_ramps, &noise(3, 5_001), 64);
    used.reset();
    used_ramps.jump_to_target();
    let (mut fresh, mut fresh_ramps) = at_rest(&l, RATE, &c);
    let x = impulse(Samples::new(12_000));
    let after = run(&mut used, &mut used_ramps, &x, 64);
    assert!(after[0].iter().any(|&s| s != 0.0));
    assert_eq!(after, run(&mut fresh, &mut fresh_ramps, &x, 64));
}

/// A clone is its own bank holding the lines, chunk count and ramps as they were: both
/// continue to the same bits, and one running on does not move the other.
#[test]
fn a_clone_continues_where_the_original_was_and_shares_nothing() {
    let l = layout(8, FreezeMode::Decaying);
    let c = controls(|_| {});
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
    let x = noise(9, 4_000);
    run(&mut bank, &mut ramps, &x[..1_777], 64);
    bank.retarget(&mut ramps, &controls(|c| c.decay = [Seconds::new(0.7); 3]));
    run(&mut bank, &mut ramps, &x[1_777..2_000], 64);
    let (mut twin, mut twin_ramps) = (bank.clone(), ramps.clone());
    let rest = run(&mut bank, &mut ramps, &x[2_000..], 64);
    assert!(rest[0].iter().any(|&s| s != 0.0));
    assert_eq!(rest, run(&mut twin, &mut twin_ramps, &x[2_000..], 64));
}

/// Before its first `retarget` a bank renders silence, and the channels of an input are averaged to
/// one: a channel and its negation cancel, and two copies of a signal render as the signal.
#[test]
fn a_bank_is_silent_until_its_target_is_set_and_averages_its_input() {
    let l = layout(8, FreezeMode::Decaying);
    let mut bank = FdnBank::<f32>::new(&l, RATE);
    let mut ramps = bank.ramps();
    let before = run(&mut bank, &mut ramps, &noise(2, 256), 64);
    assert!(before.iter().flatten().all(|&s| s == 0.0));

    let c = still(|_| {});
    let y = noise(4, 4_096);
    let negated: Vec<f32> = y.iter().map(|s| -s).collect();
    let render = |channels: &[&[f32]]| {
        let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
        let (mut left, mut right) = (vec![0.0f32; y.len()], vec![0.0f32; y.len()]);
        bank.process(
            &mut ramps,
            &mut InputOutput::paired(
                Planes::new(channels).unwrap(),
                PlanesMut::new(&mut [&mut left, &mut right]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        [left, right]
    };
    let cancelled = render(&[&y, &negated]);
    assert!(cancelled.iter().flatten().all(|&s| s == 0.0));
    let one = render(&[&y]);
    assert!(one[0].iter().any(|&s| s != 0.0));
    assert_eq!(render(&[&y, &y]), one);
}

/// The tail a bank declares is its ceiling: the longest decay's network tail, unbounded for
/// a layout that can freeze; the tail of controls is the
/// network's own, no longer than the ceiling, and at least the decay in every band (the time
/// to fall 60 dB).
#[test]
fn the_tail_is_the_networks_under_the_ceiling() {
    let plain = FdnBank::<f32>::new(&layout(8, FreezeMode::Decaying), RATE);
    let freezable = FdnBank::<f32>::new(&layout(8, FreezeMode::Freezable), RATE);
    assert_eq!(freezable.tail(), Tail::Unbounded);
    let Tail::Finite(ceiling) = plain.tail() else {
        panic!("a finite ceiling")
    };
    assert!(ceiling.get() as f64 >= 8.0 * RATE.get(), "{ceiling:?}");
    let (mut plain, mut freezable) = (plain, freezable);
    let (ramps_p, ramps_f) = (plain.ramps(), freezable.ramps());
    for rt60 in [0.3, 1.0, 2.5] {
        let c = still(|c| c.decay = [Seconds::new(rt60); 3]);
        let Tail::Finite(t) = plain.tail_of(&ramps_p, &c) else {
            panic!("finite")
        };
        assert!(
            t <= ceiling && t.get() as f64 >= rt60 * RATE.get(),
            "{t:?} at {rt60}"
        );
        assert_eq!(freezable.tail_of(&ramps_f, &c), plain.tail_of(&ramps_p, &c));
    }
    assert_eq!(
        freezable.tail_of(&ramps_f, &controls(|c| c.freeze = true)),
        Tail::Unbounded
    );
    assert!(matches!(
        plain.tail_of(&ramps_p, &controls(|c| c.freeze = true)),
        Tail::Finite(_)
    ));
}

/// The frames of the longest line of `layout` at `rate` (size 1, the table's longest entry
/// times one and a half), the period a burst has to outlast to fill every line.
fn longest_line(rate: SampleRate) -> usize {
    (5393.0 * 1.5 * rate.get() / 44_100.0).ceil() as usize
}

/// `x` (the input, then silence to `frames`) through `bank` at rest on `c`: the two channels.
fn rendered_tail(
    bank: &mut FdnBank<f32>,
    ramps: &mut FdnRamps<f32>,
    input: &[f32],
    frames: usize,
) -> [Vec<f32>; 2] {
    bank.reset();
    run(bank, ramps, &tail_probe::padded(input, frames), 512)
}

/// The reported tail covers the ring-out of the worst finite inputs: for the shortest, the
/// default and the longest decay, each band alone the longest, both ends of size, diffusion and
/// modulation, the longest pre-delay, and 8 and 16 lines, at 11.025 kHz (the default also at 48
/// kHz and 44.1 kHz), an impulse, a full-scale noise burst and a full-scale step (four times the
/// longest line, so every line is full) stop, and from the reported tail past the last input
/// frame no frame of either channel is above `TAIL_FLOOR` of the render's own peak (see
/// [`tail_probe`]). The floor is relative to the response's peak: the wet signal is the lines'
/// own output, scaled by the design, and the step is built up by the loop far past the input.
#[test]
fn the_reported_tail_covers_the_ring_out_of_the_worst_inputs() {
    let two = Seconds::new(2.0);
    // (name, lines, the layout's longest decay, controls, rates): the longest decay of a
    // layout is the one its controls reach, so most run at 2 s and one at the default layout's 8 s.
    // The whole grid runs at 11.025 kHz (the tail in frames scales with the rate; the
    // crossovers at 4 kHz stay under Nyquist); the default also runs at 48 kHz and 44.1 kHz.
    let rates = [SampleRate::of(11_025.0)];
    let one = rates;
    let slow = rates;
    let cases: Vec<Case> = vec![
        (
            "default",
            8,
            Seconds::new(8.0),
            FdnControls::DEFAULT,
            &rates,
        ),
        (
            "shortest",
            8,
            two,
            controls(|c| {
                c.decay = [Seconds::new(0.05); 3];
                c.size = Mix::new(0.0);
            }),
            &rates,
        ),
        (
            "longest",
            8,
            two,
            controls(|c| {
                c.decay = [Seconds::new(2.0); 3];
                c.size = Mix::new(1.0);
                c.pre_delay = MAX_PRE_DELAY;
            }),
            &rates,
        ),
        (
            "longest, smallest, no modulation",
            8,
            two,
            still(|c| {
                c.decay = [Seconds::new(2.0); 3];
                c.size = Mix::new(0.0);
            }),
            &one,
        ),
        (
            "longest, deepest fastest modulation",
            8,
            two,
            controls(|c| {
                c.decay = [Seconds::new(2.0); 3];
                c.modulation = Bipolar::of(1.0);
                c.rate = Hz::new(5.0);
            }),
            &one,
        ),
        (
            "mid and low bands long",
            8,
            two,
            controls(|c| c.decay = [Seconds::new(2.0), Seconds::new(2.0), Seconds::new(0.05)]),
            &one,
        ),
        (
            "only the low band long",
            8,
            two,
            controls(|c| c.decay = [Seconds::new(0.05), Seconds::new(2.0), Seconds::new(0.05)]),
            &one,
        ),
        (
            "only the high band long",
            8,
            two,
            controls(|c| c.decay = [Seconds::new(0.05), Seconds::new(0.05), Seconds::new(2.0)]),
            &one,
        ),
        (
            "shortest, longest pre-delay",
            8,
            two,
            controls(|c| {
                c.decay = [Seconds::new(0.05); 3];
                c.pre_delay = MAX_PRE_DELAY;
            }),
            &one,
        ),
        (
            "16 lines, longest, parallel combs",
            16,
            two,
            controls(|c| {
                c.decay = [Seconds::new(2.0); 3];
                c.size = Mix::new(1.0);
                c.diffusion = Mix::new(0.0);
            }),
            &rates,
        ),
        (
            "8 s",
            8,
            Seconds::new(8.0),
            controls(|c| {
                c.decay = [Seconds::new(8.0); 3];
                c.size = Mix::new(1.0);
            }),
            &slow,
        ),
    ];
    let wide = [SampleRate::SR_48K, SampleRate::SR_44K1];
    let cases = cases
        .into_iter()
        .chain([(
            "default",
            8,
            Seconds::new(8.0),
            FdnControls::DEFAULT,
            &wide[..],
        )])
        .collect();
    assert_tails_cover(cases);
}

/// One case of the ring-out tests: its name, the line count, the layout's longest decay, the
/// controls and the rates it runs at.
type Case<'a> = (&'a str, usize, Seconds, FdnControls, &'a [SampleRate]);

/// Every case's reported tail covers the ring-out of an impulse, a full-scale noise burst and a
/// full-scale step (see [`tail_probe`]).
fn assert_tails_cover(cases: Vec<Case>) {
    for (name, lines, max, c, rates) in cases {
        let layout =
            FdnLayout::new(lines, MaxDecay::new(max).unwrap(), FreezeMode::Decaying).unwrap();
        for &rate in rates {
            let period = longest_line(rate);
            let (mut bank, mut ramps) = at_rest(&layout, rate, &c);
            let Tail::Finite(tail) = bank.tail_of(&ramps, &c) else {
                panic!("{name}: a finite tail")
            };
            for (input_name, input) in tail_probe::stimuli::<f32>(4 * period) {
                let frames = tail_probe::run_length(input.len(), tail.get(), period);
                let y = rendered_tail(&mut bank, &mut ramps, &input, frames);
                let what = format!("{rate:?} {name}, {input_name}");
                tail_probe::assert_tail_covers(&what, input.len(), tail.get(), &[&y[0], &y[1]]);
            }
        }
    }
}

/// The reported tail covers the ring-out where the echoes of several lines coincide in one
/// output channel and sum, and the output's own peak, which the floor is under, can be a
/// single echo: no diffusion at 8 s (parallel combs, the smallest room, no modulation), two
/// lines at the longest setting, and only the low band long at 8 s. Each rang past the tail
/// by up to 3.75 dB over the floor while the bound covered the state alone.
#[test]
fn the_reported_tail_covers_the_ring_out_where_the_echoes_of_lines_sum() {
    let (two, eight) = (Seconds::new(2.0), Seconds::new(8.0));
    let slow = [SampleRate::of(22_050.0)];
    let one = [SampleRate::SR_48K];
    assert_tails_cover(vec![
        (
            "no diffusion, smallest, 8 s",
            8,
            eight,
            still(|c| {
                c.decay = [eight; 3];
                c.size = Mix::new(0.0);
                c.diffusion = Mix::new(0.0);
            }),
            &slow,
        ),
        (
            "two lines, longest",
            2,
            two,
            controls(|c| {
                c.decay = [two; 3];
                c.size = Mix::new(1.0);
            }),
            &one,
        ),
        (
            "only the low band long, 8 s",
            8,
            eight,
            controls(|c| c.decay = [Seconds::new(0.05), eight, Seconds::new(0.05)]),
            &slow,
        ),
    ]);
}

/// A band profile that is not monotone (the mid band long over a short low band, or a short
/// high band) has a response between the crossovers above every nominal band gain, which the
/// bank scales to peak at 0.9999 where it would exceed 1: the loop gain per trip is near 1
/// at some frequency whatever the bands say. The reported tail reads the absorption's peak,
/// not its three nominal points, so the finite ones cover the ring-out and the others are
/// unbounded.
#[test]
fn the_reported_tail_covers_the_ring_out_of_a_band_profile_that_is_not_monotone() {
    let (low, two) = (Seconds::new(0.05), Seconds::new(2.0));
    let slow = [SampleRate::of(22_050.0)];
    assert_tails_cover(vec![
        (
            "mid 2 s, low and high short",
            8,
            two,
            controls(|c| c.decay = [two, low, low]),
            &slow,
        ),
        (
            "mid and high 2 s, low 0.3 s",
            8,
            two,
            controls(|c| c.decay = [two, Seconds::new(0.3), two]),
            &[SampleRate::SR_48K],
        ),
        (
            "mid 1 s, low and high short",
            8,
            two,
            controls(|c| c.decay = [Seconds::new(1.0), low, low]),
            &[SampleRate::SR_48K],
        ),
    ]);
}

/// The settings whose peak response per trip is at or near 1 report an unbounded tail, and it
/// is warranted: the render of a noise burst or a step is still above the floor ten seconds
/// after the last input frame (the floor under the render's own peak; at 0.9999 per trip the
/// true ring-out is hours).
#[test]
fn an_unbounded_tail_of_a_band_profile_that_is_not_monotone_is_warranted() {
    let (low, two, eight) = (Seconds::new(0.05), Seconds::new(2.0), Seconds::new(8.0));
    let rate = SampleRate::SR_48K;
    let cases: Vec<(&str, Seconds, FdnControls)> = vec![
        ("[2, 0.05, 2]", two, controls(|c| c.decay = [two, low, two])),
        (
            "[8, 0.05, 0.05], size 0.5",
            eight,
            controls(|c| c.decay = [eight, low, low]),
        ),
        (
            "[8, 0.05, 8]",
            eight,
            controls(|c| c.decay = [eight, low, eight]),
        ),
    ];
    for (name, max, c) in cases {
        let layout = FdnLayout::new(8, MaxDecay::new(max).unwrap(), FreezeMode::Decaying).unwrap();
        let period = longest_line(rate);
        let (mut bank, mut ramps) = at_rest(&layout, rate, &c);
        assert_eq!(bank.tail_of(&ramps, &c), Tail::Unbounded, "{name}");
        let ten = 10 * rate.get() as usize;
        let rings = tail_probe::stimuli::<f32>(4 * period).map(|(input_name, input)| {
            let frames = input.len() + ten + period;
            let y = rendered_tail(&mut bank, &mut ramps, &input, frames);
            let what = format!("{name}, {input_name}");
            (
                input_name,
                tail_probe::rings_past(&what, input.len(), ten, &[&y[0], &y[1]]),
            )
        });
        assert!(
            rings.iter().any(|(_, rings)| *rings),
            "{name}: no input rings past ten seconds: {rings:?}"
        );
    }
}

/// `layout` at `short`, rung by each stimulus and then moved to `long` a third of the way into
/// the short tail: the channels the bank wrote, the frames of input and the tail a node reports
/// after the move (what is owed and the long tail, from the last input frame, under the ceiling).
fn raised<'a>(
    layout: &'a FdnLayout,
    rate: SampleRate,
    short: &'a FdnControls,
    long: &'a FdnControls,
    period: usize,
) -> impl Iterator<Item = (String, usize, usize, [Vec<f32>; 2], [Vec<f32>; 2])> + 'a {
    let (mut bank, mut ramps) = at_rest(layout, rate, short);
    let (Tail::Finite(was), Tail::Finite(now), Tail::Finite(ceiling)) = (
        bank.tail_of(&ramps, short),
        bank.tail_of(&ramps, long),
        bank.tail(),
    ) else {
        panic!("finite tails")
    };
    [0, was.get() / 3]
        .into_iter()
        .flat_map(move |at| {
            tail_probe::stimuli::<f32>(4 * period)
                .into_iter()
                .map(move |s| (at, s))
        })
        .map(move |(moved_at, (name, input))| {
            bank.reset();
            bank.retarget(&mut ramps, short);
            ramps.jump_to_target();
            let head = input.len() + moved_at;
            let reported = tail_probe::raised_tail(was.get() - moved_at, now.get(), ceiling.get());
            let first = run(
                &mut bank,
                &mut ramps,
                &tail_probe::padded(&input, head),
                512,
            );
            bank.retarget(&mut ramps, long);
            let rest = tail_probe::run_length(input.len(), reported, period) - head;
            let second = run(&mut bank, &mut ramps, &vec![0.0; rest], 512);
            (
                format!("{name}, {moved_at} frames after the input"),
                input.len(),
                reported,
                first,
                second,
            )
        })
}

/// A decay raised to the layout's longest while the short one rings out is covered by the tail a
/// node reports after the move (the larger of what is owed and the long tail, from the last input
/// frame, under the ceiling): from it past the last input frame no frame of either channel is
/// above `TAIL_FLOOR` of the render's own peak, after an impulse, a noise burst and a step, the
/// move landing right after the input and a third of the way into the short tail. The energy in
/// the lines at the move decays at the new, slower rate from the move on.
#[test]
fn a_decay_raised_during_the_ring_out_is_covered_by_the_tail_from_the_last_input() {
    let l = FdnLayout::new(
        8,
        MaxDecay::new(Seconds::new(2.0)).unwrap(),
        FreezeMode::Decaying,
    )
    .unwrap();
    let short = controls(|c| c.decay = [Seconds::new(0.3); 3]);
    let long = controls(|c| c.decay = [Seconds::new(2.0); 3]);
    for (name, input, reported, first, second) in
        raised(&l, RATE, &short, &long, longest_line(RATE))
    {
        tail_probe::assert_raised_tail_covers(
            &format!("{name}, decay 0.3 s to 2 s"),
            input,
            reported,
            &[&first[0], &first[1]],
            &[&second[0], &second[1]],
        );
    }
}

/// A freezable layout with `freeze` on reports `Tail::Unbounded` (its loop is lossless and its
/// input shut: nothing ever falls to the floor), both for the setting and as the ceiling it
/// declares; the same layout with `freeze` off, and a layout that is not freezable with it on,
/// report a finite tail.
#[test]
fn a_frozen_network_reports_an_unbounded_tail() {
    for rate in [SampleRate::SR_48K, SampleRate::SR_44K1] {
        let mut freezable = FdnBank::<f32>::new(&layout(8, FreezeMode::Freezable), rate);
        let mut decaying = FdnBank::<f32>::new(&layout(8, FreezeMode::Decaying), rate);
        let (ramps_f, ramps_d) = (freezable.ramps(), decaying.ramps());
        let frozen = controls(|c| c.freeze = true);
        assert_eq!(freezable.tail_of(&ramps_f, &frozen), Tail::Unbounded);
        assert_eq!(freezable.tail(), Tail::Unbounded, "the ceiling");
        assert!(matches!(
            freezable.tail_of(&ramps_f, &FdnControls::DEFAULT),
            Tail::Finite(_)
        ));
        assert!(matches!(
            decaying.tail_of(&ramps_d, &frozen),
            Tail::Finite(_)
        ));
    }
}

/// A size that is still moving keeps the decay it was designed with. The gains are designed
/// for the length a line is moving to, `g = 10^(-3 L / (fs T))` a trip of `L` frames; while the
/// line is `L'` frames long each band's gain runs raised to `L' / L`, which is the gain that
/// loses 60 dB in the same `T` over `L'` frames, `g^(L'/L) = 10^(-3 L' / (fs T))`. After every
/// chunk of a size drop from 1 to 0 (lines 3 times too long at first, and in flight for about
/// 1.5 s to 7 s depending on the line), the gain each line runs in each band is read back from
/// the chunk's constants (low is the sum of the three terms, mid the first two, high the
/// first), and the decay `60 L' / (fs * -20 log10 g)` it gives over the line's current length
/// is the band's designed RT60 (3 s low, 1.5 s mid, 0.6 s high) to 0.1% (15% in the 20 ms the
/// gains ramp between the two designs). Without the exponent a line three times too long
/// would decay three times too slowly.
///
/// The impulse response is not the measurement: a read between two samples is a low-pass,
/// which costs a trip's high frequencies as much as any gain does.
#[test]
fn a_size_on_the_move_keeps_its_rt60_in_every_band() {
    let l = layout(8, FreezeMode::Decaying);
    let bands = |size: f64| {
        still(|c| {
            c.decay = [Seconds::new(1.5), Seconds::new(3.0), Seconds::new(0.6)];
            c.size = Mix::new(size);
        })
    };
    let (mut bank, mut ramps) = at_rest(&l, RATE, &bands(1.0));
    bank.retarget(&mut ramps, &bands(0.0));
    let designed = [3.0, 1.5, 0.6];
    let Steps::Fixed(ramp_chunks) = ramp(RATE) else {
        panic!("a fixed ramp")
    };
    let ramp_chunks = ramp_chunks as usize;
    let mut in_flight = 0;
    for chunk in 0..16_000 {
        run(
            &mut bank,
            &mut ramps,
            &[0.0; FdnBank::<f32>::CHUNK],
            FdnBank::<f32>::CHUNK,
        );
        for j in 0..ramps.n {
            // The length the chunk ran with, not where the ramp has got to since.
            let (now, to) = (bank.state.c_len[j], ramps.len[j].target());
            let [high, mid_over, low_over] = bank.state.c_gain[j].map(f64::from);
            let gains = [high + mid_over + low_over, high + mid_over, high];
            // While the gains ramp (a blend of two designs for two lengths) the exponent is
            // the blend's, which is linear in the gain and not in its logarithm: a few percent off; after it, exact.
            let tol = if chunk < ramp_chunks { 0.15 } else { 1e-3 };
            for (g, t) in gains.iter().zip(designed) {
                let rt60 = 60.0 * now.span().get() / (RATE.get() * -20.0 * g.log10());
                assert!(
                    (rt60 - t).abs() < tol * t,
                    "chunk {chunk}, line {j} at {now:?} of {to:?}: {rt60} s against {t}"
                );
            }
            in_flight += usize::from(now != to);
        }
    }
    assert!(
        in_flight > 8 * 2_000,
        "lines were in flight: {in_flight} line chunks"
    );
}

/// The pre-delay that has not yet fallen is owed too, up to the longest one: from 0.4 s down
/// to none, 19 200 frames more than the same controls at rest, and the tail of the longest
/// decay with the longest pre-delay in flight still fits the ceiling.
#[test]
fn the_tail_owes_the_pre_delay_still_to_fall() {
    let l = layout(8, FreezeMode::Decaying);
    let (mut bank, mut ramps) = at_rest(&l, RATE, &still(|c| c.pre_delay = Seconds::new(0.4)));
    bank.retarget(&mut ramps, &still(|_| {}));
    let Tail::Finite(owed) = bank.tail_of(&ramps, &still(|_| {})) else {
        panic!("finite")
    };
    let (mut fresh, fresh_ramps) = at_rest(&l, RATE, &still(|_| {}));
    let Tail::Finite(base) = fresh.tail_of(&fresh_ramps, &still(|_| {})) else {
        panic!("finite")
    };
    assert!(
        owed.get().abs_diff(base.get() + 19_200) <= 1,
        "{owed:?} against {base:?}"
    );
    let longest = still(|c| c.decay = [Seconds::new(8.0); 3]);
    let (mut worst, mut worst_ramps) = at_rest(
        &l,
        RATE,
        &still(|c| {
            c.decay = [Seconds::new(8.0); 3];
            c.pre_delay = crate::MAX_PRE_DELAY;
        }),
    );
    worst.retarget(&mut worst_ramps, &longest);
    let (Tail::Finite(t), Tail::Finite(ceiling)) =
        (worst.tail_of(&worst_ramps, &longest), worst.tail())
    else {
        panic!("finite")
    };
    assert!(t <= ceiling, "{t:?} over the ceiling {ceiling:?}");
}

/// A line moves toward its target at most half a frame a chunk and is there in exactly
/// `ceil(distance / 0.5)` chunks: a size that grows a line `delta` frames takes
/// `ceil(delta / 0.5)` steps of the ramp, one a chunk, and no step is longer than 0.5.
#[test]
fn a_length_moves_half_a_frame_a_chunk_and_lands_exactly() {
    let l = layout(8, FreezeMode::Decaying);
    let (a, b) = (
        still(|c| c.size = Mix::new(0.2)),
        still(|c| c.size = Mix::new(0.3)),
    );
    let (mut bank, mut ramps) = at_rest(&l, RATE, &a);
    let (from, to) = (
        ReadPosition::from_span(fdn_of(&design(&l, RATE, &a)).lines[3].length),
        ReadPosition::from_span(fdn_of(&design(&l, RATE, &b)).lines[3].length),
    );
    let steps = ((to.span().get() - from.span().get()) / 0.5).ceil() as usize;
    assert!(steps > 100, "a long way: {steps} chunks");
    bank.retarget(&mut ramps, &b);
    let mut last = from;
    for chunk in 1..=steps {
        run(
            &mut bank,
            &mut ramps,
            &[0.0; FdnBank::<f32>::CHUNK],
            FdnBank::<f32>::CHUNK,
        );
        let now = ramps.len[3].now();
        let moved = now.span().get() - last.span().get();
        assert!(moved <= 0.5 + 1e-3, "chunk {chunk}: moved {moved}");
        assert!(now > last);
        last = now;
        assert_eq!(
            now == to,
            chunk == steps,
            "lands in {steps} chunks, not {chunk}"
        );
    }
}

/// The pre-delay moves at most 2 frames a chunk and lands exactly: 0.1 s at 48 kHz is 4800
/// frames, 2400 chunks.
#[test]
fn the_pre_delay_moves_two_frames_a_chunk_and_lands_exactly() {
    let l = layout(8, FreezeMode::Decaying);
    let (mut bank, mut ramps) = at_rest(&l, RATE, &still(|_| {}));
    bank.retarget(&mut ramps, &still(|c| c.pre_delay = Seconds::new(0.1)));
    let mut last = ReadPosition::START;
    for chunk in 1..=2_400 {
        run(
            &mut bank,
            &mut ramps,
            &[0.0; FdnBank::<f32>::CHUNK],
            FdnBank::<f32>::CHUNK,
        );
        let now = ramps.pre_len.now();
        let moved = now.span().get() - last.span().get();
        assert!(moved <= 2.0 + 1e-3, "chunk {chunk}: moved {moved}");
        last = now;
        assert_eq!(
            now == ReadPosition::from_frames(Samples::new(4_800)),
            chunk == 2_400,
            "chunk {chunk}: {now:?}"
        );
    }
}

/// A gain moves over 20 ms of whole chunks and lands exactly: at 48 kHz 960 frames is 30
/// chunks, so the wet gain goes from 1 to a half in 30 steps and not 29, and the ramp is
/// 30 steps.
#[test]
fn a_gain_ramps_over_twenty_milliseconds_of_chunks_and_lands_exactly() {
    let wanted = (0.020 * RATE.get() / FdnBank::<f32>::CHUNK as f64).ceil() as u32;
    assert_eq!(wanted, 30);
    assert_eq!(ramp(RATE), Steps::Fixed(wanted));
    let l = layout(8, FreezeMode::Decaying);
    let (mut bank, mut ramps) = at_rest(&l, RATE, &still(|_| {}));
    bank.retarget(&mut ramps, &still(|c| c.wet = Db::new(-6.0206)));
    let target = ramps.wet.target();
    assert!((target - 0.5).abs() < 1e-4);
    for chunk in 1..=wanted {
        run(
            &mut bank,
            &mut ramps,
            &[0.0; FdnBank::<f32>::CHUNK],
            FdnBank::<f32>::CHUNK,
        );
        assert_eq!(ramps.wet.now() == target, chunk == wanted, "chunk {chunk}");
    }
}

/// Every row of the lines moves on each frame, those past the lines in use too: a line idle
/// for `m` frames reads, at delay `m + 1`, the value it held one frame back when it went idle,
/// and the same after it is regrown. A cursor that stood still while idle reads another slot.
#[test]
fn a_line_idle_while_the_line_count_shrinks_ages_with_the_clock() {
    let c = still(|c| c.pre_delay = Seconds::new(0.0));
    let (mut bank, mut ramps) = at_rest(&layout(8, FreezeMode::Decaying), RATE, &c);
    run(&mut bank, &mut ramps, &impulse(Samples::new(1)), 1);
    let held = bank.state.lines[7].back(1);
    assert_ne!(held, 0.0, "the impulse reached the last line");

    let m = 100;
    bank.layout = layout(4, FreezeMode::Decaying);
    bank.designed = None;
    bank.retarget(&mut ramps, &c);
    run(&mut bank, &mut ramps, &vec![0.0; m], 7);
    assert_eq!(bank.state.lines[7].back(m + 1), held, "idle for {m} frames");

    bank.layout = layout(8, FreezeMode::Decaying);
    bank.designed = None;
    bank.retarget(&mut ramps, &c);
    run(&mut bank, &mut ramps, &vec![0.0; m], 7);
    assert_eq!(bank.state.lines[7].back(2 * m + 1), held, "regrown");
}

/// An `f64` FDN with modulation off is linear: `R(a x + b y) = a R(x) + b R(y)` to 2^-40 of
/// the peak, with `a`, `b` and two wide signals that an `f32` state cannot carry (the
/// interpolated reads, the mix and the absorption's one-poles all run in the bank's float).
#[test]
fn an_f64_fdn_is_linear() {
    use tutti_fixtures::signal::wide_signal;
    let len = 8_000;
    let x = wide_signal(len);
    let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
    let (a, b) = (1.0 / 3.0, -std::f64::consts::PI / 7.0);
    let mix: Vec<f64> = x.iter().zip(&y).map(|(x, y)| a * x + b * y).collect();
    let c = still(|c| c.diffusion = Mix::new(0.6));
    let l = layout(8, FreezeMode::Decaying);
    let render = |input: &[f64]| {
        let mut bank = FdnBank::<f64>::new(&l, RATE);
        let mut ramps = bank.ramps();
        bank.retarget(&mut ramps, &c);
        ramps.jump_to_target();
        let (mut l, mut r) = (vec![0.0f64; len], vec![0.0f64; len]);
        bank.process(
            &mut ramps,
            &mut InputOutput::paired(
                Planes::new(&[input]).unwrap(),
                PlanesMut::new(&mut [&mut l[..], &mut r[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        [l, r]
    };
    let (rx, ry, rm) = (render(&x), render(&y), render(&mix));
    for ch in 0..2 {
        let peak = rm[ch].iter().fold(0.0f64, |p, v| p.max(v.abs()));
        assert!(peak > 1e-4, "the network sounds");
        for n in 0..len {
            let want = a * rx[ch][n] + b * ry[ch][n];
            assert!(
                (rm[ch][n] - want).abs() <= peak * 2.0f64.powi(-40),
                "channel {ch}, frame {n}: {} against {want}",
                rm[ch][n]
            );
        }
    }
}

/// The absorption one-poles store their complements `c = 1 - exp(-2 pi fc / fs)`, designed in
/// `f64`: a low crossover at 192 kHz has `c` of 3.3e-3 at 100 Hz, where the pole rounded to `f32`
/// first and `1 - p` formed there is off by up to `u / (2 c)` of `c` (9e-6), and the complement is
/// within `u = 2^-24`.
#[test]
fn the_absorption_complements_keep_their_relative_precision_where_one_minus_the_pole_does_not() {
    let rate = SampleRate::SR_192K;
    let u = f64::from(f32::EPSILON) / 2.0;
    let (mut stored, mut formed) = (0.0f64, 0.0f64);
    for hz in (100..=400).map(f64::from) {
        let mut target = Target::<f32>::new(2);
        set_poles(&mut target, [Hz::new(hz), Hz::new(4_000.0)], rate);
        let exact = -libm::expm1(-std::f64::consts::TAU * hz / rate.get());
        stored = stored.max((f64::from(target.complement[0]) / exact - 1.0).abs());
        let pole = target.pole[0] as f32;
        formed = formed.max((f64::from(1.0f32 - pole) / exact - 1.0).abs());
    }
    assert!(
        stored <= u,
        "the complement's error {stored:e} against u = {u:e}"
    );
    assert!(
        formed > 100.0 * u,
        "`1 - p` in f32 is off by {formed:e}, not clearly worse than {u:e}"
    );
}

/// The longest decay (`MaxDecay::CAP`, 60 s) at the highest rate in `f32`, on the shortest line:
/// two parallel combs at room size 0 and no modulation, where line 0 is `L = 2244` frames
/// (11.7 ms at 192 kHz) and its echoes are single samples, `a g^j` with the loop gain `g =
/// 10^(-3 L / (fs T))` of the design, `T = 60 s`. The measured RT60 is `-3 L / (fs log10 g)`
/// from the peaks of the echoes `1` and `K - 1` (`K` the trips in `T`).
///
/// The bound, in `ln g` a trip: `g` rounds once to `f32` (at most `u = 2^-24` relative) and the
/// loop's one product rounds (at most `u`) each trip, `2 u` coherent; the two compared echoes
/// each carry the roundings of their path out (the output sums and gains, at most `6 u`), a
/// fixed `12 u` over the `K - 2` trips between them. The RT60's relative error is that over
/// `|ln g| = 1.35e-3`: the loop gain stays a gain because it multiplies a line's sample, which
/// rounds by `u` a trip whatever the form, so a complement stores nothing better.
#[test]
fn the_longest_decay_at_192_khz_on_the_shortest_line_holds_its_closed_form_rt60_in_f32() {
    let rate = SampleRate::SR_192K;
    let decay = MaxDecay::new(MaxDecay::CAP).unwrap();
    let l = FdnLayout::new(2, decay, FreezeMode::Decaying).unwrap();
    let c = still(|c| {
        c.decay = [decay.get(); 3];
        c.size = Mix::new(0.0);
        c.diffusion = Mix::new(0.0);
    });
    let frames_per_line = fdn_of(&design(&l, rate, &c)).lines[0].length.get() as usize;
    let trips = (decay.get().get() * rate.get() / frames_per_line as f64) as usize - 2;
    let (mut bank, mut ramps) = at_rest(&l, rate, &c);
    let [left, _] = run(
        &mut bank,
        &mut ramps,
        &impulse(Samples::new((trips + 2) * frames_per_line)),
        4096,
    );

    let peak = |j: usize| {
        left[j * frames_per_line - frames_per_line / 2..j * frames_per_line + frames_per_line / 2]
            .iter()
            .fold(0.0f64, |m, &s| m.max(f64::from(s).abs()))
    };
    let between = (trips - 1) as f64;
    let g_measured = libm::pow(peak(trips) / peak(1), 1.0 / between);
    let measured = -3.0 * frames_per_line as f64 / (rate.get() * g_measured.log10());

    let u = f64::from(f32::EPSILON) / 2.0;
    let per_trip = 2.0 * u + 12.0 * u / between;
    let ln_g = -libm::log(10.0) * 3.0 * frames_per_line as f64 / (decay.get().get() * rate.get());
    let bound = per_trip / ln_g.abs();
    let error = (measured / decay.get().get() - 1.0).abs();
    assert!(
        error <= bound,
        "RT60 {measured} s against {} s: error {error:e}, bound {bound:e} ({trips} trips of {frames_per_line} frames)",
        decay.get().get()
    );
}

/// An FFT roundtrip's relative error in `f32` over the shifter's 2048-point window: each
/// transform grows rounding like `eps * log2(window)`, and a frame is one forward and one inverse.
fn shimmer_transform_error() -> f64 {
    2.0 * 2048.0f64.log2() * f64::from(f32::EPSILON)
}

fn shimmering(lines: usize) -> FdnLayout {
    FdnLayout::shimmering(lines, MaxDecay::DEFAULT).unwrap()
}

/// The first frame of `x` that is not silent.
fn first_sound(x: &[f32]) -> usize {
    x.iter().position(|&v| v != 0.0).expect("it sounds")
}

/// At no shift the shimmer returns the feedback it took, delayed by the chunk it was collected
/// in and the vocoder's latency (`delay`, the bank's own figure) and divided by the bound `B`.
/// A diagonal two-line network at one decay in every band (a gain `g_j` a trip, whatever the
/// frequency, `u = 1 / sqrt(2)`) takes an impulse `c_j` into line `j`, which comes round at
/// `t_j` (read off its first echo) as `f_j = g_j c_j`. So on the left ear (line 0, gain
/// `sqrt(2 / n) = 1`), with a send `k`:
///
/// - `2 t0`: line 0's own echo, less the share the shimmer takes: `g_0 c_0 (1 - k / 2)`;
/// - `t0 + t1`: line 1's echo reaches line 0 as the shimmer takes it: `-k g_1 c_1 / 2`;
/// - `2 t0 + D` and `t0 + t1 + D`: the shifted copies of those two, `k g_0 c_0 / (2 B)`
///   and `k g_1 c_1 / (2 B)`.
///
/// None of the times is any other sum of `t0`, `t1` and `D`, so each frame holds one echo, to
/// the `f32` transform's error (the rest is exactly the arithmetic of the loop in `f32`).
#[test]
fn at_zero_cents_the_echoes_are_the_feedback_and_its_delayed_copy() {
    let l = shimmering(2);
    let k = 0.5;
    let c = still(|c| {
        c.decay = [Seconds::new(2.0); 3];
        c.diffusion = Mix::new(0.0);
        c.pre_delay = Seconds::ZERO;
        c.width = StereoWidth::new(1.0);
        c.wet = Db::UNITY;
        c.shimmer_pitch = Cents::new(0.0);
        c.shimmer_send = Feedback::new_clamped(k);
    });
    let net = design(&l, RATE, &c);
    let f = fdn_of(&net);
    let g = |j: usize| f.lines[j].absorption.gain(Band::Low).get();
    let input = |j: usize| f.input[j].get();
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c);
    let d = bank.shimmer_delay().get();
    let frames = 16_000;
    let [left, right] = run(&mut bank, &mut ramps, &impulse(Samples::new(frames)), 64);
    let (t0, t1) = (first_sound(&left), first_sound(&right));
    assert_eq!(
        t0 as f64,
        f.lines[0].length.get(),
        "a line of 0 delay beyond its length"
    );
    let ear = f.output.rows[0][0].get();
    let b = super::super::shimmer::GAIN_BOUND;
    let want = [
        (t0, ear * input(0)),
        (2 * t0, ear * g(0) * input(0) * (1.0 - k / 2.0)),
        (t0 + t1, -ear * k * g(1) * input(1) / 2.0),
        (2 * t0 + d, ear * k * g(0) * input(0) / (2.0 * b)),
        (t0 + t1 + d, ear * k * g(1) * input(1) / (2.0 * b)),
    ];
    let tolerance = 4.0 * shimmer_transform_error();
    for (at, amplitude) in want {
        assert!(
            (f64::from(left[at]) - amplitude).abs() < tolerance,
            "frame {at}: {} for {amplitude}",
            left[at]
        );
    }
}

/// A send the control wrote at or past the stable bound is held at it, a non-finite one is
/// none, and a layout that does not shimmer takes none: the loop's gain is `1 - k (1 - 1 / B)`
/// at the largest `k`, below 1.
#[test]
fn a_send_is_held_under_the_stable_bound() {
    let b = super::super::shimmer::GAIN_BOUND;
    let target = |l: &FdnLayout, send: Feedback| {
        let c = controls(|c| c.shimmer_send = send);
        let (bank, _) = at_rest(l, RATE, &c);
        bank.target.shimmer
    };
    let held = Feedback::MAX_STABLE.get() as f32;
    assert_eq!(target(&shimmering(8), Feedback::new(5.0)), held);
    assert_eq!(target(&shimmering(8), Feedback::new(1.0)), held);
    assert_eq!(target(&shimmering(8), Feedback::new(f64::NAN)), 0.0);
    assert_eq!(target(&shimmering(8), Feedback::new(-1.0)), 0.0);
    assert_eq!(target(&shimmering(8), Feedback::new(0.5)), 0.5);
    assert_eq!(
        target(&layout(8, FreezeMode::Decaying), Feedback::new(0.9)),
        0.0
    );
    let gain = 1.0 - Feedback::MAX_STABLE.get() * (1.0 - 1.0 / b);
    assert!(gain < 1.0, "{gain}");
}

/// A shimmering network's tail is finite, holds the shifted path's delay in every line of the
/// model while the send is above zero (and not otherwise), and covers the ring-out of the loop
/// at +1200 cents and the largest send: the last frame of an impulse's response within 60 dB of
/// its peak is inside it.
#[test]
fn the_tail_of_a_shimmer_holds_the_shifters_delay_and_covers_the_ring_out() {
    let l = shimmering(8);
    let c = |send: f64| {
        still(|c| {
            c.decay = [Seconds::new(1.0); 3];
            c.shimmer_pitch = Cents::new(1200.0);
            c.shimmer_send = Feedback::new_clamped(send);
        })
    };
    let mut probe = FdnBank::<f32>::new(&l, RATE);
    let ramps = probe.ramps();
    let Tail::Finite(plain) = probe.tail_of(&ramps, &c(0.0)) else {
        panic!("finite")
    };
    assert!(
        fdn_of(&probe.network).loop_delay == SampleSpan::ZERO,
        "no send, no loop delay in the model"
    );
    let Tail::Finite(shimmered) = probe.tail_of(&ramps, &c(0.99)) else {
        panic!("finite")
    };
    let d = probe.shimmer_delay();
    assert!(
        fdn_of(&probe.network).loop_delay == SampleSpan::from_samples(d),
        "the loop holds the shifter's delay"
    );
    assert!(
        shimmered.get() >= plain.get() + d.get(),
        "{shimmered:?} past {plain:?} by the delay"
    );

    let (mut bank, mut ramps) = at_rest(&l, RATE, &c(0.99));
    let frames = shimmered.get() + 48_000;
    let [left, right] = run(&mut bank, &mut ramps, &impulse(Samples::new(frames)), 64);
    let peak = left
        .iter()
        .chain(&right)
        .fold(0.0f32, |p, v| p.max(v.abs()));
    let floor = peak * 1e-3;
    let last = (0..frames)
        .rev()
        .find(|&i| left[i].abs() > floor || right[i].abs() > floor)
        .expect("it rings");
    assert!(
        last < shimmered.get(),
        "rings to {last}, tail {shimmered:?}"
    );
    assert!(
        left[frames - 1000..]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= floor)
    );
}

/// The reported tail of a shimmering network covers the ring-out of the worst finite inputs: for
/// no send, half and the largest send, a shift of an octave down, none, a fifth and an octave up,
/// and decays of 0.3 s and 1 s, at 11.025 kHz (the send-0.99 octave also at 48 kHz and 44.1 kHz),
/// an impulse, a full-scale noise burst and a full-scale step (four times the longest line and
/// the shifter's delay, so every line and the shifter are full) stop, and from the reported tail
/// past the last input frame no frame of either channel is above `TAIL_FLOOR` of the render's own
/// peak (see [`tail_probe`]). The floor is relative to the response's peak: the shifted copies
/// add to the lines' own echoes, and a step is built up far past the input.
#[test]
fn the_shimmer_tail_covers_the_ring_out_of_the_worst_inputs() {
    let l = FdnLayout::shimmering(8, MaxDecay::new(Seconds::new(1.0)).unwrap()).unwrap();
    let cases = [
        // (send, pitch in cents, decay in seconds, also at 44.1 kHz)
        (0.0, 1200.0, 0.3, false),
        (0.5, 1200.0, 0.3, false),
        (0.99, 1200.0, 0.3, true),
        (0.99, -1200.0, 0.3, false),
        (0.99, 0.0, 0.3, false),
        (0.99, 700.0, 1.0, false),
    ];
    for (send, pitch, decay, both) in cases {
        // The whole grid runs at 11.025 kHz; the send-0.99 octave also runs at 48 kHz and 44.1 kHz.
        let mut rates = vec![SampleRate::of(11_025.0)];
        if both {
            rates.extend([SampleRate::SR_48K, SampleRate::SR_44K1]);
        }
        for rate in rates {
            let c = still(|c| {
                c.decay = [Seconds::new(decay); 3];
                c.shimmer_pitch = Cents::new(pitch);
                c.shimmer_send = Feedback::new_clamped(send);
            });
            let (mut bank, mut ramps) = at_rest(&l, rate, &c);
            let Tail::Finite(tail) = bank.tail_of(&ramps, &c) else {
                panic!("send {send}: a finite tail")
            };
            let period = longest_line(rate) + bank.shimmer_delay().get();
            for (name, input) in tail_probe::stimuli::<f32>(4 * period) {
                let frames = tail_probe::run_length(input.len(), tail.get(), period);
                let y = rendered_tail(&mut bank, &mut ramps, &input, frames);
                let what = format!("{rate:?} send {send} pitch {pitch} decay {decay}, {name}");
                tail_probe::assert_tail_covers(&what, input.len(), tail.get(), &[&y[0], &y[1]]);
            }
        }
    }
}

/// While the send ramps down to 0 (20 ms of chunks), the shifter's delay stays in the model's
/// loop: the tail reported at each block of the ramp is the one the send held, and only once
/// the ramp has reached 0 is it the plain network's. At every one of those blocks the tail
/// covers the measured ring-out of the bank from that block on: the last frame within 60 dB of
/// the impulse response's peak is inside it.
#[test]
fn the_tail_keeps_the_shifters_delay_until_the_send_ramp_reaches_zero() {
    let l = shimmering(8);
    let c = |send: f64| {
        still(|c| {
            c.decay = [Seconds::new(1.0); 3];
            c.shimmer_pitch = Cents::new(1200.0);
            c.shimmer_send = Feedback::new_clamped(send);
        })
    };
    let (mut bank, mut ramps) = at_rest(&l, RATE, &c(0.99));
    let mut x = impulse(Samples::new(4_000));
    x.fill(0.0);
    x[0] = 1.0;
    let [left, right] = run(&mut bank, &mut ramps, &x, 64);
    let peak = left
        .iter()
        .chain(&right)
        .fold(0.0f32, |p, v| p.max(v.abs()));
    let floor = peak * 1e-3;
    let held = bank.tail_of(&ramps, &c(0.99));
    let (mut probe, probe_ramps) = at_rest(&l, RATE, &c(0.0));
    let plain = probe.tail_of(&probe_ramps, &c(0.0));
    assert_ne!(held, plain, "the delay is in the tail while the send is on");
    bank.retarget(&mut ramps, &c(0.0));
    let silence = vec![0.0f32; 64];
    let mut ramping = 0;
    for block in 0..40 {
        let now = bank.tail_of(&ramps, &c(0.0));
        let at_zero = ramps.shimmer.now() == 0.0;
        assert_eq!(
            now,
            if at_zero { plain } else { held },
            "block {block}: the delay is dropped when the ramp has reached 0, not before"
        );
        ramping += usize::from(!at_zero);
        if !at_zero {
            let (mut twin, mut twin_ramps) = (bank.clone(), ramps.clone());
            let Tail::Finite(t) = now else {
                panic!("finite")
            };
            let frames = t.get() + 48_000;
            let [l2, r2] = run(&mut twin, &mut twin_ramps, &vec![0.0; frames], 64);
            let last = (0..frames)
                .rev()
                .find(|&i| l2[i].abs() > floor || r2[i].abs() > floor);
            assert!(
                last.is_none_or(|i| i < t.get()),
                "block {block}: rings to {last:?}, tail {t:?}"
            );
        }
        run(&mut bank, &mut ramps, &silence, 64);
    }
    assert!(ramping >= 10, "the ramp spans blocks: {ramping}");
}
