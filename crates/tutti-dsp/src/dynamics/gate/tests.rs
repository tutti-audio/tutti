use crate::testing::uniform;
use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::{CompressionRatio, Db, SampleRate, Seconds};

use super::*;
use crate::testing::noise;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
/// The rates the static and time-constant tests run at.
const RATES: [SampleRate; 3] = [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K];
/// The longest block `run` hands the bank.
const CHUNK: usize = 256;

fn controls(threshold: f64, attack: f64, hold: f64, release: f64, range: f64) -> GateControls {
    GateControls {
        threshold: Db::new(threshold),
        attack: Seconds::new(attack),
        hold: Seconds::new(hold),
        release: Seconds::new(release),
        range: Db::new(range),
        ratio: CompressionRatio::new(f64::INFINITY),
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
    let mut rng = SplitMix64::new(24);
    all.extend((0..64).map(|_| {
        controls(
            uniform(&mut rng, -70.0, 6.0),
            uniform(&mut rng, 0.0, 0.05),
            uniform(&mut rng, 0.0, 0.3),
            uniform(&mut rng, 0.0, 0.5),
            uniform(&mut rng, -90.0, 0.0),
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
            attack: Seconds::new(-1.0),
            hold: Seconds::new(-1.0),
            release: Seconds::new(-1.0),
            ..base
        },
        GateControls {
            range: Db::new(12.0),
            ..base
        },
    ];
    for ratio in [f64::NAN, 0.5, f64::MAX, f64::NEG_INFINITY] {
        all.push(GateControls {
            ratio: CompressionRatio::new(ratio),
            ..base
        });
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        all.push(GateControls {
            threshold: Db::new(bad),
            range: Db::new(bad),
            ..base
        });
        all.push(GateControls {
            attack: Seconds::new(bad),
            hold: Seconds::new(bad),
            release: Seconds::new(bad),
            ..base
        });
    }
    all
}

/// A bank at `rate` at rest on `c`: set and at rest, nothing ramping.
fn bank_at<F: Float + SimdLanes + Lerp>(c: &GateControls, rate: SampleRate) -> GateBank<F> {
    let mut bank = GateBank::new(rate);
    bank.retarget(c);
    bank.jump_to_target();
    bank
}

/// [`bank_at`] in `f32` at `rate`.
fn runner_at(c: &GateControls, rate: SampleRate) -> GateBank<f32> {
    bank_at(c, rate)
}

/// [`runner_at`] at 48 kHz.
fn runner(c: &GateControls) -> GateBank<f32> {
    runner_at(c, RATE)
}

/// `audio` (one slice per channel) through `bank`, keyed by `key`, with `signal` as the
/// threshold control signal when given, in calls of at most `block` frames.
fn run_in<F: Float + SimdLanes + Lerp>(
    bank: &mut GateBank<F>,
    audio: &[&[F]],
    key: &[&[F]],
    signal: Option<&[F]>,
    block: usize,
) -> Vec<Vec<F>> {
    let n = audio[0].len();
    let mut out = vec![vec![F::ZERO; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[F]> = audio.iter().map(|c| &c[at..end]).collect();
        let kk: Vec<&[F]> = key.iter().map(|c| &c[at..end]).collect();
        let signals = GateSignals {
            threshold: signal.map(|l| Mono::new(&l[at..end])),
        };
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            signals,
            Planes::new(&kk).unwrap(),
            &mut InputOutput::paired(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// [`run_in`] in calls of [`CHUNK`] frames.
fn run<F: Float + SimdLanes + Lerp>(
    bank: &mut GateBank<F>,
    audio: &[&[F]],
    key: &[&[F]],
    signal: Option<&[F]>,
) -> Vec<Vec<F>> {
    run_in(bank, audio, key, signal, CHUNK)
}

/// One channel of `audio` and of the key, as the stereo pair the tests run.
fn run_mono<F: Float + SimdLanes + Lerp>(
    bank: &mut GateBank<F>,
    audio: &[F],
    key: &[F],
    signal: Option<&[F]>,
) -> Vec<F> {
    run(bank, &[audio, audio], &[key, key], signal).remove(0)
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
    let m = Design::<f32>::of(&controls(-20.0, 0.001, 0.0102, 0.1, -12.0), RATE);
    // 0.0102 s at 48 kHz is 489.6 frames, floored.
    assert_eq!(m.hold_samples, 489);
    assert_eq!(m.threshold, Db::new(-20.0).to_amplitude());
    // One step from 0 toward 1 lands on the attack coefficient, and one from 1 toward 0 on
    // 1 less the release coefficient.
    let (attack, release) = (
        Seconds::new(0.001).one_pole_coeff(RATE) as f32,
        Seconds::new(0.1).one_pole_coeff(RATE) as f32,
    );
    assert_eq!(m.ballistics.follow_raw(&mut Smoothed::at(0.0), 1.0), attack);
    assert_eq!(
        m.ballistics.follow_raw(&mut Smoothed::at(1.0), 0.0),
        1.0 - release
    );
    assert_eq!(m.range, Db::new(-12.0));

    let m = Design::<f32>::of(
        &GateControls {
            attack: Seconds::new(-1.0),
            hold: Seconds::new(-1.0),
            release: Seconds::new(-1.0),
            range: Db::new(6.0),
            ..GateControls::DEFAULT
        },
        RATE,
    );
    assert_eq!(
        m.ballistics.follow_raw(&mut Smoothed::at(0.0), 1.0),
        1.0,
        "an attack under 0 is instant"
    );
    assert_eq!(
        m.ballistics.follow_raw(&mut Smoothed::at(1.0), 0.0),
        0.0,
        "a release under 0 is instant"
    );
    assert_eq!(m.hold_samples, 0);
    assert_eq!(m.range, Db::UNITY, "a range over 0 dB reads as 0");

    let m = Design::<f32>::of(
        &GateControls {
            threshold: Db::new(f64::NAN),
            range: Db::new(f64::INFINITY),
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
    for threshold in [-20.0f64, 6.0] {
        let edge = f64::from(amplitude(threshold));
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
    for range in [-80.0f64, -24.0, -6.0] {
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
            let want = amplitude(range);
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
    let range = -40.0f64;
    let r = f64::from(amplitude(range));
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

/// A threshold control signal is read per frame: with the follower instant, each frame is open
/// when the key is at or over that frame's threshold, and a frame that is not finite takes
/// the set one.
#[test]
fn a_threshold_signal_is_read_per_frame() {
    let n = 600;
    let base = -30.0f64;
    let c = controls(base, 0.0, 0.0, 0.0, -40.0);
    let level = -20.0f32;
    let signal: Vec<f32> = (0..n)
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
        Some(&signal),
    );
    for (i, &t) in signal.iter().enumerate() {
        let threshold = if t.is_finite() { f64::from(t) } else { base };
        let want = if f64::from(level) >= threshold {
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
    for (threshold, opens) in [(-200.0f64, true), (Db::FLOOR.get(), true), (-143.0, false)] {
        let c = controls(threshold, 0.0, 0.0, 0.0, -40.0);
        let m = Design::<f32>::of(&c, RATE);
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

/// A range that moves ramps over [`GAIN_RAMP`] (5 ms: 221, 240 and 480 frames at 44.1, 48
/// and 96 kHz) in a straight line in dB, frame `i` being `(i + 1) / n` of the way, and is
/// exactly its value after; no frame steps.
#[test]
fn a_moved_range_ramps_over_five_milliseconds() {
    for (rate, frames) in [(RATES[0], 221usize), (RATES[1], 240), (RATES[2], 480)] {
        assert_eq!((0.005 * rate.get()).ceil() as usize, frames);
        let (a, b) = (
            controls(-30.0, 0.0, 0.0, 0.0, -60.0),
            controls(-30.0, 0.0, 0.0, 0.0, -12.0),
        );
        let mut k = runner_at(&a, rate);
        k.retarget(&b);
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
/// has nothing ramping).
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
/// shortens from 1 s to instant closes the gate on the next frame, with no ramp.
#[test]
fn a_moved_time_lands_at_once() {
    let (a, b) = (
        controls(-20.0, 0.0, 0.0, 1.0, -40.0),
        controls(-20.0, 0.0, 0.0, 0.0, -40.0),
    );
    let mut k = runner(&a);
    let _ = run_mono(&mut k, &vec![1.0; 100], &vec![0.5; 100], None);
    k.retarget(&b);
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

/// The follower of a gate opening from closed after `frames` frames of an attack of `tau`
/// seconds at 48 kHz: `1 - a^frames` with `a = exp(-1 / (tau rate))`.
fn opened(tau: f64, frames: usize) -> f64 {
    1.0 - (-(frames as f64) / (tau * RATE.get())).exp()
}

/// The gain of a gate `open` of the way from closed at `range` dB: `r + open (1 - r)`, `r` the
/// range as an amplitude.
fn gain_at(range: f64, open: f64) -> f64 {
    let r = 10f64.powf(range / 20.0);
    r + open * (1.0 - r)
}

/// The render is the closed form whatever the calls cut the stream into: a key over a control signal's
/// threshold (and under the set one) opens the gate along the one-pole `1 - a^n`, then falls
/// silent at frame 517 (inside a block of 7, 33, 64, 100 and 255 frames alike) with no hold, so
/// the follower releases as `f b^m` from the `f` the step found, and the range ramps linearly in
/// dB over the 240 steps of 5 ms at 48 kHz, in calls of 1, 7, 33, 64, 100 and 255 frames and in
/// one call.
#[test]
fn a_cut_stream_follows_the_one_pole_and_ramp_closed_forms() {
    let (tau, release, steps, n, step_at) = (0.002, 0.03, 240usize, 1_200usize, 517usize);
    let (from, to) = (-30.0, -6.0);
    let c = controls(-10.0, tau, 0.0, release, from);
    let target = controls(-10.0, tau, 0.0, release, to);
    let b = (-1.0 / (release * RATE.get())).exp();
    let open = |frames: usize| {
        if frames <= step_at {
            opened(tau, frames)
        } else {
            opened(tau, step_at) * b.powi((frames - step_at) as i32)
        }
    };
    let ones = vec![1.0f64; n];
    // -20 dB: over the control signal's -30 dB threshold, under the set -10 dB one; then silence.
    let key: Vec<f64> = (0..n)
        .map(|i| if i < step_at { 0.1 } else { 0.0 })
        .collect();
    let signal = vec![-30.0f64; n];
    for block in [1, 7, 33, 64, 100, 255, n] {
        let mut k = bank_at::<f64>(&c, RATE);
        k.retarget(&target);
        let out = run_in(&mut k, &[&ones], &[&key], Some(&signal), block).remove(0);
        for (i, &y) in out.iter().enumerate() {
            let range = from + (to - from) * ((i + 1).min(steps) as f64 / steps as f64);
            let want = gain_at(range, open(i + 1));
            assert!(
                (y / want - 1.0).abs() < 1e-12,
                "blocks of {block}, frame {i}: {y} vs {want}"
            );
        }
    }
}

/// A reset gate opens from closed along the one-pole closed form with no hold left: after wear
/// under other controls and a reset, a key over the threshold gives `r + (1 - a^n)(1 - r)` from
/// its first frame, the range at rest on its target.
#[test]
fn a_reset_gate_opens_from_closed_along_the_closed_form() {
    let (tau, range) = (0.001, -50.0);
    let n = 2_000;
    let c = controls(-24.0, tau, 0.02, 0.05, range);
    let wear: Vec<f64> = noise(3, n, 0.7).iter().map(|&x| f64::from(x)).collect();
    let mut worn = bank_at::<f64>(&c, RATE);
    let _ = run(&mut worn, &[&wear], &[&wear], None);
    worn.retarget(&controls(-10.0, 0.01, 0.1, 0.1, -3.0));
    let _ = run(&mut worn, &[&wear], &[&wear], None);
    worn.retarget(&c);
    worn.reset();
    let ones = vec![1.0f64; n];
    let key = vec![0.5f64; n];
    let out = run(&mut worn, &[&ones], &[&key], None).remove(0);
    for (i, &y) in out.iter().enumerate() {
        let want = gain_at(range, opened(tau, i + 1));
        assert!((y / want - 1.0).abs() < 1e-12, "frame {i}: {y} vs {want}");
    }
}

/// A clone continues the one-pole where the original is and shares nothing with it: both open
/// along the closed form on from the frame they were cloned at, and what the clone is set to
/// and rendered under leaves the original's closed form unmoved.
#[test]
fn a_clone_continues_along_the_closed_form_and_shares_nothing() {
    let (tau, range) = (0.003, -50.0);
    let c = controls(-24.0, tau, 0.02, 0.05, range);
    let (before, after) = (700usize, 900usize);
    let ones = vec![1.0f64; after];
    let key = vec![0.5f64; after];
    let mut a = bank_at::<f64>(&c, RATE);
    let _ = run(&mut a, &[&ones[..before]], &[&key[..before]], None);
    let mut b = a.clone();
    let on = |out: &[f64], what: &str| {
        for (j, &y) in out.iter().enumerate() {
            let want = gain_at(range, opened(tau, before + j + 1));
            assert!(
                (y / want - 1.0).abs() < 1e-12,
                "{what}, frame {j}: {y} vs {want}"
            );
        }
    };
    on(&run(&mut b, &[&ones], &[&key], None)[0], "the clone");
    // The clone moves to other controls and runs on; the original has not heard of it.
    b.retarget(&controls(-10.0, 0.0, 0.0, 0.0, -3.0));
    let _ = run(&mut b, &[&ones], &[&key], None);
    on(&run(&mut a, &[&ones], &[&key], None)[0], "the original");
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
        let floor = (Db::new(c.range.get().min(0.0)).to_amplitude().get() * (1.0 - 1e-5)) as f32;
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

/// `audio` through an `f64` bank at 48 kHz set to `c` and at rest, in one block, keyed by
/// `key`.
fn run_f64(c: &GateControls, audio: &[f64], key: &[f64]) -> Vec<f64> {
    let mut bank = GateBank::<f64>::new(RATE);
    bank.retarget(c);
    bank.jump_to_target();
    let mut out = vec![0.0f64; audio.len()];
    bank.process(
        GateSignals::default(),
        Planes::new(&[key]).unwrap(),
        &mut InputOutput::paired(
            Planes::new(&[audio]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    out
}

/// An open gate (a key over the threshold, an instant attack) has gain `range + 1 (1 -
/// range)`, which is 1 to 2^-50 whatever the range: an `f64` signal of more than 24 bits
/// comes out as it went in.
#[test]
fn an_f64_open_gate_is_the_identity() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(300);
    let c = controls(-40.0, 0.0, 0.0, 0.0, -80.0);
    assert_close_f64(&run_f64(&c, &x, &x), &x, 2.0f64.powi(-50));
}

/// An opening gate's follower is the one-pole `f[n] = f[n-1] + c (1 - f[n-1])`, `c = 1 -
/// exp(-1 / (tau rate))`, from closed, and the gain is `r + f[n] (1 - r)` with `r =
/// 10^(range / 20)`: the closed form in `f64`, to 1e-12.
#[test]
fn an_f64_gate_opens_along_the_one_pole_closed_form_in_f64() {
    use tutti_fixtures::signal::assert_close_f64;
    let (tau, range) = (0.002, -60.0);
    let key: Vec<f64> = vec![tutti_fixtures::signal::wide_signal(1)[0]; 2_000];
    let c = controls(-40.0, tau, 0.0, 0.1, range);
    let coeff = 1.0 - (-1.0 / (tau * RATE.get())).exp();
    let r = 10f64.powf(range / 20.0);
    let mut open = 0.0f64;
    let want: Vec<f64> = key
        .iter()
        .map(|&x| {
            open += coeff * (1.0 - open);
            x * (r + open * (1.0 - r))
        })
        .collect();
    assert_close_f64(&run_f64(&c, &key, &key), &want, 1e-12);
}

/// A gate released over 300 s at 192 kHz (a coefficient of `1.7e-8`, under half an ulp of a
/// follower near 1) still closes in f32: opened at once by a loud key with no hold, then given
/// silence, the follower falls as `a^n`, `a = 1 - c` with `c` the f32 coefficient, so over a
/// quarter of a time constant the gain is `r + a^n (1 - r)` for the range `r` to within `2^-24`
/// stationary in the follower (the product's rounding over `c`), half an ulp carried in its
/// value, and `2^-24` for the gain's own rounding. The plain recurrence `f - c f`, computed here,
/// stays frozen at 1.
#[test]
fn an_f32_three_hundred_second_release_at_192_khz_still_closes_along_the_closed_form() {
    const CHUNK_FRAMES: usize = 1 << 16;
    let (rate, tau, range) = (SampleRate::SR_192K, 300.0, -60.0);
    let c = f64::from(Seconds::new(tau).one_pole_coeff(rate) as f32);
    let mut plain = 1.0f32;
    for _ in 0..1_000 {
        plain += -(c as f32) * plain;
    }
    assert_eq!(plain, 1.0, "the plain recurrence moved");
    let mut k = runner_at(&controls(-20.0, 0.0, 0.0, tau, range), rate);
    let _ = run_mono(&mut k, &[1.0f32; 64], &[0.5f32; 64], None);
    let n = (0.25 * tau * rate.get()) as usize;
    let probes = [n / 4, n / 2, n - 1];
    let (ones, quiet) = (vec![1.0f32; CHUNK_FRAMES], vec![0.0f32; CHUNK_FRAMES]);
    let (mut seen, mut worst) = (0, 0.0f64);
    while seen < n {
        let m = CHUNK_FRAMES.min(n - seen);
        let out = run_mono(&mut k, &ones[..m], &quiet[..m], None);
        for &p in probes.iter().filter(|&&p| (seen..seen + m).contains(&p)) {
            let want = gain_at(range, (-c).ln_1p().exp().powi(p as i32 + 1));
            worst = worst.max((f64::from(out[p - seen]) - want).abs());
            assert!(want < 0.99, "the gate has closed some by the probe");
        }
        seen += m;
    }
    let bound = 2f64.powi(-24) + 2f64.powi(-25) + 2f64.powi(-24);
    assert!(
        worst <= bound,
        "{worst} off the closed form, bound {bound:e}"
    );
}

/// The last frame of a constant key `under` dB under `threshold` dB, gating a signal of ones in
/// an `f64` bank at rest on `c` (no times, so the follower is on its target at once).
fn settled_gain_under(c: &GateControls, under: f64) -> f64 {
    let level = 10f64.powf((c.threshold.get() - under) / 20.0);
    let key = vec![level; 64];
    run_f64(c, &vec![1.0; 64], &key)[63]
}

fn expander(threshold: f64, ratio: f64, range: f64) -> GateControls {
    GateControls {
        ratio: CompressionRatio::new(ratio),
        ..controls(threshold, 0.0, 0.0, 0.0, range)
    }
}

/// A downward expander's static curve: a key `x` dB under the threshold has a gain of
/// `-(ratio - 1) x` dB, floored at the range. At ratios 2 and 4 from 3 dB to 30 dB under a
/// threshold of -20 dB, over a range of -80 dB, the gain is `10^(-(ratio - 1) x / 20)` or the
/// range's, to `1e-10` (the `log10` and `powf` of the detector in `f64`).
#[test]
fn an_expander_gain_falls_ratio_db_per_db_under_the_threshold_to_the_range() {
    let range = Db::new(-80.0).to_amplitude().get();
    for ratio in [2.0, 4.0] {
        for under in [3.0, 10.0, 17.5, 30.0] {
            let want = 10f64.powf(-(ratio - 1.0) * under / 20.0).max(range);
            let got = settled_gain_under(&expander(-20.0, ratio, -80.0), under);
            assert!(
                (got / want - 1.0).abs() < 1e-10,
                "ratio {ratio}, {under} dB under: {got} against {want}"
            );
        }
    }
    // Ratio 4 at 30 dB under asks for -90 dB: the floor, exactly.
    assert_eq!(
        settled_gain_under(&expander(-20.0, 4.0, -80.0), 30.0),
        range
    );
}

/// A key over the threshold is open at unity gain whatever the ratio, and a ratio of 1 is no
/// expansion: the gain is unity at every level under the threshold too.
#[test]
fn an_expander_is_unity_over_the_threshold_and_at_a_ratio_of_one() {
    for ratio in [1.0, 2.0, 4.0] {
        let got = settled_gain_under(&expander(-20.0, ratio, -80.0), -10.0);
        assert!((got - 1.0).abs() < 1e-12, "ratio {ratio} over: {got}");
    }
    for under in [3.0, 40.0] {
        let got = settled_gain_under(&expander(-20.0, 1.0, -80.0), under);
        assert!(
            (got - 1.0).abs() < 1e-12,
            "ratio 1, {under} dB under: {got}"
        );
    }
}

/// The infinite ratio is the gate: a key a hair under the threshold is at the range at once, a
/// NaN ratio reads as the gate, and a ratio under 1 reads as 1. The assertions are those of
/// `a_level_either_side_of_the_threshold_opens_or_closes`.
#[test]
fn the_infinite_ratio_is_the_gate_and_a_ratio_under_one_reads_as_one() {
    let range = Db::new(-30.0).to_amplitude().get();
    for threshold in [-20.0f64, 6.0] {
        for ratio in [f64::INFINITY, f64::NAN] {
            let c = expander(threshold, ratio, -30.0);
            let closed = settled_gain_under(&c, 0.001 * 8.686);
            assert!((closed / range - 1.0).abs() < 1e-12, "{ratio}: {closed}");
            assert!((settled_gain_under(&c, -0.001 * 8.686) - 1.0).abs() < 1e-12);
        }
    }
    let under_one = settled_gain_under(&expander(-20.0, 0.5, -80.0), 10.0);
    assert!((under_one - 1.0).abs() < 1e-12, "{under_one}");
}

/// The follower runs toward the expander's gain with the release: a key that drops from over the
/// threshold to 10 dB under it at ratio 2 (a gain of -10 dB) takes the gain from 1 to
/// `g = 10^(-10 / 20)` along `g + (1 - g) (1 - c)^n`, `c` the release's one-pole coefficient,
/// to `1e-9` (the range, -80 dB, is under `g`, so the fraction `(g - r) / (1 - r)` maps back to it).
#[test]
fn an_expander_follows_the_release_toward_its_gain() {
    let (tau, range) = (0.002, -80.0);
    let c = GateControls {
        ratio: CompressionRatio::new(2.0),
        ..controls(-20.0, 0.0, 0.0, tau, range)
    };
    let mut bank = GateBank::<f64>::new(RATE);
    bank.retarget(&c);
    bank.jump_to_target();
    let mut key = vec![1.0; 100];
    key.extend(vec![10f64.powf(-30.0 / 20.0); 400]);
    let mut out = vec![0.0f64; key.len()];
    bank.process(
        GateSignals::default(),
        Planes::new(&[&key]).unwrap(),
        &mut InputOutput::paired(
            Planes::new(&[&vec![1.0; key.len()]]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out[99], 1.0);
    let coeff = Seconds::new(tau).one_pole_coeff(RATE);
    let g = 10f64.powf(-10.0 / 20.0);
    for n in [0usize, 10, 100, 399] {
        let want = g + (1.0 - g) * (1.0 - coeff).powi(n as i32 + 1);
        assert!(
            (out[100 + n] - want).abs() < 1e-9,
            "frame {n}: {} against {want}",
            out[100 + n]
        );
    }
}
