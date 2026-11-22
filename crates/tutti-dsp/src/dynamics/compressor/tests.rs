use crate::testing::uniform;
use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::{CompressionRatio, Db, SampleRate, Seconds};

use super::*;
use crate::compute_compressor_gain_reduction;
use crate::testing::noise;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
/// The rates the static and time-constant tests run at.
const RATES: [SampleRate; 3] = [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K];
/// The longest block `run` hands the bank.
const CHUNK: usize = 256;

fn controls(
    threshold: f64,
    knee: f64,
    ratio: f64,
    attack: f64,
    release: f64,
    makeup: f64,
) -> CompressorControls {
    CompressorControls {
        threshold: Db::new(threshold),
        knee: Db::new(knee),
        ratio: CompressionRatio::new(ratio),
        attack: Seconds::new(attack),
        release: Seconds::new(release),
        makeup: Db::new(makeup),
        ..CompressorControls::DEFAULT
    }
}

/// A grid and seeded draws across every control.
fn reachable() -> Vec<CompressorControls> {
    let mut all = Vec::new();
    for threshold in [-40.0, -18.0, -6.0, 0.0] {
        for knee in [0.0, 6.0, 12.0] {
            for ratio in [1.0, 2.0, 4.0, 20.0] {
                for (attack, release) in [(0.0, 0.0), (0.001, 0.05), (0.01, 0.2)] {
                    all.push(controls(threshold, knee, ratio, attack, release, 0.0));
                }
            }
        }
    }
    let mut rng = SplitMix64::new(24);
    all.extend((0..64).map(|_| {
        controls(
            uniform(&mut rng, -60.0, 6.0),
            uniform(&mut rng, 0.0, 24.0),
            uniform(&mut rng, 1.0, 30.0),
            uniform(&mut rng, 0.0, 0.05),
            uniform(&mut rng, 0.0, 0.5),
            uniform(&mut rng, -12.0, 12.0),
        )
    }));
    all
}

/// Below the floors (a knee under 0, a ratio under 1, a time under 0) and not finite: each
/// is a control the bank can run.
fn hostile() -> Vec<CompressorControls> {
    let base = controls(-18.0, 6.0, 4.0, 0.003, 0.08, 3.0);
    let mut all = vec![
        CompressorControls {
            knee: Db::new(-6.0),
            ..base
        },
        CompressorControls {
            ratio: CompressionRatio::new(0.25),
            ..base
        },
        CompressorControls {
            attack: Seconds::new(-1.0),
            release: Seconds::new(-1.0),
            ..base
        },
    ];
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        all.push(CompressorControls {
            threshold: Db::new(bad),
            knee: Db::new(bad),
            ratio: CompressionRatio::new(bad),
            makeup: Db::new(bad),
            ..base
        });
        all.push(CompressorControls {
            attack: Seconds::new(bad),
            release: Seconds::new(bad),
            ..base
        });
    }
    all
}

/// A bank at `rate` at rest on `c`: set and at rest, nothing ramping.
fn bank_at<F: Float + SimdLanes + Lerp>(
    c: &CompressorControls,
    rate: SampleRate,
) -> CompressorBank<F> {
    let mut bank = CompressorBank::new(rate);
    bank.retarget(c);
    bank.jump_to_target();
    bank
}

/// [`bank_at`] in `f32` at `rate`.
fn runner_at(c: &CompressorControls, rate: SampleRate) -> CompressorBank<f32> {
    bank_at(c, rate)
}

/// [`runner_at`] at 48 kHz.
fn runner(c: &CompressorControls) -> CompressorBank<f32> {
    runner_at(c, RATE)
}

/// `audio` (one slice per channel) through `bank`, keyed by `key`, with `signal` as the
/// threshold control signal when given, in calls of at most `block` frames.
fn run_in<F: Float + SimdLanes + Lerp>(
    bank: &mut CompressorBank<F>,
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
        let signals = CompressorSignals {
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
    bank: &mut CompressorBank<F>,
    audio: &[&[F]],
    key: &[&[F]],
    signal: Option<&[F]>,
) -> Vec<Vec<F>> {
    run_in(bank, audio, key, signal, CHUNK)
}

/// A level of `db` as an amplitude, from the definition.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// The static curve of the compressor from its definition (Giannoulis, Massberg and
/// Reiss, 2012, eq. 4 and 5), in `f64`: the reduction in dB, positive.
fn curve(level: f64, threshold: f64, knee: f64, ratio: f64) -> f64 {
    let slope = 1.0 - 1.0 / ratio;
    if knee <= 0.0 {
        slope * (level - threshold).max(0.0)
    } else if level <= threshold - knee / 2.0 {
        0.0
    } else if level >= threshold + knee / 2.0 {
        slope * (level - threshold)
    } else {
        slope * (level - threshold + knee / 2.0).powi(2) / (2.0 * knee)
    }
}

/// The static curve of `m`'s controls at `level`.
fn reduction_db(m: &CompressorMember, level: Db) -> Db {
    compute_compressor_gain_reduction(level, m.threshold, m.ratio, m.knee)
}

/// The member's reduction is the static curve: nothing under the knee, the
/// straight line over it, the quadratic between, and the three agree at the knee's two
/// edges (the curve is continuous).
#[test]
fn the_member_reduction_is_the_static_curve() {
    for c in reachable() {
        let m = CompressorMember::designed(&c, RATE);
        let (t, w, r) = (m.threshold.get(), m.knee.get(), m.ratio.get());
        for step in -80..=40 {
            let level = f64::from(step) * 1.5 - 20.0;
            let want = curve(level, t, w, r);
            let got = reduction_db(&m, Db::new(level)).get();
            // `Db` is `f64` and the member and the oracle are one formula, so the gap is rounding.
            assert!(
                (got - want).abs() < 1e-12,
                "{c:?} at {level}: {got} vs {want}"
            );
        }
        // Continuity at both edges: the two pieces meeting there give the same value.
        for edge in [t - w / 2.0, t + w / 2.0] {
            let (below, above) = (
                reduction_db(&m, Db::new(edge - 1e-3)).get(),
                reduction_db(&m, Db::new(edge + 1e-3)).get(),
            );
            assert!(
                (above - below).abs() < 1e-2,
                "{c:?} jumps at the edge {edge}"
            );
        }
    }
}

/// Nothing below the knee, `(1 - 1/R)(L - T)` over it: five anchors, by hand.
#[test]
fn the_curve_has_its_three_regions() {
    let m = CompressorMember::designed(&controls(-20.0, 10.0, 4.0, 0.0, 0.0, 0.0), RATE);
    // Under -25 dB: nothing. Over -15 dB: three quarters of the excess.
    assert_eq!(reduction_db(&m, Db::new(-40.0)), Db::UNITY);
    assert_eq!(reduction_db(&m, Db::new(-25.0)), Db::UNITY);
    // `7.5` and `15` are exact in `f64`, which the `Db` stores; the quadratic is to rounding.
    assert_eq!(reduction_db(&m, Db::new(-10.0)), Db::new(7.5));
    assert_eq!(reduction_db(&m, Db::UNITY), Db::new(15.0));
    // At the threshold the quadratic has run a half of the knee: (3/4)(5)^2/(2*10).
    assert!((reduction_db(&m, Db::new(-20.0)).get() - 0.75 * 25.0 / 20.0).abs() < 1e-14);
}

/// The member holds the controls made runnable: a knee under 0 reads as 0, a ratio under
/// 1 as 1 and a control that is not finite as the default's, and its detector runs a time
/// under 0 as 0.
#[test]
fn the_member_floors_what_cannot_be_run() {
    let base = controls(-18.0, 6.0, 4.0, 0.003, 0.08, 3.0);
    let m = CompressorMember::designed(
        &CompressorControls {
            knee: Db::new(-6.0),
            ratio: CompressionRatio::new(0.25),
            attack: Seconds::new(-1.0),
            release: Seconds::new(-0.5),
            ..base
        },
        RATE,
    );
    assert_eq!((m.knee, m.ratio), (Db::UNITY, CompressionRatio::new(1.0)));
    // A time under 0 is the follower of time 0: instant.
    assert_eq!(
        CompressorStage::<f32>::of(&m).ballistics,
        Follower::<f32>::of(floored(Seconds::ZERO, Seconds::ZERO), RATE)
    );
    let d = CompressorControls::DEFAULT;
    let bad = CompressorMember::designed(
        &CompressorControls {
            threshold: Db::new(f64::NAN),
            knee: Db::new(f64::INFINITY),
            ratio: CompressionRatio::new(f64::NAN),
            makeup: Db::new(f64::NEG_INFINITY),
            ..base
        },
        RATE,
    );
    assert_eq!(
        (bad.threshold, bad.knee, bad.ratio, bad.makeup),
        (d.threshold, d.knee, d.ratio, d.makeup)
    );
}

/// A steady key `level` dB settles the reduction on the static curve, and the gain is that
/// reduction taken off the makeup: from the definition, over a grid.
#[test]
fn a_steady_key_settles_on_the_static_curve() {
    for rate in RATES {
        // The same 625 ms at every rate: twelve release time constants of the slowest case.
        let n = (rate.get() * 0.625) as usize;
        for (c, level) in [
            (controls(-20.0, 0.0, 4.0, 0.001, 0.05, 0.0), -10.0),
            (controls(-30.0, 0.0, 2.0, 0.001, 0.05, 3.0), -6.0),
            (controls(-20.0, 10.0, 4.0, 0.002, 0.05, 0.0), -20.0),
            (controls(-20.0, 10.0, 4.0, 0.002, 0.05, 0.0), -26.0),
            (controls(-20.0, 10.0, 4.0, 0.002, 0.05, -2.0), -14.0),
        ] {
            let mut k = runner_at(&c, rate);
            let ones = vec![1.0f32; n];
            let key = vec![amplitude(level); n];
            let out = run(&mut k, &[&ones, &ones], &[&key, &key], None);
            let want_db =
                -curve(level, c.threshold.get(), c.knee.get(), c.ratio.get()) + c.makeup.get();
            let got_db = 20.0 * f64::from(out[0][n - 1]).log10();
            assert!(
                (got_db - want_db).abs() < 2e-3,
                "{rate:?}, {c:?} at {level} dB: {got_db} vs {want_db}"
            );
        }
    }
}

/// The one-pole follower reaches `1 - 1/e` of its target in one time constant going up
/// (the attack) and has `1/e` left in one going down (the release): the coefficient is
/// `1 - exp(-1 / (tau * rate))`, so after `tau * rate` frames the gap is `e^-1`.
#[test]
fn the_follower_takes_one_time_constant_to_the_one_over_e_point() {
    // 480 and 960 frames at each rate: the time constants are whole frames.
    let (attack, release) = (480usize, 960usize);
    let level = -8.0;
    let target = curve(level, -20.0, 0.0, 4.0);
    let reduction_at = |out: &[f32], i: usize| -20.0 * f64::from(out[i]).log10();
    let e = std::f64::consts::E;
    for rate in RATES {
        let seconds = |frames: usize| frames as f64 / rate.get();
        let c = controls(-20.0, 0.0, 4.0, seconds(attack), seconds(release), 0.0);

        let mut k = runner_at(&c, rate);
        let ones = vec![1.0f32; attack];
        let key = vec![amplitude(level); attack];
        let up = run(&mut k, &[&ones, &ones], &[&key, &key], None);
        let rose = reduction_at(&up[0], attack - 1);
        assert!(
            (rose / target - (1.0 - 1.0 / e)).abs() < 1e-3,
            "{rate:?}: {rose} of {target} after one attack"
        );

        // Settle, then fall silent: one release time constant leaves `1/e` of it.
        let mut k = runner_at(&c, rate);
        let hold = vec![1.0f32; 20_000];
        let loud = vec![amplitude(level); 20_000];
        let _ = run(&mut k, &[&hold, &hold], &[&loud, &loud], None);
        let ones = vec![1.0f32; release];
        let quiet = vec![0.0f32; release];
        let down = run(&mut k, &[&ones, &ones], &[&quiet, &quiet], None);
        let left = reduction_at(&down[0], release - 1);
        assert!(
            (left / target - 1.0 / e).abs() < 1e-3,
            "{rate:?}: {left} of {target} after one release"
        );
    }
}

/// One linked gain on every channel: the loudest key channel sets it, wherever it is,
/// and every main channel is multiplied by it.
#[test]
fn the_gain_is_linked_across_the_channels_from_the_loudest_key() {
    let n = 4000;
    let c = controls(-20.0, 0.0, 4.0, 0.001, 0.05, 0.0);
    let loud = vec![0.9f32; n];
    let quiet = vec![0.01f32; n];
    // Exact binary multiples, so a shared gain is a bit-exact relation.
    let (l, r) = (vec![1.0f32; n], vec![0.25f32; n]);
    let mut gains = Vec::new();
    for key in [[&loud, &quiet], [&quiet, &loud], [&loud, &loud]] {
        let mut k = runner(&c);
        let out = run(&mut k, &[&l, &r], &[key[0], key[1]], None);
        for (i, (&a, &b)) in out[0].iter().zip(&out[1]).enumerate() {
            assert_eq!(b, 0.25 * a, "the gain differs by channel at {i}");
        }
        assert!(out[0][n - 1] < 0.5, "the loud key reduced the gain");
        gains.push(out[0].clone());
    }
    assert_eq!(gains[0], gains[1], "the key's channel order moved the gain");
    assert_eq!(
        gains[0], gains[2],
        "a quiet key channel lowered the loudest one's gain"
    );
}

/// The key drives the detector, not the signal: a quiet signal under a loud key is
/// reduced; a loud signal under a silent key (an unwired key) is not touched.
#[test]
fn the_key_and_not_the_signal_drives_the_detector() {
    let n = 4000;
    let c = controls(-20.0, 0.0, 4.0, 0.001, 0.05, 0.0);
    let quiet = vec![0.01f32; n];
    let loud = vec![0.9f32; n];
    let silent = vec![0.0f32; n];

    let mut k = runner(&c);
    let out = run(&mut k, &[&quiet, &quiet], &[&loud, &loud], None);
    assert!(
        out[0][n - 1] < 0.5 * 0.01,
        "a loud key ducks a quiet signal"
    );

    let mut k = runner(&c);
    let out = run(&mut k, &[&loud, &loud], &[&silent, &silent], None);
    assert!(
        out[0].iter().all(|&x| x == 0.9),
        "a silent key reduced a loud signal"
    );
}

/// An own key is the signal itself: a loud signal ducks itself by the static curve while a
/// quiet one under the threshold is not touched (which a side-chain keyed by silence would do
/// to a loud one too).
#[test]
fn an_own_key_detects_the_signal_itself() {
    let n = 30_000;
    let c = controls(-20.0, 0.0, 4.0, 0.001, 0.05, 0.0);
    for level in [-10.0f64, -30.0] {
        let x = vec![amplitude(level); n];
        let mut k = runner(&c);
        let out = run(&mut k, &[&x, &x], &[&x, &x], None);
        let want = -curve(level, -20.0, 0.0, 4.0);
        let got = 20.0 * (f64::from(out[0][n - 1]) / f64::from(x[0])).log10();
        assert!(
            (got - want).abs() < 2e-3,
            "a signal at {level} dB: {got} dB vs {want}"
        );
    }
}

/// A threshold control signal is read per frame: with the follower instant, each frame's gain is the
/// curve at that frame's threshold, and a frame that is not finite takes the set one.
#[test]
fn a_threshold_signal_is_read_per_frame() {
    let n = 600;
    let base = -40.0f64;
    let c = controls(base, 0.0, 4.0, 0.0, 0.0, 0.0);
    let level = -6.0;
    let signal: Vec<f32> = (0..n)
        .map(|i| match i {
            0..100 => 20.0,
            100..200 => -10.0,
            200..300 => f32::NAN,
            300..400 => f32::INFINITY,
            _ => -25.0,
        })
        .collect();
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let key = vec![amplitude(level); n];
    let out = run(&mut k, &[&ones, &ones], &[&key, &key], Some(&signal));
    for (i, &t) in signal.iter().enumerate() {
        let threshold = if t.is_finite() { f64::from(t) } else { base };
        let want = -curve(level, threshold, 0.0, 4.0);
        let got = 20.0 * f64::from(out[0][i]).log10();
        assert!(
            (got - want).abs() < 2e-3,
            "frame {i}: threshold {t}: {got} dB vs {want}"
        );
    }
}

/// The detector's controls land whole: a ratio that moves is in the next frame's gain,
/// not ramped to (the follower is the smoother), and a new target that moves the ratio only
/// takes no makeup step.
#[test]
fn a_moved_detector_control_lands_at_once() {
    let n = 100;
    let (a, b) = (
        controls(-20.0, 0.0, 2.0, 0.0, 0.0, 0.0),
        controls(-20.0, 0.0, 8.0, 0.0, 0.0, 0.0),
    );
    let mut k = runner(&a);
    let ones = vec![1.0f32; n];
    let key = vec![amplitude(-8.0); n];
    let _ = run(&mut k, &[&ones, &ones], &[&key, &key], None);
    k.retarget(&b);
    let out = run(&mut k, &[&ones, &ones], &[&key, &key], None);
    let want = -curve(-8.0, -20.0, 0.0, 8.0);
    let got = 20.0 * f64::from(out[0][0]).log10();
    assert!(
        (got - want).abs() < 2e-3,
        "{got} dB vs {want} on the first frame"
    );
}

/// A makeup that moves ramps over [`GAIN_RAMP`] (5 ms: 221, 240 and 480 frames at 44.1, 48
/// and 96 kHz) in a straight line in dB, frame `i` being `(i + 1) / n` of the way, and is
/// exactly its value after; no frame steps.
#[test]
fn a_moved_makeup_ramps_over_five_milliseconds() {
    for (rate, frames) in [(RATES[0], 221usize), (RATES[1], 240), (RATES[2], 480)] {
        assert_eq!((0.005 * rate.get()).ceil() as usize, frames);
        let (a, b) = (
            controls(0.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            controls(0.0, 0.0, 1.0, 0.0, 0.0, 12.0),
        );
        let mut k = runner_at(&a, rate);
        k.retarget(&b);
        let n = 2 * frames;
        let ones = vec![1.0f32; n];
        let silent = vec![0.0f32; n];
        let out = run(&mut k, &[&ones, &ones], &[&silent, &silent], None);
        let mut previous = 1.0f64;
        for (i, &x) in out[0].iter().enumerate() {
            let db = 12.0 * ((i + 1).min(frames) as f64 / frames as f64);
            let want = 10f64.powf(db / 20.0);
            assert!(
                (f64::from(x) / want - 1.0).abs() < 1e-5,
                "{rate:?} frame {i}: {x} vs {want}"
            );
            // No step: a frame is within a 12 dB ramp's own increment of the last.
            assert!(
                f64::from(x) / previous < 1.0 + 0.0013 * 12.0,
                "{rate:?}: a step at {i}"
            );
            previous = f64::from(x);
        }
    }
}

/// A makeup that does not move is applied exactly from the first frame (a bank at rest on
/// it has nothing ramping), and with nothing to reduce the gain is its amplitude.
#[test]
fn a_held_makeup_applies_exactly() {
    let c = controls(0.0, 0.0, 1.0, 0.0, 0.0, 6.0);
    let mut k = runner(&c);
    let n = 50;
    let ones = vec![1.0f32; n];
    let silent = vec![0.0f32; n];
    let out = run(&mut k, &[&ones, &ones], &[&silent, &silent], None);
    let want = 10f64.powf(6.0 / 20.0);
    for &x in &out[0] {
        assert!((f64::from(x) / want - 1.0).abs() < 1e-6, "{x} vs {want}");
    }
}

/// The compressor's `f64` bank at 48 kHz on a steady key `level` dB with the static reduction
/// `target`: the reduction after `frames` frames of an attack of `tau` seconds from nothing,
/// `target * (1 - a^frames)` with `a = exp(-1 / (tau rate))`.
fn attacked(target: f64, tau: f64, frames: usize) -> f64 {
    target * (1.0 - (-(frames as f64) / (tau * RATE.get())).exp())
}

/// The gain of `makeup` dB and `reduction` dB, from the definition.
fn gain_of(makeup: f64, reduction: f64) -> f64 {
    10f64.powf((makeup - reduction) / 20.0)
}

/// The render is the closed form whatever the calls cut the stream into: a key that steps from
/// `L1` to a louder `L2` at frame 517 (inside a block of 7, 33, 64, 100 and 255 frames alike)
/// attacks along the one-pole toward the static curve at the control signal's threshold, `T1 (1 - a^n)`
/// and then `T2 + (R - T2) a^m` from the reduction `R` the step found, and the makeup ramps
/// linearly in dB over the 240 steps of 5 ms at 48 kHz, in calls of 1, 7, 33, 64, 100 and 255
/// frames and in one call.
#[test]
fn a_cut_stream_follows_the_one_pole_and_ramp_closed_forms() {
    let (signal_threshold, ratio, tau) = (-20.0, 4.0, 0.002);
    let (first, second) = (-14.0, -6.0);
    let (steps, makeup, step_at) = (240usize, 9.0, 517usize);
    let n = 1_200;
    let (t1, t2) = (
        curve(first, signal_threshold, 0.0, ratio),
        curve(second, signal_threshold, 0.0, ratio),
    );
    let a = (-1.0 / (tau * RATE.get())).exp();
    let reduction = |frames: usize| {
        if frames <= step_at {
            attacked(t1, tau, frames)
        } else {
            t2 + (attacked(t1, tau, step_at) - t2) * a.powi((frames - step_at) as i32)
        }
    };
    let c = controls(-40.0, 0.0, ratio, tau, 0.03, 0.0);
    let to = controls(-40.0, 0.0, ratio, tau, 0.03, makeup);
    let ones = vec![1.0f64; n];
    let key: Vec<f64> = (0..n)
        .map(|i| 10f64.powf(if i < step_at { first } else { second } / 20.0))
        .collect();
    let signal = vec![signal_threshold; n];
    for block in [1, 7, 33, 64, 100, 255, n] {
        let mut k = bank_at::<f64>(&c, RATE);
        k.retarget(&to);
        let out = run_in(&mut k, &[&ones], &[&key], Some(&signal), block).remove(0);
        for (i, &y) in out.iter().enumerate() {
            let ramp = makeup * ((i + 1).min(steps) as f64 / steps as f64);
            let want = gain_of(ramp, reduction(i + 1));
            assert!(
                (y / want - 1.0).abs() < 1e-12,
                "blocks of {block}, frame {i}: {y} vs {want}"
            );
        }
    }
}

/// A reset bank attacks from nothing along the one-pole closed form, with the makeup at rest on
/// its target: after wear under other controls and a reset, a steady key over the threshold gives
/// `10^((makeup - target (1 - a^n)) / 20)` from its first frame.
#[test]
fn a_reset_bank_attacks_from_nothing_along_the_closed_form() {
    let (threshold, knee, ratio, tau, makeup, level) = (-24.0, 6.0, 6.0, 0.001, 4.0, -6.0);
    let n = 2_000;
    let c = controls(threshold, knee, ratio, tau, 0.05, makeup);
    let worn_by = noise(3, n, 0.7);
    let wear: Vec<f64> = worn_by.iter().map(|&x| f64::from(x)).collect();
    let mut worn = bank_at::<f64>(&c, RATE);
    let _ = run(&mut worn, &[&wear], &[&wear], None);
    worn.retarget(&controls(-10.0, 0.0, 2.0, 0.01, 0.1, -3.0));
    let _ = run(&mut worn, &[&wear], &[&wear], None);
    worn.retarget(&c);
    worn.reset();
    let target = curve(level, threshold, knee, ratio);
    let ones = vec![1.0f64; n];
    let key = vec![10f64.powf(level / 20.0); n];
    let out = run(&mut worn, &[&ones], &[&key], None).remove(0);
    for (i, &y) in out.iter().enumerate() {
        let want = gain_of(makeup, attacked(target, tau, i + 1));
        assert!((y / want - 1.0).abs() < 1e-12, "frame {i}: {y} vs {want}");
    }
}

/// A clone continues the one-pole where the original is and shares nothing with it: both follow
/// the closed form on from the frame they were cloned at, and what the clone is set to and
/// rendered under leaves the original's closed form unmoved.
#[test]
fn a_clone_continues_along_the_closed_form_and_shares_nothing() {
    let (threshold, ratio, tau, level) = (-24.0, 6.0, 0.003, -6.0);
    let c = controls(threshold, 0.0, ratio, tau, 0.05, 4.0);
    let target = curve(level, threshold, 0.0, ratio);
    let (before, after) = (700usize, 900usize);
    let ones = vec![1.0f64; after];
    let key = vec![10f64.powf(level / 20.0); after];
    let mut a = bank_at::<f64>(&c, RATE);
    let _ = run(&mut a, &[&ones[..before]], &[&key[..before]], None);
    let mut b = a.clone();
    let on = |out: &[f64], from: usize, what: &str| {
        for (j, &y) in out.iter().enumerate() {
            let want = gain_of(4.0, attacked(target, tau, from + j + 1));
            assert!(
                (y / want - 1.0).abs() < 1e-12,
                "{what}, frame {j}: {y} vs {want}"
            );
        }
    };
    on(
        &run(&mut b, &[&ones], &[&key], None)[0],
        before,
        "the clone",
    );
    // The clone moves to other controls and runs on; the original has not heard of it.
    b.retarget(&controls(-40.0, 0.0, 20.0, 0.0, 0.0, 0.0));
    let _ = run(&mut b, &[&ones], &[&key], None);
    on(
        &run(&mut a, &[&ones], &[&key], None)[0],
        before,
        "the original",
    );
}

/// A control below its floor or not finite is the floor or the default, and the render is
/// finite: the follower never keeps a NaN, and a NaN key sample reads as quiet.
#[test]
fn hostile_controls_and_keys_render_finite() {
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
        // A finite key afterwards recovers: the output is finite again.
        let after = run(&mut k, &[&audio, &audio], &[&audio, &audio], None);
        assert!(after[0].iter().all(|x| x.is_finite()), "{c:?} after");
    }
}

/// An infinite key sample, own or side-chain, reads as the largest finite level: the follower
/// stays finite and a finite key afterwards gives the gain of the static curve again, at
/// ratio 1 (where `inf * 0` would be NaN) and at ratio 4 (where the follower would take
/// `inf + c (t - inf)`). A NaN key sample reads as quiet.
#[test]
fn an_infinite_key_does_not_latch_the_follower() {
    let n = 40_000;
    let level = -10.0;
    for ratio in [1.0f64, 4.0] {
        let c = controls(-20.0, 0.0, ratio, 0.0, 0.01, 0.0);
        let mut k = runner(&c);
        let mut key = vec![amplitude(level); n];
        key[5] = f32::INFINITY;
        key[6] = f32::NEG_INFINITY;
        key[7] = f32::NAN;
        let ones = vec![1.0f32; n];
        let out = run(&mut k, &[&ones, &ones], &[&key, &key], None);
        assert!(out[0].iter().all(|x| x.is_finite()), "ratio {ratio}");
        // After the release has run out the gain is the curve's again.
        let want = -curve(level, -20.0, 0.0, ratio);
        let got = 20.0 * f64::from(out[0][n - 1]).log10();
        assert!(
            (got - want).abs() < 2e-3,
            "ratio {ratio}: {got} dB vs {want}"
        );
        // And a silent key lets go of it: silent input is finite and unity.
        let quiet = vec![0.0f32; n];
        let out = run(&mut k, &[&ones, &ones], &[&quiet, &quiet], None);
        assert!(out[0].iter().all(|x| x.is_finite()), "ratio {ratio}, quiet");
    }
}

/// A makeup past what a finite gain can hold reads as the most: silent input stays silent
/// and finite, with the makeup at 1e30 dB.
#[test]
fn a_makeup_past_the_finite_range_is_clamped() {
    let c = controls(0.0, 0.0, 1.0, 0.0, 0.0, 1.0e30);
    let mut k = runner(&c);
    let silent = vec![0.0f32; 200];
    let out = run(&mut k, &[&silent, &silent], &[&silent, &silent], None);
    assert!(out[0].iter().all(|&x| x == 0.0), "silence stays silent");
    let mut k = runner(&c);
    let tiny = vec![1.0e-30f32; 200];
    let out = run(&mut k, &[&tiny, &tiny], &[&silent, &silent], None);
    assert!(out[0].iter().all(|x| x.is_finite()));
}

/// The key keeps being followed while the main signal is silent: a loud key over a silent
/// main settles the reduction, and when the main resumes under the same key it is reduced by
/// that settled amount from its first frame, the curve's (not from nothing).
#[test]
fn a_keyed_follower_keeps_tracking_while_the_main_signal_is_silent() {
    let c = controls(-20.0, 0.0, 4.0, 0.002, 0.2, 0.0);
    let level = -8.0;
    let n = 20_000;
    let mut k = runner(&c);
    let (silent, ones) = (vec![0.0f32; n], vec![1.0f32; n]);
    let key = vec![amplitude(level); n];
    let quiet_main = run(&mut k, &[&silent, &silent], &[&key, &key], None);
    assert!(quiet_main[0].iter().all(|&x| x == 0.0));
    let resumed = run(&mut k, &[&ones, &ones], &[&key, &key], None);
    let want = -curve(level, -20.0, 0.0, 4.0);
    let first = 20.0 * f64::from(resumed[0][0]).log10();
    assert!(
        (first - want).abs() < 2e-3,
        "first frame {first} dB vs {want}"
    );
}

/// The deepest reduction since the last take is the curve's value at a steady key (a hard
/// knee, an instant attack, a release that holds), is read once and starts again from
/// nothing, and a reset clears it.
#[test]
fn the_deepest_reduction_is_read_once_and_cleared_by_a_reset() {
    let c = controls(-20.0, 0.0, 4.0, 0.0, f64::INFINITY, 0.0);
    let mut bank = runner(&c);
    let level = -6.0;
    let x = vec![amplitude(level); 4_800];
    run(&mut bank, &[&x], &[&x], None);
    let got = bank.take_deepest().get();
    let want = curve(level, -20.0, 0.0, 4.0);
    assert!((got - want).abs() < 1e-3, "{got} vs {want}");
    assert_eq!(bank.take_deepest(), Db::UNITY, "a read starts again");
    // The held follower reads the same depth again at once; a reset forgets it.
    let quiet = vec![0.0f32; 64];
    run(&mut bank, &[&quiet], &[&quiet], None);
    assert!(bank.take_deepest().get() > want - 1e-3);
    run(&mut bank, &[&quiet], &[&quiet], None);
    bank.reset();
    assert_eq!(bank.take_deepest(), Db::UNITY);
}

/// A knee past 400 dB is held there: a knee of 1e20 or 1e200 renders finite with the key inside
/// it (1e200 is a `Db` `f64` holds and its square overflows), and once the key has left (silence,
/// then a steady -30 dB) the gain is back on the static curve of the 400 dB knee.
#[test]
fn a_huge_knee_is_held_and_the_gain_returns_to_the_static_curve() {
    for knee in [1.0e20f64, 1.0e200] {
        let mut bank = runner(&controls(-20.0, knee, 4.0, 0.001, 0.001, 0.0));
        let inside = vec![amplitude(-6.0); 4_800];
        let out = run(&mut bank, &[&inside], &[&inside], None);
        assert!(out[0].iter().all(|s| s.is_finite()), "knee {knee}: inside");
        let silence = vec![0.0f32; 4_800];
        let out = run(&mut bank, &[&silence], &[&silence], None);
        assert!(out[0].iter().all(|s| s.is_finite()), "knee {knee}: silence");
        let level = -30.0;
        let x = vec![amplitude(level); 9_600];
        let out = run(&mut bank, &[&x], &[&x], None);
        let got = f64::from(out[0][9_599] / x[9_599]);
        let want = 10f64.powf(-curve(level, -20.0, 400.0, 4.0) / 20.0);
        assert!(
            (got - want).abs() < 1e-3 * want,
            "knee {knee}: gain {got} vs {want}"
        );
    }
}

/// `audio` through an `f64` bank at 48 kHz set to `c` and at rest, in one block, keyed by
/// `key`.
fn run_f64(c: &CompressorControls, audio: &[f64], key: &[f64]) -> Vec<f64> {
    let mut bank = CompressorBank::<f64>::new(RATE);
    bank.retarget(c);
    bank.jump_to_target();
    let mut out = vec![0.0f64; audio.len()];
    bank.process(
        CompressorSignals::default(),
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

/// A key level `-12.04 dB` or so that no `f32` holds: the value `wide_signal` starts at,
/// held for `len` frames.
fn wide_constant(len: usize) -> Vec<f64> {
    vec![tutti_fixtures::signal::wide_signal(1)[0]; len]
}

/// Under the threshold the reduction is exactly 0 and the gain `10^0` is 1: an `f64` signal
/// of more than 24 bits comes out as it went in, whatever it is keyed by, so nothing in the
/// path narrows it.
#[test]
fn an_f64_compressor_under_its_threshold_is_the_identity_at_0_db_makeup() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(300);
    let c = controls(0.0, 6.0, 4.0, 0.002, 0.05, 0.0);
    assert_close_f64(&run_f64(&c, &x, &x), &x, 0.0);
}

/// At a constant key the reduction is the one-pole `y[n] = y[n-1] + c (target - y[n-1])`
/// toward the curve's reduction, `c = 1 - exp(-1 / (tau rate))`, and the gain is
/// `10^(-y[n] / 20)`: the closed form in `f64`, to 1e-12.
#[test]
fn an_f64_compressor_follows_the_one_pole_closed_form_in_f64() {
    use tutti_fixtures::signal::assert_close_f64;
    let (threshold, ratio, tau) = (-30.0, 4.0, 0.002);
    let key = wide_constant(2_000);
    let k = key[0];
    let c = controls(threshold, 0.0, ratio, tau, 0.05, 0.0);
    let target = (20.0 * k.log10() - threshold) * (1.0 - 1.0 / ratio);
    let coeff = 1.0 - (-1.0 / (tau * RATE.get())).exp();
    let mut reduction = 0.0f64;
    let want: Vec<f64> = key
        .iter()
        .map(|&x| {
            reduction += coeff * (target - reduction);
            x * 10f64.powf(-reduction / 20.0)
        })
        .collect();
    assert_close_f64(&run_f64(&c, &key, &key), &want, 1e-12);
}

/// With instant times the reduction is the curve's at once, so a constant key `L` dB over
/// the threshold `T` comes out at `T + (L - T) / R` dB: the static curve of a hard knee, to
/// 1e-12 dB.
#[test]
fn an_f64_compressor_above_its_threshold_lands_on_the_static_curve_in_f64() {
    let (threshold, ratio) = (-30.0, 5.0);
    let key = wide_constant(64);
    let c = controls(threshold, 0.0, ratio, 0.0, 0.0, 0.0);
    let level = 20.0 * key[0].log10();
    let out = run_f64(&c, &key, &key);
    let got = 20.0 * out[63].log10();
    let want = threshold + (level - threshold) / ratio;
    assert!((got - want).abs() < 1e-12, "{got} dB against {want} dB");
}

/// An f32 compressor at 192 kHz with an instant attack and a release of `tau` seconds, its
/// reduction put on the static curve's 9 dB by a loud key and then released under silence: the
/// reduction in dB the render shows at each of `probes` (frames into the release, ascending),
/// run in chunks so a long release does not hold the render in memory.
fn released_f32(tau: f64, probes: &[usize]) -> Vec<f64> {
    const CHUNK_FRAMES: usize = 1 << 16;
    let mut k = runner_at(
        &controls(-20.0, 0.0, 4.0, 0.0, tau, 0.0),
        SampleRate::SR_192K,
    );
    let _ = run(
        &mut k,
        &[&[1.0f32; 64]],
        &[&[amplitude(RELEASE_LEVEL); 64]],
        None,
    );
    let (ones, quiet) = (vec![1.0f32; CHUNK_FRAMES], vec![0.0f32; CHUNK_FRAMES]);
    let end = probes.last().map_or(0, |&p| p + 1);
    let (mut seen, mut shown) = (0, Vec::new());
    while seen < end {
        let n = CHUNK_FRAMES.min(end - seen);
        let out = run(&mut k, &[&ones[..n]], &[&quiet[..n]], None).remove(0);
        for &p in probes.iter().filter(|&&p| (seen..seen + n).contains(&p)) {
            shown.push(-20.0 * f64::from(out[p - seen]).log10());
        }
        seen += n;
    }
    shown
}

/// The key level that puts the release tests' reduction on the 4:1 curve's 9 dB at -20 dB.
const RELEASE_LEVEL: f64 = -8.0;

/// The f32 coefficient of a release of `tau` seconds at 192 kHz, as `f64`.
fn release_coeff(tau: f64) -> f64 {
    f64::from(Seconds::new(tau).one_pole_coeff(SampleRate::SR_192K) as f32)
}

/// The deviation from the closed form an f32 compensated one-pole of coefficient `c` shows in a
/// reduction `r0` dB falling toward 0: each step's product `c r` rounds by at most `2^-24` of
/// itself, a stationary `2^-24 r0` in the value (the error decays at `c` a step); the value is
/// within half an ulp of the sum it carries, `2^(e - 24)` for `r0` in `[2^e, 2^(e+1))`; and the
/// gain `10^(-r/20)` rounds in powf by up to `2^-23` relative, which is `20 log10 e 2^-23` dB.
fn f32_compensated_bound(r0: f64) -> f64 {
    let half_ulp = 2f64.powi(r0.log2().floor() as i32 - 24);
    2f64.powi(-24) * r0 + half_ulp + 20.0 / std::f64::consts::LN_10 * 2f64.powi(-23)
}

/// The slowest release the documentation promises, 10 s at 192 kHz, holds its time constant in
/// f32: its coefficient is `5.2e-7`, kept as it is with the part each step's rounding drops, so the
/// reduction falls along `target a^n` with `a = 1 - c` (the f32 coefficient) to within
/// [`f32_compensated_bound`] over two time constants, probed every 997 frames of them, and one
/// `tau` leaves `1/e` of the reduction.
#[test]
fn an_f32_ten_second_release_at_192_khz_holds_its_closed_form_time_constant() {
    let (tau, rate) = (10.0, SampleRate::SR_192K.get());
    let target = curve(RELEASE_LEVEL, -20.0, 0.0, 4.0);
    let n = (2.0 * tau * rate) as usize;
    let probes: Vec<usize> = (0..n).step_by(997).collect();
    let shown = released_f32(tau, &probes);
    let a = (-release_coeff(tau)).ln_1p().exp();
    let bound = f32_compensated_bound(target);
    let worst = probes
        .iter()
        .zip(&shown)
        .map(|(&i, &got)| (got - target * a.powi(i as i32 + 1)).abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= bound,
        "{worst} dB off the closed form, bound {bound}"
    );
    let one = released_f32(tau, &[(tau * rate) as usize - 1])[0];
    assert!(
        (one / target - (-1.0f64).exp()).abs() < 1e-4,
        "{one} of {target}"
    );
}

/// A release of 100 s and of 300 s at 192 kHz (coefficients `5.2e-8` and `1.7e-8`, under the
/// `2^-24` where a step of `c r` is under half an ulp of `r`) still releases in f32: over a
/// quarter of a time constant the reduction follows `target a^n` to within
/// [`f32_compensated_bound`], where the plain recurrence `r - c r` computed here stays frozen at
/// the 9 dB it started from, never releasing.
#[test]
fn an_f32_hundred_and_three_hundred_second_release_still_falls_along_the_closed_form() {
    let (rate, target) = (
        SampleRate::SR_192K.get(),
        curve(RELEASE_LEVEL, -20.0, 0.0, 4.0),
    );
    for tau in [100.0, 300.0] {
        let c = release_coeff(tau);
        let n = (0.25 * tau * rate) as usize;
        let mut plain = target as f32;
        for _ in 0..1_000 {
            plain += -(c as f32) * plain;
        }
        assert_eq!(
            f64::from(plain),
            f64::from(target as f32),
            "{tau} s: the plain recurrence moved"
        );
        let at = [n / 4, n / 2, n - 1];
        let a = (-c).ln_1p().exp();
        for (&i, got) in at.iter().zip(released_f32(tau, &at)) {
            let want = target * a.powi(i as i32 + 1);
            assert!(
                (got - want).abs() <= f32_compensated_bound(target),
                "{tau} s at {i}: {got} dB vs {want}"
            );
            assert!(target - got > 0.1, "{tau} s: the reduction never released");
        }
    }
}
