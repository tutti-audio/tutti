use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_types::{Db, SampleRate, Seconds};

use super::*;
use crate::testing::noise;

const RATE: SampleRate = SampleRate::SR_48K;
/// The rates the static and time-constant tests run at.
const RATES: [SampleRate; 3] = [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K];
/// The longest block `run` hands the bank.
const CHUNK: usize = 256;

fn controls(threshold: f32, attack: f64, hold: f64, release: f64, range: f32) -> GateControls {
    GateControls {
        threshold: Db(threshold),
        attack: Seconds(attack),
        hold: Seconds(hold),
        release: Seconds(release),
        range: Db(range),
        ..GateControls::DEFAULT
    }
}

/// A grid and seeded draws across every control.
fn reachable() -> Vec<GateControls> {
    let mut all = Vec::new();
    for threshold in [-60.0, -30.0, -12.0, 0.0] {
        for range in [-80.0, -24.0, -6.0, 0.0] {
            for (attack, hold, release) in [(0.0, 0.0, 0.0), (0.001, 0.02, 0.05), (0.01, 0.1, 0.3)]
            {
                all.push(controls(threshold, attack, hold, release, range));
            }
        }
    }
    let mut rng = StdRng::seed_from_u64(24);
    all.extend((0..64).map(|_| {
        controls(
            rng.random_range(-70.0..6.0),
            rng.random_range(0.0..0.05),
            rng.random_range(0.0..0.3),
            rng.random_range(0.0..0.5),
            rng.random_range(-90.0..0.0),
        )
    }));
    all
}

/// Below the floors (a time under 0), above the cap (a range over 0 dB) and not finite: each
/// is a control the bank can run.
fn hostile() -> Vec<GateControls> {
    let base = controls(-30.0, 0.003, 0.05, 0.08, -40.0);
    let mut all = vec![
        GateControls {
            attack: Seconds(-1.0),
            hold: Seconds(-1.0),
            release: Seconds(-1.0),
            ..base
        },
        GateControls {
            range: Db(12.0),
            ..base
        },
    ];
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        all.push(GateControls {
            threshold: Db(bad),
            range: Db(bad),
            ..base
        });
        all.push(GateControls {
            attack: Seconds(f64::from(bad)),
            hold: Seconds(f64::from(bad)),
            release: Seconds(f64::from(bad)),
            ..base
        });
    }
    all
}

/// A bank at `rate` at rest on `c`: aimed and landed, nothing gliding.
fn runner_at(c: &GateControls, rate: SampleRate) -> GateBank {
    let mut bank = GateBank::new(rate);
    bank.aim(c);
    bank.settle();
    bank
}

/// [`runner_at`] at 48 kHz.
fn runner(c: &GateControls) -> GateBank {
    runner_at(c, RATE)
}

/// `audio` (one slice per channel) through `bank`, keyed by `key`, with `lane` as the
/// threshold lane when given, in calls of at most `block` frames.
fn run_in(
    bank: &mut GateBank,
    audio: &[&[f32]],
    key: &[&[f32]],
    lane: Option<&[f32]>,
    block: usize,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let kk: Vec<&[f32]> = key.iter().map(|c| &c[at..end]).collect();
        let lanes = GateLanes {
            threshold: lane.map(|l| Mono::new(&l[at..end])),
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            lanes,
            Planes::new(&kk).unwrap(),
            Planes::new(&a).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        );
        at = end;
    }
    out
}

/// [`run_in`] in calls of [`CHUNK`] frames.
fn run(
    bank: &mut GateBank,
    audio: &[&[f32]],
    key: &[&[f32]],
    lane: Option<&[f32]>,
) -> Vec<Vec<f32>> {
    run_in(bank, audio, key, lane, CHUNK)
}

/// One channel of `audio` and of the key, as the stereo pair the tests run.
fn run_mono(bank: &mut GateBank, audio: &[f32], key: &[f32], lane: Option<&[f32]>) -> Vec<f32> {
    run(bank, &[audio, audio], &[key, key], lane).remove(0)
}

/// A level of `db` as an amplitude, from the definition.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// The design holds the figures its definition gives: the hold is `floor(hold * rate)`
/// frames, the times are one-pole coefficients at the rate, floored at 0 (instant), the range
/// is capped at 0 dB, and a non-finite control is the default's.
#[test]
fn the_design_holds_the_figures_of_its_definition() {
    let m = Design::of(&controls(-20.0, 0.001, 0.0102, 0.1, -12.0), RATE);
    // 0.0102 s at 48 kHz is 489.6 frames, floored.
    assert_eq!(m.hold_samples, 489);
    assert_eq!(m.threshold, Db(-20.0).to_amplitude());
    assert_eq!(m.attack, Seconds(0.001).one_pole_coeff(RATE));
    assert_eq!(m.release, Seconds(0.1).one_pole_coeff(RATE));
    assert_eq!(m.range, Db(-12.0));

    let m = Design::of(
        &GateControls {
            attack: Seconds(-1.0),
            hold: Seconds(-1.0),
            release: Seconds(-1.0),
            range: Db(6.0),
            ..GateControls::DEFAULT
        },
        RATE,
    );
    assert_eq!(
        (m.attack, m.release),
        (1.0, 1.0),
        "a time under 0 is instant"
    );
    assert_eq!(m.hold_samples, 0);
    assert_eq!(m.range, Db::UNITY, "a range over 0 dB reads as 0");

    let m = Design::of(
        &GateControls {
            threshold: Db(f32::NAN),
            range: Db(f32::INFINITY),
            ..GateControls::DEFAULT
        },
        RATE,
    );
    assert_eq!(m.threshold, GateControls::DEFAULT.threshold.to_amplitude());
    assert_eq!(m.range, GateControls::DEFAULT.range);
}

/// A key a hair over the threshold opens the gate and one a hair under leaves it closed
/// (the margin, 0.1 percent, is far outside the rounding of the comparison), on a
/// threshold under and over 0 dB.
#[test]
fn a_level_either_side_of_the_threshold_opens_or_closes() {
    for threshold in [-20.0f32, 6.0] {
        let edge = f64::from(amplitude(f64::from(threshold)));
        let c = controls(threshold, 0.0, 0.0, 0.0, -30.0);
        for (level, opens) in [(edge * 1.001, true), (edge * 0.999, false)] {
            let mut k = runner(&c);
            let key = vec![level as f32; 64];
            let out = run_mono(&mut k, &vec![1.0; 64], &key, None);
            let want = if opens { 1.0 } else { amplitude(-30.0) };
            assert!(
                (out[63] / want - 1.0).abs() < 1e-5,
                "threshold {threshold} dB, level {level}: {} vs {want}",
                out[63]
            );
        }
    }
}

/// The settled gain is 1 open and the range closed: a long loud key, then a long quiet one.
#[test]
fn the_settled_gain_is_one_open_and_the_range_closed() {
    for range in [-80.0f32, -24.0, -6.0] {
        for rate in RATES {
            let c = controls(-30.0, 0.002, 0.01, 0.02, range);
            let mut k = runner_at(&c, rate);
            // 40 000 frames at 48 kHz (a rate-long 0.83 s) is 40 release time constants: the
            // residue is under 1e-17.
            let n = (rate.get() * 0.8333) as usize;
            let ones = vec![1.0f32; n];
            let out = run_mono(&mut k, &ones, &vec![amplitude(-10.0); n], None);
            // An f32 one-pole stalls within `ulp / c` of its target (about 6e-6 here).
            assert!(
                (out[n - 1] - 1.0).abs() < 2e-5,
                "{rate:?} open: {}",
                out[n - 1]
            );
            let out = run_mono(&mut k, &ones, &vec![amplitude(-60.0); n], None);
            let want = amplitude(f64::from(range));
            assert!(
                (out[n - 1] / want - 1.0).abs() < 1e-5,
                "{rate:?} closed at {range} dB: {} vs {want}",
                out[n - 1]
            );
        }
    }
}

/// The hold keeps an opened gate fully open for exactly `floor(hold * rate)` frames after
/// the key falls under the threshold, and the release (instant here) starts on the next.
#[test]
fn the_hold_keeps_the_gate_open_for_its_whole_length() {
    // 0.0102 s is 489.6 frames at 48 kHz, 449.8 at 44.1 and 979.2 at 96: the count is the floor.
    for (rate, hold) in [(RATES[0], 449usize), (RATES[1], 489), (RATES[2], 979)] {
        let c = controls(-20.0, 0.0, 0.0102, 0.0, -40.0);
        let (loud, quiet) = (10, 2000);
        let mut key = vec![amplitude(-6.0); loud];
        key.extend(vec![0.0f32; quiet]);
        let ones = vec![1.0f32; loud + quiet];
        let mut k = runner_at(&c, rate);
        let out = run_mono(&mut k, &ones, &key, None);
        let closed = amplitude(-40.0);
        for (i, &x) in out.iter().enumerate() {
            let open = i < loud + hold;
            let want = if open { 1.0 } else { closed };
            assert!(
                (x / want - 1.0).abs() < 1e-5,
                "{rate:?} frame {i} ({} after the fall): {x} vs {want}",
                i as i64 - loud as i64
            );
        }
    }
}

/// The follower is a one-pole: after one attack time constant of an open key it has risen
/// to `1 - 1/e` of the way from closed to open, and after one release time constant of a
/// closed one `1/e` of the way is left (the coefficient is `1 - exp(-1 / (tau * rate))`,
/// so the gap after `tau * rate` frames is `e^-1`).
#[test]
fn the_follower_takes_one_time_constant_to_the_one_over_e_point() {
    // 480 and 960 frames at each rate: the time constants are whole frames.
    let (attack, release) = (480usize, 960usize);
    let range = -40.0f32;
    let r = f64::from(amplitude(f64::from(range)));
    let fraction = |gain: f32| (f64::from(gain) - r) / (1.0 - r);
    let e = std::f64::consts::E;

    for rate in RATES {
        let seconds = |frames: usize| frames as f64 / rate.get();
        let c = controls(-30.0, seconds(attack), 0.0, seconds(release), range);
        let mut k = runner_at(&c, rate);
        let up = run_mono(
            &mut k,
            &vec![1.0; attack],
            &vec![amplitude(-10.0); attack],
            None,
        );
        let rose = fraction(up[attack - 1]);
        assert!(
            (rose - (1.0 - 1.0 / e)).abs() < 1e-3,
            "{rate:?}: {rose} after one attack"
        );

        let mut k = runner_at(&c, rate);
        let _ = run_mono(
            &mut k,
            &vec![1.0; 20_000],
            &vec![amplitude(-10.0); 20_000],
            None,
        );
        let down = run_mono(&mut k, &vec![1.0; release], &vec![0.0; release], None);
        let left = fraction(down[release - 1]);
        assert!(
            (left - 1.0 / e).abs() < 1e-3,
            "{rate:?}: {left} left after one release"
        );
    }
}

/// One linked gain on every channel: the loudest key channel opens the gate, wherever it
/// is, a quiet channel does not close it, and every main channel is multiplied by it.
#[test]
fn the_gain_is_linked_across_the_channels_from_the_loudest_key() {
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let loud = vec![0.9f32; n];
    let quiet = vec![0.001f32; n];
    // Exact binary multiples, so a shared gain is a bit-exact relation.
    let (l, r) = (vec![1.0f32; n], vec![0.25f32; n]);
    let mut gains = Vec::new();
    for key in [[&loud, &quiet], [&quiet, &loud], [&loud, &loud]] {
        let mut k = runner(&c);
        let out = run(&mut k, &[&l, &r], &[key[0], key[1]], None);
        for (i, (&a, &b)) in out[0].iter().zip(&out[1]).enumerate() {
            assert_eq!(b, 0.25 * a, "the gain differs by channel at {i}");
        }
        assert!(out[0][n - 1] > 0.99, "the loud key opened the gate");
        gains.push(out[0].clone());
    }
    assert_eq!(gains[0], gains[1], "the key's channel order moved the gain");
    assert_eq!(
        gains[0], gains[2],
        "a quiet key channel lowered the loudest one's gain"
    );
}

/// A side-chain key drives the detector, not the signal: a quiet signal under a loud key
/// passes whole, a loud signal under a silent key (an unwired key) is closed to the range.
#[test]
fn a_side_chain_key_and_not_the_signal_drives_the_detector() {
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let (quiet, loud, silent) = (vec![0.01f32; n], vec![0.9f32; n], vec![0.0f32; n]);

    let mut k = runner(&c);
    let out = run_mono(&mut k, &quiet, &loud, None);
    assert!(
        (out[n - 1] - 0.01).abs() < 1e-7,
        "a loud key opens the gate"
    );

    let mut k = runner(&c);
    let out = run_mono(&mut k, &loud, &silent, None);
    let want = 0.9 * amplitude(-40.0);
    assert!(
        (out[n - 1] / want - 1.0).abs() < 1e-5,
        "a silent key leaves the gate closed: {} vs {want}",
        out[n - 1]
    );
}

/// An own key is the signal itself: a loud signal opens its own gate and a quiet one under
/// the threshold stays closed.
#[test]
fn an_own_key_detects_the_signal_itself() {
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let loud = vec![0.5f32; n];
    let quiet = vec![0.01f32; n];

    let mut k = runner(&c);
    let out = run(&mut k, &[&loud, &loud], &[&loud, &loud], None).remove(0);
    assert!(
        (out[n - 1] - 0.5).abs() < 1e-6,
        "a loud signal opens itself: {}",
        out[n - 1]
    );

    let mut k = runner(&c);
    let out = run(&mut k, &[&quiet, &quiet], &[&quiet, &quiet], None).remove(0);
    let want = 0.01 * amplitude(-40.0);
    assert!(
        (out[n - 1] / want - 1.0).abs() < 1e-5,
        "a quiet signal gates itself: {} vs {want}",
        out[n - 1]
    );
}

/// A threshold lane is read per frame: with the follower instant, each frame is open
/// when the key is at or over that frame's threshold, and a frame that is not finite takes
/// the aimed one.
#[test]
fn a_threshold_lane_is_read_per_frame() {
    let n = 600;
    let base = -30.0f32;
    let c = controls(base, 0.0, 0.0, 0.0, -40.0);
    let level = -20.0f32;
    let lane: Vec<f32> = (0..n)
        .map(|i| match i {
            0..100 => 0.0,
            100..200 => -40.0,
            200..300 => f32::NAN,
            300..400 => f32::INFINITY,
            400..500 => 10.0,
            _ => f32::NEG_INFINITY,
        })
        .collect();
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let out = run_mono(
        &mut k,
        &ones,
        &vec![amplitude(f64::from(level)); n],
        Some(&lane),
    );
    for (i, &t) in lane.iter().enumerate() {
        let threshold = if t.is_finite() { t } else { base };
        let want = if level >= threshold {
            1.0
        } else {
            amplitude(-40.0)
        };
        assert!(
            (out[i] / want - 1.0).abs() < 1e-5,
            "frame {i}: threshold {t}: {} vs {want}",
            out[i]
        );
    }
}

/// A threshold at or under [`Db::FLOOR`] opens on a silent key (silence is pinned at the
/// floor for the comparison) and one above it does not; the design and the render agree,
/// and a level over a floor-pinned threshold still decides on its own.
#[test]
fn silence_opens_a_threshold_at_or_under_the_floor() {
    for (threshold, opens) in [(-200.0f32, true), (Db::FLOOR.get(), true), (-143.0, false)] {
        let c = controls(threshold, 0.0, 0.0, 0.0, -40.0);
        let m = Design::of(&c, RATE);
        assert_eq!(m.silence_opens, opens, "threshold {threshold}");
        let mut k = runner(&c);
        let out = run_mono(&mut k, &vec![1.0; 64], &vec![0.0; 64], None);
        let want = if opens { 1.0 } else { amplitude(-40.0) };
        assert!(
            (out[63] / want - 1.0).abs() < 1e-5,
            "threshold {threshold}: {} vs {want}",
            out[63]
        );
    }
    // -150 dB is 3.2e-8: a key of 1e-10 (-200 dB) is under it, and one of 1e-7 is over.
    let c = controls(-150.0, 0.0, 0.0, 0.0, -40.0);
    let mut k = runner(&c);
    let out = run_mono(&mut k, &vec![1.0; 64], &vec![1.0e-10; 64], None);
    assert!(out[63] < 0.1, "-200 dB is under a -150 dB threshold");
    let out = run_mono(&mut k, &vec![1.0; 64], &vec![1.0e-7; 64], None);
    assert!(out[63] > 0.99, "-140 dB is over a -150 dB threshold");
}

/// A key of NaN reads as silent (the detector's `max` ignores it): the gate stays closed
/// and finite, and it recovers on the next finite level.
#[test]
fn a_nan_key_leaves_the_gate_closed_and_finite() {
    let n = 600;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let out = run_mono(&mut k, &ones, &vec![f32::NAN; n], None);
    assert!(out.iter().all(|x| x.is_finite()));
    assert!((out[n - 1] / amplitude(-40.0) - 1.0).abs() < 1e-5);
    let out = run_mono(&mut k, &vec![1.0; 4000], &vec![0.5; 4000], None);
    assert!(out[3999] > 0.99, "it recovers on a finite key");
}

/// A range that moves glides over [`GAIN_GLIDE`] (5 ms: 221, 240 and 480 frames at 44.1, 48
/// and 96 kHz) in a straight line in dB, frame `i` being `(i + 1) / n` of the way, and is
/// exactly its value after; no frame steps.
#[test]
fn a_moved_range_glides_over_five_milliseconds() {
    for (rate, frames) in [(RATES[0], 221usize), (RATES[1], 240), (RATES[2], 480)] {
        assert_eq!((0.005 * rate.get()).ceil() as usize, frames);
        let (a, b) = (
            controls(-30.0, 0.0, 0.0, 0.0, -60.0),
            controls(-30.0, 0.0, 0.0, 0.0, -12.0),
        );
        let mut k = runner_at(&a, rate);
        k.aim(&b);
        let n = 2 * frames;
        // A closed gate (silent key): the gain is the range itself.
        let out = run_mono(&mut k, &vec![1.0; n], &vec![0.0; n], None);
        let mut previous = f64::from(amplitude(-60.0));
        // 48 dB over `frames` frames is a factor of this a frame.
        let step = 10f64.powf(48.0 / frames as f64 / 20.0) * (1.0 + 1e-4);
        for (i, &x) in out.iter().enumerate() {
            let db = -60.0 + 48.0 * ((i + 1).min(frames) as f64 / frames as f64);
            let want = 10f64.powf(db / 20.0);
            assert!(
                (f64::from(x) / want - 1.0).abs() < 1e-5,
                "{rate:?} frame {i}: {x} vs {want}"
            );
            assert!(f64::from(x) / previous < step, "{rate:?}: a step at {i}");
            previous = f64::from(x);
        }
    }
}

/// A range that does not move is applied exactly from the first frame (a bank at rest on it
/// has nothing gliding).
#[test]
fn a_held_range_applies_exactly() {
    let c = controls(-30.0, 0.0, 0.0, 0.0, -12.0);
    let mut k = runner(&c);
    let out = run_mono(&mut k, &[1.0; 50], &[0.0; 50], None);
    let want = 10f64.powf(-12.0 / 20.0);
    for &x in &out {
        assert!((f64::from(x) / want - 1.0).abs() < 1e-6, "{x} vs {want}");
    }
}

/// A time that moves lands at once (the follower is the smoother): a release that
/// shortens from 1 s to instant closes the gate on the next frame, with no glide.
#[test]
fn a_moved_time_lands_at_once() {
    let (a, b) = (
        controls(-20.0, 0.0, 0.0, 1.0, -40.0),
        controls(-20.0, 0.0, 0.0, 0.0, -40.0),
    );
    let mut k = runner(&a);
    let _ = run_mono(&mut k, &vec![1.0; 100], &vec![0.5; 100], None);
    k.aim(&b);
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!(
        (out[0] / amplitude(-40.0) - 1.0).abs() < 1e-5,
        "closed on the first frame: {}",
        out[0]
    );
}

/// A reset clears the follower and the hold: a key that has just fallen, mid-hold, leaves
/// the gate open before it and closed after it.
#[test]
fn a_reset_clears_the_follower_and_the_hold() {
    let c = controls(-20.0, 0.0, 1.0, 1.0, -40.0);
    let mut k = runner(&c);
    let _ = run_mono(&mut k, &vec![1.0; 100], &vec![0.5; 100], None);
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!((out[0] - 1.0).abs() < 1e-6, "the hold keeps it open");
    k.reset();
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!(
        (out[0] / amplitude(-40.0) - 1.0).abs() < 1e-5,
        "a reset gate is closed: {}",
        out[0]
    );
}

/// The hold is what stops a gate chattering: a key that hovers 3 dB either side of the
/// threshold every 10 frames flutters a gate with no hold between its range and 1, and a
/// hold longer than the half-period keeps the gate fully open after its first opening.
#[test]
fn a_hold_keeps_a_hovering_key_from_chattering() {
    let n = 2000;
    let key: Vec<f32> = (0..n)
        .map(|i| amplitude(if (i / 10) % 2 == 0 { -17.0 } else { -23.0 }))
        .collect();
    let ones = vec![1.0f32; n];
    let closed = amplitude(-40.0);
    let mut bare = runner(&controls(-20.0, 0.0, 0.0, 0.0, -40.0));
    let out = run_mono(&mut bare, &ones, &key, None);
    let chatter = out.iter().filter(|&&x| x < 0.5).count();
    assert_eq!(
        chatter,
        n / 2,
        "no hold: the gate is shut on every low half-period"
    );
    assert!(out.iter().any(|&x| (x / closed - 1.0).abs() < 1e-5));
    let mut held = runner(&controls(-20.0, 0.0, 0.0102, 0.0, -40.0));
    let out = run_mono(&mut held, &ones, &key, None);
    assert!(
        out.iter().all(|&x| (x - 1.0).abs() < 1e-6),
        "a 489-frame hold rides through 10-frame dips"
    );
}

/// The render does not depend on how the stream is cut: calls of 1, 7, 33, 64, 100 and 255
/// frames give the bits of one call, with a threshold lane, a key that moves and a range
/// glide in flight.
#[test]
fn a_split_stream_renders_the_bits_of_the_whole() {
    let n = 3000;
    let c = controls(-20.0, 0.002, 0.01, 0.03, -30.0);
    let to = controls(-20.0, 0.002, 0.01, 0.03, -6.0);
    let audio = noise(1, n, 0.8);
    let key = noise(2, n, 0.5);
    let lane: Vec<f32> = (0..n).map(|i| -30.0 + (i % 700) as f32 * 0.03).collect();
    let render = |block: usize| {
        let mut k = runner(&c);
        k.aim(&to);
        run_in(&mut k, &[&audio, &audio], &[&key, &key], Some(&lane), block)
    };
    let whole = render(n);
    for block in [1, 7, 33, 64, 100, 255] {
        assert_eq!(render(block), whole, "blocks of {block}");
    }
}

/// A reset bank renders as a fresh one, bit for bit.
#[test]
fn a_reset_bank_renders_as_a_fresh_one() {
    let n = 2000;
    let c = controls(-24.0, 0.001, 0.02, 0.05, -50.0);
    let audio = noise(3, n, 0.7);
    let key = noise(4, n, 0.2);
    let mut worn = runner(&c);
    let _ = run(&mut worn, &[&audio], &[&key], None);
    worn.aim(&controls(-10.0, 0.01, 0.1, 0.1, -3.0));
    let _ = run(&mut worn, &[&audio], &[&key], None);
    worn.aim(&c);
    worn.reset();
    let mut fresh = runner(&c);
    assert_eq!(
        run(&mut worn, &[&audio], &[&key], None),
        run(&mut fresh, &[&audio], &[&key], None)
    );
}

/// A clone continues exactly where the original is and shares nothing with it.
#[test]
fn a_clone_continues_identically_and_shares_nothing() {
    let n = 1500;
    let c = controls(-24.0, 0.001, 0.02, 0.05, -50.0);
    let audio = noise(5, n, 0.7);
    let key = noise(6, n, 0.2);
    let mut a = runner(&c);
    let _ = run(&mut a, &[&audio], &[&key], None);
    let mut b = a.clone();
    let from_a = run(&mut a, &[&audio], &[&key], None);
    assert_eq!(run(&mut b, &[&audio], &[&key], None), from_a);
}

/// A control below its floor or not finite is the floor or the default, and the render is
/// finite.
#[test]
fn hostile_controls_render_finite() {
    let n = 600;
    let audio = noise(7, n, 1.0);
    let mut key = noise(8, n, 1.0);
    key[10] = f32::NAN;
    for c in hostile() {
        let mut k = runner(&c);
        let out = run(&mut k, &[&audio, &audio], &[&key, &key], None);
        for ch in &out {
            assert!(ch.iter().all(|x| x.is_finite()), "{c:?}");
        }
        let after = run(&mut k, &[&audio, &audio], &[&audio, &audio], None);
        assert!(after[0].iter().all(|x| x.is_finite()), "{c:?} after");
    }
}

/// Over a grid and seeded draws of every control, the gate only ever scales: each output
/// sample is the input times a gain between the closed floor and 1 (never above the input),
/// and the sign is the input's.
#[test]
fn every_reachable_gate_scales_between_its_floor_and_unity() {
    let n = 1500;
    let audio = noise(21, n, 1.0);
    let key = noise(22, n, 0.3);
    for c in reachable() {
        let floor = Db(c.range.get().min(0.0)).to_amplitude().get() * (1.0 - 1e-5);
        let mut k = runner(&c);
        let out = run(&mut k, &[&audio], &[&key], None).remove(0);
        for (i, (&y, &x)) in out.iter().zip(&audio).enumerate() {
            assert!(
                y.abs() <= x.abs() * (1.0 + 1e-6) && y.abs() >= x.abs() * floor,
                "{c:?} frame {i}: {y} from {x}"
            );
        }
    }
}

/// An infinite key sample is the largest finite level: it opens the gate (it is over any
/// threshold) and leaves the follower finite, so a quiet key closes the gate again.
#[test]
fn an_infinite_key_opens_the_gate_and_does_not_latch_it() {
    let n = 4000;
    let c = controls(-20.0, 0.0, 0.0, 0.0, -40.0);
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let mut key = vec![0.0f32; n];
    key[100] = f32::INFINITY;
    let out = run_mono(&mut k, &ones, &key, None);
    assert!(out.iter().all(|x| x.is_finite()));
    assert!((out[100] - 1.0).abs() < 1e-6, "open on the infinite sample");
    assert!(
        (out[n - 1] / amplitude(-40.0) - 1.0).abs() < 1e-5,
        "closed again"
    );
}

/// A render over `f64` views is the `f32` render converted: the state is `f32`, the views are
/// converted at the edges, and an `f32` input is exact in `f64`, so the two agree to the
/// last bit (a tolerance of 0), with a threshold lane and a range glide in flight.
#[test]
fn an_f64_render_is_the_f32_render_converted() {
    let n = 2_000;
    let audio = noise(61, n, 0.9);
    let key = noise(62, n, 0.3);
    let lane: Vec<f32> = (0..n).map(|i| -40.0 + (i % 400) as f32 * 0.05).collect();
    let (c, to) = (
        controls(-20.0, 0.002, 0.01, 0.05, -40.0),
        controls(-20.0, 0.002, 0.01, 0.05, -6.0),
    );
    let mut k = runner(&c);
    k.aim(&to);
    let want = run(&mut k, &[&audio], &[&key], Some(&lane)).remove(0);

    let to64 = |x: &[f32]| x.iter().map(|&v| f64::from(v)).collect::<Vec<f64>>();
    let (audio64, key64, lane64) = (to64(&audio), to64(&key), to64(&lane));
    let mut k = runner(&c);
    k.aim(&to);
    let mut got = vec![0.0f64; n];
    let mut at = 0;
    while at < n {
        let end = (at + CHUNK).min(n);
        k.process(
            GateLanes {
                threshold: Some(Mono::new(&lane64[at..end])),
            },
            Planes::new(&[&key64[at..end]]).unwrap(),
            Planes::new(&[&audio64[at..end]]).unwrap(),
            PlanesMut::new(&mut [&mut got[at..end]]).unwrap(),
        );
        at = end;
    }
    for (i, (&g, &w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, f64::from(w), "frame {i}");
    }
}
