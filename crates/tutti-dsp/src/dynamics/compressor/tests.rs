use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_types::{CompressionRatio, Db, SampleRate, Seconds};

use super::*;
use crate::contract::noise;

const RATE: SampleRate = SampleRate::SR_48K;
/// The rates the static and time-constant tests run at.
const RATES: [SampleRate; 3] = [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K];
/// The longest block `run` hands the bank.
const CHUNK: usize = 256;

fn controls(
    threshold: f32,
    knee: f32,
    ratio: f32,
    attack: f64,
    release: f64,
    makeup: f32,
) -> CompressorControls {
    CompressorControls {
        threshold: Db(threshold),
        knee: Db(knee),
        ratio: CompressionRatio(ratio),
        attack: Seconds(attack),
        release: Seconds(release),
        makeup: Db(makeup),
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
    let mut rng = StdRng::seed_from_u64(24);
    all.extend((0..64).map(|_| {
        controls(
            rng.random_range(-60.0..6.0),
            rng.random_range(0.0..24.0),
            rng.random_range(1.0..30.0),
            rng.random_range(0.0..0.05),
            rng.random_range(0.0..0.5),
            rng.random_range(-12.0..12.0),
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
            knee: Db(-6.0),
            ..base
        },
        CompressorControls {
            ratio: CompressionRatio(0.25),
            ..base
        },
        CompressorControls {
            attack: Seconds(-1.0),
            release: Seconds(-1.0),
            ..base
        },
    ];
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        all.push(CompressorControls {
            threshold: Db(bad),
            knee: Db(bad),
            ratio: CompressionRatio(bad),
            makeup: Db(bad),
            ..base
        });
        all.push(CompressorControls {
            attack: Seconds(f64::from(bad)),
            release: Seconds(f64::from(bad)),
            ..base
        });
    }
    all
}

/// A bank at `rate` at rest on `c`: aimed and landed, nothing gliding.
fn runner_at(c: &CompressorControls, rate: SampleRate) -> CompressorBank {
    let mut bank = CompressorBank::new(rate);
    bank.aim(c);
    bank.settle();
    bank
}

/// [`runner_at`] at 48 kHz.
fn runner(c: &CompressorControls) -> CompressorBank {
    runner_at(c, RATE)
}

/// `audio` (one slice per channel) through `bank`, keyed by `key`, with `lane` as the
/// threshold lane when given, in calls of at most `block` frames.
fn run_in(
    bank: &mut CompressorBank,
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
        let lanes = CompressorLanes {
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
    bank: &mut CompressorBank,
    audio: &[&[f32]],
    key: &[&[f32]],
    lane: Option<&[f32]>,
) -> Vec<Vec<f32>> {
    run_in(bank, audio, key, lane, CHUNK)
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

/// The member's `reduction_db` is the static curve: nothing under the knee, the
/// straight line over it, the quadratic between, and the three agree at the knee's two
/// edges (the curve is continuous).
#[test]
fn the_member_reduction_is_the_static_curve() {
    for c in reachable() {
        let m = CompressorMember::designed(&c, RATE);
        let (t, w, r) = (
            f64::from(m.threshold.get()),
            f64::from(m.knee.get()),
            f64::from(m.ratio.get()),
        );
        for step in -80..=40 {
            let level = f64::from(step) * 1.5 - 20.0;
            let want = curve(level, t, w, r);
            let got = f64::from(m.reduction_db(Db(level as f32)).get());
            assert!(
                (got - want).abs() < 2e-3,
                "{c:?} at {level}: {got} vs {want}"
            );
        }
        // Continuity at both edges: the two pieces meeting there give the same value.
        for edge in [t - w / 2.0, t + w / 2.0] {
            let (below, above) = (
                f64::from(m.reduction_db(Db((edge - 1e-3) as f32)).get()),
                f64::from(m.reduction_db(Db((edge + 1e-3) as f32)).get()),
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
    assert_eq!(m.reduction_db(Db(-40.0)), Db(0.0));
    assert_eq!(m.reduction_db(Db(-25.0)), Db(0.0));
    assert!((m.reduction_db(Db(-10.0)).get() - 7.5).abs() < 1e-5);
    assert!((m.reduction_db(Db(0.0)).get() - 15.0).abs() < 1e-5);
    // At the threshold the quadratic has run a half of the knee: (3/4)(5)^2/(2*10).
    assert!((m.reduction_db(Db(-20.0)).get() - 0.75 * 25.0 / 20.0).abs() < 1e-5);
}

/// The member holds the controls made runnable: a knee under 0 reads as 0, a ratio under
/// 1 as 1, a time under 0 as 0, and a control that is not finite as the default's.
#[test]
fn the_member_floors_what_cannot_be_run() {
    let base = controls(-18.0, 6.0, 4.0, 0.003, 0.08, 3.0);
    let m = CompressorMember::designed(
        &CompressorControls {
            knee: Db(-6.0),
            ratio: CompressionRatio(0.25),
            attack: Seconds(-1.0),
            release: Seconds(-0.5),
            ..base
        },
        RATE,
    );
    assert_eq!(
        (m.knee, m.ratio, m.attack, m.release),
        (Db(0.0), CompressionRatio(1.0), Seconds(0.0), Seconds(0.0))
    );
    let d = CompressorControls::DEFAULT;
    let bad = CompressorMember::designed(
        &CompressorControls {
            threshold: Db(f32::NAN),
            knee: Db(f32::INFINITY),
            ratio: CompressionRatio(f32::NAN),
            makeup: Db(f32::NEG_INFINITY),
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
            let want_db = -curve(
                level,
                f64::from(c.threshold.get()),
                f64::from(c.knee.get()),
                f64::from(c.ratio.get()),
            ) + f64::from(c.makeup.get());
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

/// A threshold lane is read per frame: with the follower instant, each frame's gain is the
/// curve at that frame's threshold, and a frame that is not finite takes the aimed one.
#[test]
fn a_threshold_lane_is_read_per_frame() {
    let n = 600;
    let base = -40.0f32;
    let c = controls(base, 0.0, 4.0, 0.0, 0.0, 0.0);
    let level = -6.0;
    let lane: Vec<f32> = (0..n)
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
    let out = run(&mut k, &[&ones, &ones], &[&key, &key], Some(&lane));
    for (i, &t) in lane.iter().enumerate() {
        let threshold = if t.is_finite() {
            f64::from(t)
        } else {
            f64::from(base)
        };
        let want = -curve(level, threshold, 0.0, 4.0);
        let got = 20.0 * f64::from(out[0][i]).log10();
        assert!(
            (got - want).abs() < 2e-3,
            "frame {i}: threshold {t}: {got} dB vs {want}"
        );
    }
}

/// The detector's controls land whole: a ratio that moves is in the next frame's gain,
/// not glided to (the follower is the smoother), and a retarget that moves the ratio only
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
    k.aim(&b);
    let out = run(&mut k, &[&ones, &ones], &[&key, &key], None);
    let want = -curve(-8.0, -20.0, 0.0, 8.0);
    let got = 20.0 * f64::from(out[0][0]).log10();
    assert!(
        (got - want).abs() < 2e-3,
        "{got} dB vs {want} on the first frame"
    );
}

/// A makeup that moves glides over [`GAIN_GLIDE`] (5 ms: 221, 240 and 480 frames at 44.1, 48
/// and 96 kHz) in a straight line in dB, frame `i` being `(i + 1) / n` of the way, and is
/// exactly its value after; no frame steps.
#[test]
fn a_moved_makeup_glides_over_five_milliseconds() {
    for (rate, frames) in [(RATES[0], 221usize), (RATES[1], 240), (RATES[2], 480)] {
        assert_eq!((0.005 * rate.get()).ceil() as usize, frames);
        let (a, b) = (
            controls(0.0, 0.0, 1.0, 0.0, 0.0, 0.0),
            controls(0.0, 0.0, 1.0, 0.0, 0.0, 12.0),
        );
        let mut k = runner_at(&a, rate);
        k.aim(&b);
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
            // No step: a frame is within a 12 dB glide's own increment of the last.
            assert!(
                f64::from(x) / previous < 1.0 + 0.0013 * 12.0,
                "{rate:?}: a step at {i}"
            );
            previous = f64::from(x);
        }
    }
}

/// A makeup that does not move is applied exactly from the first frame (a bank at rest on
/// it has nothing gliding), and with nothing to reduce the gain is its amplitude.
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

/// The render does not depend on how the stream is cut: calls of 1, 7, 33, 64, 100 and 255
/// frames give the bits of one call, with a threshold lane, a key that moves and a makeup
/// glide in flight.
#[test]
fn a_split_stream_renders_the_bits_of_the_whole() {
    let n = 3000;
    let c = controls(-20.0, 6.0, 4.0, 0.002, 0.03, 0.0);
    let to = controls(-20.0, 6.0, 4.0, 0.002, 0.03, 9.0);
    let audio = noise(1, n, 0.8);
    let key = noise(2, n, 0.9);
    let lane: Vec<f32> = (0..n).map(|i| -30.0 + (i % 700) as f32 * 0.05).collect();
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

/// A reset bank renders as a fresh one, bit for bit: the follower is empty and the makeup
/// landed on its target.
#[test]
fn a_reset_bank_renders_as_a_fresh_one() {
    let n = 2000;
    let c = controls(-24.0, 6.0, 6.0, 0.001, 0.05, 4.0);
    let audio = noise(3, n, 0.7);
    let key = noise(4, n, 0.9);
    let mut worn = runner(&c);
    let _ = run(&mut worn, &[&audio], &[&key], None);
    worn.aim(&controls(-10.0, 0.0, 2.0, 0.01, 0.1, -3.0));
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
    let c = controls(-24.0, 6.0, 6.0, 0.001, 0.05, 4.0);
    let audio = noise(5, n, 0.7);
    let key = noise(6, n, 0.9);
    let mut a = runner(&c);
    let _ = run(&mut a, &[&audio], &[&key], None);
    let mut b = a.clone();
    let from_a = run(&mut a, &[&audio], &[&key], None);
    assert_eq!(run(&mut b, &[&audio], &[&key], None), from_a);
    b.aim(&controls(-40.0, 0.0, 20.0, 0.0, 0.0, 0.0));
    let _ = run(&mut b, &[&audio], &[&key], None);
    assert_eq!(
        run(&mut a.clone(), &[&audio], &[&key], None),
        run(&mut a, &[&audio], &[&key], None)
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
    for ratio in [1.0f32, 4.0] {
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
        let want = -curve(level, -20.0, 0.0, f64::from(ratio));
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

/// A render over `f64` views is the `f32` render converted: the state is `f32`, the views are
/// converted at the edges, and an `f32` input is exact in `f64`, so the two agree to the
/// last bit (a tolerance of 0), with a threshold lane and a makeup glide in flight.
#[test]
fn an_f64_render_is_the_f32_render_converted() {
    let n = 2_000;
    let audio = noise(51, n, 0.9);
    let key = noise(52, n, 0.9);
    let lane: Vec<f32> = (0..n).map(|i| -30.0 + (i % 400) as f32 * 0.05).collect();
    let (c, to) = (
        controls(-20.0, 6.0, 4.0, 0.002, 0.05, 0.0),
        controls(-20.0, 6.0, 4.0, 0.002, 0.05, 6.0),
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
            CompressorLanes {
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
