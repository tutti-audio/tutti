use crate::testing::{noise, uniform};
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::{Db, SampleRate, Seconds};

use super::*;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
/// 5 ms at 48 kHz: 240 frames.
const LOOKAHEAD: Lookahead = Lookahead::of(Seconds::new(0.005));
const L: usize = 240;
/// The longest block `run` hands the bank.
const CHUNK: usize = 256;

fn controls(threshold: f64, ceiling: f64, release: f64) -> LimiterControls {
    LimiterControls {
        threshold: Db::new(threshold),
        ceiling: Db::new(ceiling),
        release: Seconds::new(release),
    }
}

/// A grid and seeded draws across every control.
fn reachable() -> Vec<LimiterControls> {
    let mut all = Vec::new();
    for threshold in [-30.0, -12.0, -6.0, 0.0] {
        for ceiling in [-12.0, -6.0, -0.3, 0.0] {
            for release in [0.001, 0.02, 0.1, 0.5] {
                all.push(controls(threshold, ceiling, release));
            }
        }
    }
    let mut rng = SplitMix64::new(24);
    all.extend((0..64).map(|_| {
        controls(
            uniform(&mut rng, -40.0, 6.0),
            uniform(&mut rng, -20.0, 3.0),
            uniform(&mut rng, 0.0, 1.0),
        )
    }));
    all
}

/// A release under the floor, and each control not finite (a threshold or a ceiling only
/// fails at NaN: `+-inf` are levels): each is a control the bank can run.
fn hostile() -> Vec<LimiterControls> {
    let base = controls(-6.0, -0.3, 0.1);
    let mut all = vec![
        LimiterControls {
            release: Seconds::new(-1.0),
            ..base
        },
        LimiterControls {
            release: Seconds::ZERO,
            ..base
        },
    ];
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        all.push(LimiterControls {
            threshold: Db::new(bad),
            ..base
        });
        all.push(LimiterControls {
            ceiling: Db::new(bad),
            ..base
        });
        all.push(LimiterControls {
            release: Seconds::new(bad),
            ..base
        });
    }
    all
}

/// A stereo bank at `rate` set to `c`.
fn runner_at(c: &LimiterControls, rate: SampleRate) -> LimiterBank<f32> {
    let mut bank = LimiterBank::<f32>::new(ChannelLayout::STEREO, LOOKAHEAD, rate);
    bank.retarget(c);
    bank
}

/// A stereo bank at 48 kHz set to `c`.
fn runner(c: &LimiterControls) -> LimiterBank<f32> {
    runner_at(c, RATE)
}

/// `audio` (one slice per channel) through `bank` in blocks of `block`, with the threshold
/// and ceiling control signals when given.
fn run_in(
    bank: &mut LimiterBank<f32>,
    audio: &[&[f32]],
    signals: [Option<&[f32]>; 2],
    block: usize,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let signals = LimiterSignals {
            threshold: signals[0].map(|l| Mono::new(&l[at..end])),
            ceiling: signals[1].map(|l| Mono::new(&l[at..end])),
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            signals,
            &mut InputOutput::paired(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

fn run(bank: &mut LimiterBank<f32>, audio: &[&[f32]]) -> Vec<Vec<f32>> {
    run_in(bank, audio, [None, None], CHUNK)
}

/// The same signal on both channels, run with no control signal.
fn run_mono(bank: &mut LimiterBank<f32>, audio: &[f32]) -> Vec<f32> {
    run(bank, &[audio, audio]).remove(0)
}

/// A level of `db` as an amplitude, from the definition.
fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// How many ulps (`f32::EPSILON`) over its ceiling a limiter's sample peak may land: the
/// ceiling's amplitude, the curve's divide and the gain's product each round, so the bound
/// is a few ulps, not an exact one.
const CEILING_ULPS: f32 = 4.0;

/// The most a sample may reach under a ceiling of `db`: the ceiling's amplitude plus
/// [`CEILING_ULPS`] ulps, since the curve's roundings (the ceiling's amplitude, the divide, the
/// product) can put a sample a few ulps over it.
fn bound_of(db: f64) -> f64 {
    amplitude(db) * (1.0 + f64::from(CEILING_ULPS * f32::EPSILON))
}

/// The design holds the figures its definition gives: the lookahead is `ceil(lookahead *
/// rate)` frames and is the latency, the release is floored at 1 ms and a non-finite control
/// is the default's (a ceiling only for a NaN).
#[test]
fn the_bank_holds_the_figures_of_its_definition() {
    // 5 ms is 220.5 frames at 44.1 kHz, rounded up; exact at the others.
    for (rate, frames) in [(44_100.0, 221), (48_000.0, 240), (96_000.0, 480)] {
        let bank = runner_at(&LimiterControls::DEFAULT, SampleRate::of(rate));
        assert_eq!(bank.latency().get(), frames, "at {rate}");
    }
    // A lookahead under one frame is one frame: the window rounds up.
    let bank =
        LimiterBank::<f32>::new(ChannelLayout::MONO, Lookahead::of(Seconds::new(1e-9)), RATE);
    assert_eq!(bank.latency().get(), 1);

    let m = Design::<f32>::of(&controls(-12.0, -3.0, 0.0002), RATE);
    assert_eq!(
        m.release,
        Pole::Closes(Seconds::new(0.001).one_pole_coeff(RATE) as f32),
        "a release under 1 ms reads as 1 ms"
    );
    assert_eq!((m.threshold, m.ceiling), (-12.0, -3.0));

    let d = LimiterControls::DEFAULT;
    let m = Design::<f32>::of(
        &LimiterControls {
            threshold: Db::new(f64::NAN),
            ceiling: Db::new(f64::NAN),
            release: Seconds::new(f64::INFINITY),
        },
        RATE,
    );
    assert_eq!(
        (m.threshold, m.ceiling, m.release),
        (
            d.threshold.get() as f32,
            d.ceiling.get() as f32,
            Pole::Closes(d.release.one_pole_coeff(RATE) as f32)
        )
    );
    for inf in [f64::INFINITY, f64::NEG_INFINITY] {
        let m = Design::<f32>::of(
            &LimiterControls {
                ceiling: Db::new(inf),
                ..d
            },
            RATE,
        );
        // An infinity crosses into the float as its largest finite value, still beyond any level.
        let end = if inf > 0.0 { f32::MAX } else { -f32::MAX };
        assert_eq!(m.ceiling, end, "`+-inf` are ceilings");
        let m = Design::<f32>::of(
            &LimiterControls {
                threshold: Db::new(inf),
                ..d
            },
            RATE,
        );
        assert_eq!(m.threshold, end, "`+-inf` are thresholds");
    }
}

/// An independent figure for every output sample, from the definition and not from the
/// code's own helpers: `f64` throughout, the curve in its piecewise form, the envelope
/// by its two rules and the window minimum by brute force over `L + 1` frames. `x` is
/// the input of every channel. The envelope starts with no history, so the first frame
/// takes its target at once.
fn oracle(x: &[Vec<f32>], c: &LimiterControls, rate: f64) -> Vec<Vec<f64>> {
    let n = x[0].len();
    let (drive, threshold) = (
        amplitude(c.ceiling.get() - c.threshold.get()),
        amplitude(c.threshold.get()),
    );
    let coeff = 1.0 - (-1.0 / (c.release.get() * rate)).exp();
    let mut envelope = f64::INFINITY;
    let mut gain = Vec::with_capacity(n);
    for i in 0..n {
        let peak = x
            .iter()
            .map(|ch| f64::from(ch[i]).abs())
            .fold(0.0, f64::max);
        let target = if peak > threshold {
            drive * threshold / peak
        } else {
            drive
        };
        envelope = if target < envelope {
            target
        } else {
            envelope + coeff * (target - envelope)
        };
        gain.push(envelope);
    }
    // The delayed sample `i` leaves at `i + L`, scaled by the least gain of the frames
    // `i..=i + L`.
    let mut out = vec![vec![0.0f64; n]; x.len()];
    for i in 0..n.saturating_sub(L) {
        let least = gain[i..=i + L]
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        for (o, ch) in out.iter_mut().zip(x) {
            o[i + L] = f64::from(ch[i]) * least;
        }
    }
    out
}

/// Design diagram: every output sample is the input delayed by the lookahead and scaled
/// by the least envelope gain over the window of the sample and the lookahead after it,
/// where the envelope takes the curve's target at once when it is lower and releases by a
/// one-pole otherwise, and the curve is `min(drive, ceiling / peak)`. The figure is an
/// independent `f64` model of the definition, with a brute-force window minimum, over
/// loud and quiet passages and several threshold, ceiling and release settings.
#[test]
fn the_output_is_the_delayed_input_scaled_by_the_window_minimum_of_the_envelope() {
    let loud = |seed: u64, level: f32| -> Vec<f32> {
        let mut x = noise(seed, 12_000, level);
        // Quiet passages between the loud ones.
        for v in &mut x[3000..5000] {
            *v *= 0.02;
        }
        for v in &mut x[8000..9000] {
            *v *= 0.1;
        }
        x
    };
    let x = vec![loud(1, 1.5), loud(2, 0.9)];
    for c in [
        controls(-6.0, -0.3, 0.1),
        controls(-12.0, -12.0, 0.02),
        controls(-20.0, -3.0, 0.005),
        controls(-3.0, -9.0, 0.3),
    ] {
        let mut k = runner(&c);
        let got = run(&mut k, &[&x[0], &x[1]]);
        let want = oracle(&x, &c, RATE.get());
        let mut worst = 0.0f64;
        for (g, w) in got.iter().zip(&want) {
            for (i, (&g, &w)) in g.iter().zip(w).enumerate() {
                let err = (f64::from(g) - w).abs() / w.abs().max(1e-3);
                worst = worst.max(err);
                assert!(err < 2e-4, "{c:?} frame {i}: {g} vs {w}");
            }
        }
        assert!(
            worst > 0.0 || c.release.get() > 0.0,
            "the figure was compared"
        );
        // The figure was not vacuous: the curve engaged, some sample leaving well under
        // its input moved by the drive (the gain of a signal under the threshold).
        let drive = amplitude(c.ceiling.get() - c.threshold.get());
        let reduced = want[0]
            .iter()
            .skip(L)
            .zip(x[0].iter())
            .any(|(&w, &x)| w.abs() < 0.5 * drive * f64::from(x).abs());
        assert!(reduced, "{c:?} reduced nothing");
    }
}

/// The output stays within the ceiling's bound ([`bound_of`]), at every block length, for loud
/// input, whatever the ceiling's order against the threshold.
#[test]
fn the_output_stays_within_the_ceiling_bound() {
    let x = [noise(5, 20_000, 2.0), noise(6, 20_000, 1.4)];
    for (threshold, ceiling, release) in
        [(-12.0, -6.0, 0.05), (-20.0, -3.0, 0.2), (-3.0, -9.0, 0.01)]
    {
        let bound = bound_of(ceiling);
        for block in [1, 7, 64, 256] {
            let mut k = runner(&controls(threshold, ceiling, release));
            let out = run_in(&mut k, &[&x[0], &x[1]], [None, None], block);
            let peak = out
                .iter()
                .flatten()
                .map(|&s| f64::from(s).abs())
                .fold(0.0, f64::max);
            assert!(
                peak <= bound,
                "block {block}, ceiling {ceiling} dB: peak {peak} over {bound}"
            );
            // The limiter was at work: it reached for the ceiling, and the input was over it.
            assert!(
                peak > 0.8 * amplitude(ceiling),
                "block {block}: peak {peak}"
            );
        }
        assert!(f64::from(2.0f32) > amplitude(ceiling));
    }
}

/// With a threshold equal to the ceiling the drive is 1: a signal under it passes
/// untouched apart from the delay, from the first sample that leaves the delay (a reset
/// envelope has no history to rise from), and a louder one is only ever reduced. And in
/// general a signal under the threshold is moved by the drive, the ceiling over the
/// threshold in dB.
#[test]
fn a_signal_under_the_threshold_is_moved_by_the_drive_and_delayed() {
    let n = 48_000;
    for (threshold, ceiling, level) in [
        (-6.0f64, -6.0f64, 0.1f32),
        (-20.0, -6.0, 0.03),
        (-6.0, -12.0, 0.2),
    ] {
        let x = noise(7, n, level);
        assert!(
            f64::from(level) < amplitude(threshold),
            "the signal is under the threshold"
        );
        let mut k = runner(&controls(threshold, ceiling, 0.01));
        let out = run_mono(&mut k, &x);
        let drive = amplitude(ceiling - threshold);
        for i in L..n {
            let want = f64::from(x[i - L]) * drive;
            assert!(
                (f64::from(out[i]) - want).abs() <= 1e-4 * want.abs().max(1e-3),
                "threshold {threshold}, ceiling {ceiling}, frame {i}: {} vs {want}",
                out[i]
            );
        }
    }
}

/// **The first audio after a reset is limited from its first sample**: a full-scale step from
/// frame 0 leaves the delay at exactly the ceiling, not faded in from 0 over a release. And a
/// reset puts it back: the same step after a reset leaves at the ceiling again.
#[test]
fn the_first_audio_after_a_reset_is_limited_from_its_first_sample() {
    let ceiling = Db::new(-6.0).to_amplitude().get() as f32;
    let x = vec![1.0f32; 4 * L];
    let mut k = runner(&controls(-6.0, -6.0, 0.1));
    for pass in 0..2 {
        let out = run_mono(&mut k, &x);
        assert!(out[..L].iter().all(|&s| s == 0.0), "pass {pass}: the delay");
        for (i, &y) in out.iter().enumerate().skip(L) {
            assert!(
                (y - ceiling).abs() <= 2.0 * f32::EPSILON,
                "pass {pass}, frame {i}: {y}, the ceiling is {ceiling}"
            );
        }
        k.reset();
    }
}

/// Before the lookahead has passed nothing has come out, and after it every output is the
/// input a lookahead ago scaled by a gain between 0 and 1 (a ceiling at the threshold is a
/// drive of 1, so the limiter only ever reduces).
#[test]
fn the_output_is_the_input_delayed_by_the_lookahead_times_a_gain_at_most_one() {
    let x = [noise(8, 10_000, 1.5), noise(9, 10_000, 1.0)];
    let mut k = runner(&controls(-6.0, -6.0, 0.05));
    let out = run(&mut k, &[&x[0], &x[1]]);
    for (o, x) in out.iter().zip(&x) {
        assert!(
            o[..L].iter().all(|&s| s == 0.0),
            "silent before the lookahead"
        );
        let mut reduced = false;
        for i in L..o.len() {
            let delayed = x[i - L];
            assert!(o[i] * delayed >= 0.0, "frame {i}: the sign is the input's");
            assert!(
                o[i].abs() <= delayed.abs() * (1.0 + 2.0 * f32::EPSILON),
                "frame {i}: |{}| over |{delayed}|",
                o[i]
            );
            reduced |= o[i].abs() < 0.9 * delayed.abs();
        }
        assert!(reduced, "a loud signal is reduced somewhere");
    }
}

/// The gain is linked across channels, from the loudest: a loud peak on either channel
/// reduces the other too, by the same gain, though the other is under the threshold on its
/// own.
#[test]
fn the_gain_is_linked_across_channels() {
    let n = 12_000;
    let loud = 6_000;
    for loud_channel in 0..2 {
        let mut x = [vec![0.05f32; n], vec![0.05f32; n]];
        x[loud_channel][loud] = 1.0;
        // A release of 5 ms: the envelope (which starts at 0) has settled long before the peak.
        let mut k = runner(&controls(-6.0, -6.0, 0.005));
        let out = run(&mut k, &[&x[0], &x[1]]);
        let quiet = 1 - loud_channel;
        // The quiet channel's gain, where it has a signal, is the loud one's.
        let mut gained = false;
        for i in L..n {
            let (a, b) = (
                out[loud_channel][i] / x[loud_channel][i - L],
                out[quiet][i] / x[quiet][i - L],
            );
            assert!(
                (a - b).abs() <= 1e-6 * a.abs().max(1e-3),
                "loud channel {loud_channel}, frame {i}: {a} vs {b}"
            );
            // Around the peak, where the settled gain is 1 unless the peak reduced it.
            gained |= (loud + 1..loud + L).contains(&i) && b < 0.6;
        }
        assert!(
            gained,
            "the quiet channel was reduced by channel {loud_channel}'s peak"
        );
    }
    // Alone, a quiet channel is untouched (a settled drive of 1).
    let right = vec![0.05f32; n];
    let mut alone = runner(&controls(-6.0, -6.0, 0.05));
    let solo = run(&mut alone, &[&right, &right]);
    assert!(solo[0][n - 1] > 0.049, "{}", solo[0][n - 1]);
}

/// The attack is instant and leads the peak by the whole lookahead, and the release is a
/// one-pole of the coefficient `1 - exp(-1 / (release * rate))`: after a single peak of 1
/// over quiet audio the gain is the curve's `ceiling / peak` at once, for the peak's own
/// sample and the `L` samples before it (their windows hold the peak's frame), not for
/// the one before those; then it climbs back by `1 - (1 - g0) * (1 - c)^k`.
#[test]
fn the_attack_is_instant_and_the_release_is_one_pole() {
    let release = 0.05;
    let n = 60_000;
    let peak_at = 30_000;
    let mut x = vec![0.1f32; n];
    x[peak_at] = 1.0;
    // Threshold at the ceiling: the quiet signal is at gain 1 and the peak's target is
    // the ceiling over the peak, -6 dB.
    let mut k = runner(&controls(-6.0, -6.0, release));
    let out = run_mono(&mut k, &x);
    let g0 = amplitude(-6.0);

    // The peak's own sample is at the target at once.
    let got = f64::from(out[peak_at + L]);
    assert!((got / g0 - 1.0).abs() < 1e-5, "{got} vs {g0}");
    // So are the L samples before it, and not the one before those.
    for j in 1..=L {
        let g = f64::from(out[peak_at - j + L]) / 0.1;
        assert!(
            (g / g0 - 1.0).abs() < 1e-5,
            "{j} before the peak: {g} vs {g0}"
        );
    }
    let before = f64::from(out[peak_at - L - 1 + L]) / 0.1;
    assert!(before > 0.99, "the sample {} before: {before}", L + 1);

    // After it the gain rises by the one-pole's figure, which is checked as a ratio to
    // the gap left: `1 - g_k = (1 - g0) * exp(-k / (release * rate))`.
    let tau = release * RATE.get();
    for k in [1usize, 100, 1000, 2400, 5000] {
        let g = f64::from(out[peak_at + k + L]) / 0.1;
        let want = 1.0 - (1.0 - g0) * (-(k as f64) / tau).exp();
        assert!(
            (g - want).abs() < 2e-4,
            "{k} frames after the peak: {g} vs {want}"
        );
    }
}

/// A threshold and a ceiling control signal are read at every frame, not once per block: a ceiling
/// that alternates per frame bounds each output sample by its own frame's ceiling (the
/// sample a lookahead ago, which that frame's gain was made for), and a frame the control signal
/// leaves non-finite is bounded by the set ceiling and does not poison the gain.
#[test]
fn the_ceiling_signal_bounds_each_sample_by_its_own_frames_ceiling() {
    let n = 12_000;
    let x = noise(11, n, 1.5);
    // The set ceiling is 0 dB, which the control signal's -20 and -6 dB are under; a NaN frame
    // is the set one.
    let c = controls(-30.0, 0.0, 0.01);
    let signal: Vec<f32> = (0..n)
        .map(|i| match i % 8 {
            0..=2 => -20.0,
            3 => f32::NAN,
            _ => -6.0,
        })
        .collect();
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&x, &x], [None, Some(&signal)], 100);
    let mut reached = false;
    for i in 0..n - L {
        let bound = if signal[i].is_nan() {
            amplitude(0.0)
        } else {
            amplitude(f64::from(signal[i]))
        } * (1.0 + f64::from(CEILING_ULPS * f32::EPSILON));
        let got = f64::from(out[0][i + L]);
        assert!(got.is_finite(), "frame {i}: {got}");
        assert!(
            got.abs() <= bound,
            "frame {i} ({} dB): {got} over {bound}",
            signal[i]
        );
        reached |= signal[i] == -20.0 && got.abs() > 0.5 * amplitude(-20.0);
    }
    assert!(
        reached,
        "the -20 dB frames were limited to their own ceiling"
    );
}

/// The threshold control signal is the threshold of its frame: a quiet signal is moved by the
/// drive, so a control signal at -30 dB and a base at -10 dB give a drive of 24 dB against 4 dB,
/// and a control signal that changes inside a block (not at a block's edge) changes the gain there.
/// A frame the control signal leaves non-finite is the set threshold.
#[test]
fn the_threshold_signal_is_read_per_frame_and_a_non_finite_frame_is_the_members() {
    let n = 12_000;
    let x = vec![0.01f32; n];
    // A 1 ms release settles within a few hundred frames: 40 time constants in 2000.
    let c = controls(-10.0, -6.0, 0.001);
    let drive = |threshold: f64| amplitude(-6.0 - threshold);
    // -30 dB until frame 6000, then the member's -10 dB (as a NaN frame), in blocks of
    // 256: 6000 is not a multiple.
    let signal: Vec<f32> = (0..n)
        .map(|i| if i < 6000 { -30.0 } else { f32::NAN })
        .collect();
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&x, &x], [Some(&signal), None], CHUNK);
    for (i, want) in [(L + 5000, drive(-30.0)), (n - 1, drive(-10.0))] {
        let got = f64::from(out[0][i]) / 0.01;
        assert!(
            (got / want - 1.0).abs() < 1e-3,
            "frame {i}: gain {got} vs {want}"
        );
    }
    // Both the control signals at once, over the loud noise: finite and under the control signals' ceiling.
    let loud = noise(12, n, 2.0);
    let ceiling = vec![-12.0f32; n];
    let thr = vec![-20.0f32; n];
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&loud, &loud], [Some(&thr), Some(&ceiling)], 64);
    let bound = bound_of(-12.0);
    assert!(out[0].iter().all(|&s| f64::from(s).abs() <= bound));
}

/// A moved ceiling lands at once: after a retarget to a lower ceiling, no sample fed from
/// then on comes out over the new one (a ramped ceiling would let samples past it while it
/// moved), though the samples fed before it, already in the delay, left at the old.
#[test]
fn a_moved_ceiling_lands_at_once() {
    let n = 24_000;
    let at = 12_000;
    let x = noise(13, n, 2.0);
    let (old, new) = (controls(-12.0, -3.0, 0.1), controls(-12.0, -18.0, 0.1));
    let mut k = runner(&old);
    let mut out = run(&mut k, &[&x[..at], &x[..at]]).remove(0);
    k.retarget(&new);
    out.extend(run(&mut k, &[&x[at..], &x[at..]]).remove(0));

    let bound = bound_of(-18.0);
    // What was fed from the retarget on leaves a lookahead later, every frame under it.
    for (i, &s) in out.iter().enumerate().skip(at + L) {
        assert!(f64::from(s).abs() <= bound, "frame {i}: {s} over {bound}");
    }
    // The old ceiling's output was over the new one: the bound is not trivially met.
    let before = out[..at]
        .iter()
        .map(|&s| f64::from(s).abs())
        .fold(0.0, f64::max);
    assert!(before > 2.0 * bound, "{before}");
}

/// A reset clears the envelope and the delay: nothing of the audio before it is heard after
/// (the delay does not hold it).
#[test]
fn a_reset_empties_the_delay() {
    let mut k = runner(&controls(-6.0, -6.0, 0.05));
    run_mono(&mut k, &noise(14, 5_000, 1.0));
    k.reset();
    let out = run_mono(&mut k, &vec![0.0; 2 * L]);
    assert!(
        out.iter().all(|&s| s == 0.0),
        "the old audio came out of the delay"
    );
}

/// Every hostile control runs: the render is finite (a `+inf` ceiling is a boost of the
/// largest finite gain, and `-inf` mutes).
#[test]
fn hostile_controls_render_finite() {
    let x = noise(15, 4_000, 1.0);
    for c in hostile() {
        let mut k = runner(&c);
        let out = run_mono(&mut k, &x);
        assert!(out.iter().all(|s| s.is_finite()), "{c:?}");
    }
}

/// `Lookahead` keeps the range-checked shape: `new` refuses what is not a
/// lookahead (not finite, not above zero, past the cap) and never clamps, `of` agrees,
/// and `TryFrom` carries the refused raw. A silently clamped lookahead would declare a
/// latency the limiter does not have.
#[test]
fn a_lookahead_keeps_the_range_checked_shape() {
    let c = tutti_fixtures::RangeContract {
        new: Lookahead::new,
        get: Lookahead::get,
        of: Some(Lookahead::of),
        valid: &[
            Seconds::new(1e-9),
            Seconds::new(0.005),
            Seconds::new(0.1),
            Lookahead::CAP,
        ],
        invalid: &[
            Seconds::ZERO,
            Seconds::new(-0.005),
            Seconds::new(1.000_001),
            Seconds::new(f64::NAN),
            Seconds::new(f64::INFINITY),
            Seconds::new(f64::NEG_INFINITY),
        ],
    };
    tutti_fixtures::check_range_contract(&c);
    assert_eq!(
        Lookahead::try_from(Seconds::new(-1.0)),
        Err(LookaheadError(Seconds::new(-1.0)))
    );
}

/// The ceiling bounds the **sample** peak of the output: a full-scale square, an impulse train
/// and a burst of 1e30 all leave under it at every ceiling from -20 to 0 dB (a gain divide
/// and a product can round one ulp past, so the bound has two ulps' room). It is not a
/// true-peak limit: the bound is on the samples, which is what the brickwall's clamp bounds
/// too.
#[test]
fn a_square_an_impulse_train_and_a_burst_stay_under_the_ceiling() {
    let n = 12_000;
    let square: Vec<f32> = (0..n)
        .map(|i| if (i / 50) % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let train: Vec<f32> = (0..n)
        .map(|i| if i % 997 == 0 { 8.0 } else { 0.0 })
        .collect();
    let burst: Vec<f32> = (0..n)
        .map(|i| {
            if (4000..4100).contains(&i) {
                1e30 * if i % 2 == 0 { 1.0 } else { -1.0 }
            } else {
                0.01
            }
        })
        .collect();
    for ceiling in [-20.0f64, -6.0, -0.3, 0.0] {
        let bound = bound_of(ceiling);
        for (name, x) in [("square", &square), ("train", &train), ("burst", &burst)] {
            for block in [1, 33, 256] {
                let mut k = runner(&controls(-12.0, ceiling, 0.05));
                let out = run_in(&mut k, &[x, x], [None, None], block);
                let peak = out
                    .iter()
                    .flatten()
                    .map(|&s| f64::from(s).abs())
                    .fold(0.0, f64::max);
                assert!(
                    peak <= bound,
                    "{name} at {ceiling} dB, block {block}: peak {peak} over {bound}"
                );
            }
        }
    }
}

/// The reduction begins `lookahead` frames before the peak reaches the output: the gain on
/// the sample `j` frames before a peak is already the peak's, for every `j` up to the
/// lookahead, and the sample before that is untouched. (The same figure, measured on the
/// output of a signal at 0.1 with one peak of 1.)
#[test]
fn the_reduction_begins_a_lookahead_before_the_peak_leaves() {
    let mut x = vec![0.1f32; 20_000];
    let peak_at = 10_000;
    x[peak_at] = 1.0;
    let mut k = runner(&controls(-6.0, -6.0, 0.05));
    let out = run_mono(&mut k, &x);
    let g0 = amplitude(-6.0);
    let gain = |i: usize| f64::from(out[i + L]) / f64::from(x[i]);
    assert!((gain(peak_at) / g0 - 1.0).abs() < 1e-5);
    assert!((gain(peak_at - L) / g0 - 1.0).abs() < 1e-5, "L before");
    assert!(gain(peak_at - L - 1) > 0.99, "L + 1 before");
}

/// The render does not depend on how the stream is cut: calls of 1, 7, 33, 64, 100 and 255
/// frames give the bits of one call, with both control signals and a moving release.
#[test]
fn a_split_stream_renders_the_bits_of_the_whole() {
    let n = 4000;
    let audio = [noise(1, n, 1.4), noise(2, n, 0.9)];
    let thr: Vec<f32> = (0..n).map(|i| -20.0 + (i % 500) as f32 * 0.03).collect();
    let ceil: Vec<f32> = (0..n).map(|i| -3.0 - (i % 300) as f32 * 0.02).collect();
    let render = |block: usize| {
        let mut k = runner(&controls(-12.0, -6.0, 0.05));
        run_in(
            &mut k,
            &[&audio[0], &audio[1]],
            [Some(&thr), Some(&ceil)],
            block,
        )
    };
    let whole = render(n);
    for block in [1, 7, 33, 64, 100, 255] {
        assert_eq!(render(block), whole, "blocks of {block}");
    }
}

/// A reset bank renders as a fresh one, bit for bit, though the controls moved meanwhile
/// (the design is kept, the envelope and the rings are not).
#[test]
fn a_reset_bank_renders_as_a_fresh_one() {
    let c = controls(-12.0, -6.0, 0.05);
    let x = noise(3, 3000, 1.5);
    let mut worn = runner(&c);
    let _ = run_mono(&mut worn, &x);
    worn.reset();
    let mut fresh = runner(&c);
    assert_eq!(run_mono(&mut worn, &x), run_mono(&mut fresh, &x));
}

/// A clone continues exactly where the original is, and shares nothing with it.
#[test]
fn a_clone_continues_identically_and_shares_nothing() {
    let x = noise(4, 3000, 1.5);
    let mut a = runner(&controls(-12.0, -6.0, 0.05));
    let _ = run_mono(&mut a, &x);
    let mut b = a.clone();
    let from_a = run_mono(&mut a, &x);
    assert_eq!(run_mono(&mut b, &x), from_a);
}

/// Over a grid and seeded draws of every control, the output stays within the ceiling's bound
/// ([`bound_of`]), whatever the threshold's order against it and the release.
#[test]
fn every_reachable_limiter_stays_within_its_ceiling_bound() {
    let x = [noise(31, 3_000, 2.0), noise(32, 3_000, 1.2)];
    for c in reachable() {
        let bound = bound_of(c.ceiling.get());
        let mut k = runner(&c);
        let out = run(&mut k, &[&x[0], &x[1]]);
        let peak = out
            .iter()
            .flatten()
            .map(|&s| f64::from(s).abs())
            .fold(0.0, f64::max);
        assert!(peak <= bound, "{c:?}: peak {peak} over {bound}");
    }
}

/// A finite control signal value of any size is a level and not an overflow: a ceiling control signal of +-1e30 dB
/// and a threshold control signal of -1e3, -1e6 and -1e10 dB leave the render finite (an envelope that
/// took an infinite target would keep it, `toward(inf, inf)` being NaN), and a threshold far
/// under the ceiling still puts the output on the ceiling: the gain is the ceiling over the
/// peak, not the difference of two enormous levels.
#[test]
fn a_hostile_signal_is_clamped_and_does_not_latch_or_lose_the_ceiling() {
    let n = 4_000;
    let x = [noise(41, n, 1.5), noise(42, n, 1.0)];
    for ceiling in [1.0e30f32, -1.0e30] {
        let signal = vec![ceiling; n];
        let mut k = runner(&controls(-12.0, -6.0, 0.05));
        let out = run_in(&mut k, &[&x[0], &x[1]], [None, Some(&signal)], 64);
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "ceiling signal {ceiling}"
        );
        // The bank still works after it: a finite control signal is limited to its own ceiling.
        let sane = vec![-6.0f32; n];
        let after = run_in(&mut k, &[&x[0], &x[1]], [None, Some(&sane)], 64);
        // The first `L` frames are the audio the hostile control signal's gain was made for.
        let peak = after
            .iter()
            .flat_map(|c| &c[L..])
            .map(|&s| f64::from(s).abs())
            .fold(0.0, f64::max);
        assert!(
            peak <= bound_of(-6.0) && peak > 0.9 * amplitude(-6.0),
            "{ceiling}: {peak}"
        );
    }
    for threshold in [-1.0e3f32, -1.0e6, -1.0e10] {
        let signal = vec![threshold; n];
        let mut k = runner(&controls(-12.0, -6.0, 0.05));
        let out = run_in(&mut k, &[&x[0], &x[1]], [Some(&signal), None], 64);
        let peak = out
            .iter()
            .flatten()
            .map(|&s| f64::from(s).abs())
            .fold(0.0, f64::max);
        assert!(
            peak <= bound_of(-6.0) && peak > 0.95 * amplitude(-6.0),
            "threshold signal {threshold}: peak {peak}, the ceiling {}",
            amplitude(-6.0)
        );
    }
}

/// A cell threshold of `-inf` or far under -6165 dB leaves the output under the ceiling: the
/// boost under the threshold is held to the ceiling over the (clamped) threshold amplitude,
/// not the gain a clamped gap would give, and a ceiling of `-inf` mutes. Silence, tiny
/// samples and a loud signal, at ceilings 0, 12 and 20 dB and `-inf`.
#[test]
fn a_threshold_past_the_clamp_does_not_overshoot_the_ceiling() {
    let n = 3_000;
    let mut x = noise(43, n, 1.0);
    x[100] = 1.0e-38;
    x[101] = 1.1e-38;
    x[102] = 1.2e-38;
    // The thresholds straddle the `f64` amplitude limit (`10^(-6165/20)` is near `f64::MIN_POSITIVE`),
    // a `Db` being `f64`; the `f32` clamp (`MAX_GAP`) sits well inside them.
    for threshold in [f64::NEG_INFINITY, -7000.0, -6165.0, -2000.0] {
        for ceiling in [0.0f64, f64::NEG_INFINITY, 12.0, 20.0] {
            let mut k = runner(&controls(threshold, ceiling, 0.05));
            for scale in [1.0f32, 1.0e-30, 1.0e-38] {
                let scaled: Vec<f32> = x.iter().map(|v| v * scale).collect();
                let out = run_mono(&mut k, &scaled);
                let peak = out.iter().map(|&s| f64::from(s).abs()).fold(0.0, f64::max);
                let bound = if ceiling.is_finite() {
                    bound_of(ceiling)
                } else {
                    0.0
                };
                assert!(
                    peak <= bound,
                    "threshold {threshold}, ceiling {ceiling}, scale {scale}: peak {peak} over {bound}"
                );
            }
        }
    }
}

/// An infinite input sample reads as the largest finite level: every output frame is finite
/// (the sample's own frame leaves at about the ceiling, `inf` held to `f32::MAX` before the
/// gain), the following finite audio is limited as ever (a stuck or NaN envelope would
/// not be), and a ceiling of `-inf` mutes it.
#[test]
fn an_infinite_sample_is_finite_on_its_own_frame_and_does_not_latch_the_envelope() {
    let n = 6_000;
    let mut x = noise(44, n, 0.5);
    x[1000] = f32::INFINITY;
    x[1001] = f32::NEG_INFINITY;
    let mut k = runner(&controls(-12.0, -6.0, 0.01));
    let out = run_mono(&mut k, &x);
    assert!(out.iter().all(|s| s.is_finite()), "every frame is finite");
    // The infinities leave on frames 1000 + L and 1001 + L, at the ceiling.
    assert!(f64::from(out[1000 + L].abs()) <= bound_of(-6.0));
    assert!(f64::from(out[1001 + L].abs()) <= bound_of(-6.0));
    // The audio after is limited to the ceiling's bound and loud enough to be limited (the
    // input is 0.5 under a -6 dB ceiling at threshold -12 dB: it settles on 0.5 * drive).
    let tail = &out[2000 + L..];
    let peak = tail.iter().map(|&s| f64::from(s).abs()).fold(0.0, f64::max);
    assert!(
        peak <= bound_of(-6.0) && peak > 0.2,
        "the tail: peak {peak}"
    );
    let mut k = runner(&controls(-12.0, f64::NEG_INFINITY, 0.01));
    let out = run_mono(&mut k, &x);
    assert!(out.iter().all(|&s| s == 0.0), "a ceiling of -inf mutes it");
}

/// The set ceiling and threshold are held like a control signal's: a ceiling cell of +inf or 1e30 dB
/// with a threshold control signal and no ceiling control signal (the fallback path) leaves the render finite
/// and the next sane ceiling limited to itself; and the quiet signal's gain is not `inf * 0`.
#[test]
fn a_hostile_set_ceiling_read_with_a_signal_is_clamped_too() {
    let n = 4_000;
    let x = [noise(45, n, 1.5), noise(46, n, 1.0)];
    let thr = vec![-20.0f32; n];
    for ceiling in [f64::INFINITY, 1.0e30] {
        let mut k = runner(&controls(-12.0, ceiling, 0.05));
        let mut silent_then_loud = vec![0.0f32; 500];
        silent_then_loud.extend_from_slice(&x[0][..n - 500]);
        let out = run_in(&mut k, &[&silent_then_loud, &x[1]], [Some(&thr), None], 64);
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "ceiling {ceiling}"
        );
        k.retarget(&controls(-12.0, -6.0, 0.05));
        let after = run_in(&mut k, &[&x[0], &x[1]], [Some(&thr), None], 64);
        let peak = after
            .iter()
            .flat_map(|c| &c[L..])
            .map(|&s| f64::from(s).abs())
            .fold(0.0, f64::max);
        assert!(
            peak <= bound_of(-6.0) && peak > 0.9 * amplitude(-6.0),
            "{ceiling}: {peak}"
        );
    }
}

/// A limiter does not park: after `L` frames of silence the output is exactly zero, and a gap
/// of digital silence at least a lookahead long leaves the release running (it is not reset
/// to full drive): quiet audio after a 6 ms gap is still being scaled by the envelope the
/// reduction left, not by the drive.
#[test]
fn the_output_is_silent_a_lookahead_after_the_input_and_the_release_runs_on() {
    let mut x = vec![1.0f32; 2_000];
    x.extend(vec![0.0f32; 300]);
    x.extend(vec![0.05f32; 4_000]);
    let mut k = runner(&controls(-6.0, -6.0, 0.5));
    let out = run_mono(&mut k, &x);
    // The gap, 300 frames, leaves its own `L` frames silent from where the loud audio ends
    // leaving the delay: frames 2000 + L .. 2300 + L hold the gap's silence.
    assert!(out[2000 + L..2300 + L].iter().all(|&s| s == 0.0));
    // The quiet audio leaves at 2300 + L at the release's gain (about -6 dB a half second
    // from full drive), not at the drive's unity.
    let first = f64::from(out[2300 + L + 10]) / 0.05;
    assert!(
        first < 0.6,
        "the release ran on across the gap: gain {first}"
    );
}

/// The set value a frame falls back to keeps its documented meaning with a control signal driven: an
/// set ceiling of `-inf` mutes exactly (a gain of 0, not the -300 dB a control signal value is held
/// to), an set threshold of `+inf` is a gain of 0 whatever the ceiling control signal says, and one of
/// `-inf` limits every signal to the ceiling.
#[test]
fn the_set_extremes_keep_their_meaning_on_the_signal_path() {
    let n = 3_000;
    let x = [noise(47, n, 1.5), noise(48, n, 1.0)];
    let thr = vec![-20.0f32; n];
    let ceil = vec![-6.0f32; n];

    let mut k = runner(&controls(-12.0, f64::NEG_INFINITY, 0.05));
    let out = run_in(&mut k, &[&x[0], &x[1]], [Some(&thr), None], 64);
    assert!(
        out.iter().flatten().all(|&s| s == 0.0),
        "a -inf ceiling mutes exactly"
    );

    let mut k = runner(&controls(f64::INFINITY, -6.0, 0.05));
    let out = run_in(&mut k, &[&x[0], &x[1]], [None, Some(&ceil)], 64);
    assert!(
        out.iter().flatten().all(|&s| s == 0.0),
        "a +inf threshold is a gain of 0"
    );

    let mut k = runner(&controls(f64::NEG_INFINITY, -6.0, 0.05));
    let out = run_in(&mut k, &[&x[0], &x[1]], [None, Some(&ceil)], 64);
    let peak = out
        .iter()
        .flat_map(|c| &c[L..])
        .map(|&s| f64::from(s).abs())
        .fold(0.0, f64::max);
    assert!(
        peak <= bound_of(-6.0) && peak > 0.9 * amplitude(-6.0),
        "a -inf threshold limits everything: peak {peak}"
    );
}

mod true_peak;

/// `x` through a mono `f64` bank at 48 kHz set to `c`, in one block.
fn run_f64(c: &LimiterControls, x: &[f64]) -> Vec<f64> {
    let mut bank = LimiterBank::<f64>::new(ChannelLayout::MONO, LOOKAHEAD, RATE);
    bank.retarget(c);
    let mut out = vec![0.0f64; x.len()];
    bank.process(
        LimiterSignals::default(),
        &mut InputOutput::paired(
            Planes::new(&[x]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    out
}

/// Under a threshold at the ceiling the gain is `10^0 = 1`: an `f64` signal of more than 24
/// bits comes out exactly `L` frames late (the lookahead is the delay) and silent before.
#[test]
fn an_f64_limiter_under_its_threshold_delays_by_exactly_the_lookahead() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let len = 3 * L;
    let x = wide_signal(len);
    let want: Vec<f64> = (0..len)
        .map(|n| n.checked_sub(L).map_or(0.0, |k| x[k]))
        .collect();
    assert_close(&run_f64(&controls(0.0, 0.0, 0.1), &x), &want, 0.0);
}

/// A constant signal over the threshold comes out at the ceiling the control holds, to 1e-15:
/// the oracle is the closed form `10^(-20 / 20)`; the gain is divided in `f64`.
#[test]
fn an_f64_limiter_holds_a_constant_signal_at_the_f64_ceiling() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let x = vec![wide_signal(1)[0]; 3 * L];
    let y = run_f64(&controls(-30.0, -20.0, 0.1), &x);
    // The ceiling is the `f64` converter's value: the control is no longer rounded to `f32`.
    assert_close(
        &y[L..],
        &vec![Db::new(-20.0).to_amplitude().get(); 2 * L],
        1e-15,
    );
}

/// After a loud stretch the gain rises toward the quiet gain `10^((ceiling - threshold) / 20)`
/// along the one-pole `e[k] = e[k-1] + c (target - e[k-1])`, `c = 1 - exp(-1 / (release
/// rate))`, from the limited gain `ceiling / peak`; the output is the delayed quiet signal
/// times that envelope (a rising envelope's window minimum is its oldest value): the closed
/// form in `f64`, to 1e-12, from the ceiling and quiet gains' closed forms.
#[test]
fn an_f64_limiter_releases_along_the_one_pole_closed_form_in_f64() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (threshold, ceiling, release) = (-6.0f64, -3.0f64, 0.01);
    let (len, switch) = (2_000, 600);
    let quiet = wide_signal(1)[0];
    let loud = 4.0 * quiet;
    let x: Vec<f64> = (0..len)
        .map(|n| if n < switch { loud } else { quiet })
        .collect();
    let coeff = 1.0 - (-1.0 / (release * RATE.get())).exp();
    let below = Db::new(ceiling - threshold).to_amplitude().get();
    let mut envelope = Db::new(ceiling).to_amplitude().get() / loud;
    let mut gains = vec![envelope; switch];
    for _ in switch..len {
        envelope += coeff * (below - envelope);
        gains.push(envelope);
    }
    let want: Vec<f64> = (switch + L..len).map(|n| quiet * gains[n - L]).collect();
    let y = run_f64(&controls(threshold, ceiling, release), &x);
    // The closed form is `f64` throughout, so the tolerance is rounding, not `f32`'s.
    assert_close(&y[switch + L..], &want, 1e-12);
}

/// A limiter released over 300 s at 192 kHz (a coefficient of `1.7e-8`, under half an ulp of an
/// envelope near 1) still recovers in f32: one loud frame sets the envelope to its instant
/// gain `g0 = 10^(-0.3 / 20)` (peak 1 over the -6 dB threshold), and the quiet frames after it
/// let it rise toward the drive `gt = 10^(5.7 / 20)` as `gt - (gt - g0) a^m`, `a = 1 - c` with
/// `c` the f32 coefficient. The output a lookahead later is `0.01 * envelope`, the oldest gain of
/// a rising window. Over a quarter of a time constant it holds that to the sum of: `2^-24 (gt -
/// g0)` stationary in the envelope (the product's rounding over `c`), half an ulp carried in its
/// value (`2^-24` in `[1, 2)`), the figures `g0` and `gt` rounded into f32 (`1 + 2` times `2^-24`
/// at most) and the output's own rounding (`2 * 2^-24`), under `8 * 2^-24`. The plain recurrence
/// `e + c (gt - e)`, computed here, stays frozen at `g0`.
#[test]
fn an_f32_three_hundred_second_release_at_192_khz_still_recovers_along_the_closed_form() {
    const CHUNK_FRAMES: usize = 1 << 16;
    let (rate, tau) = (SampleRate::SR_192K, 300.0);
    let c = f64::from(Seconds::new(tau).one_pole_coeff(rate) as f32);
    let g0 = 10f64.powf(-0.3 / 20.0);
    let gt = 10f64.powf(5.7 / 20.0);
    let mut plain = g0 as f32;
    for _ in 0..1_000 {
        plain += c as f32 * (gt as f32 - plain);
    }
    assert_eq!(plain, g0 as f32, "the plain recurrence moved");
    let mut bank = runner_at(&controls(-6.0, -0.3, tau), rate);
    let lookahead = bank.latency().get();
    let quiet = 0.01f32;
    let n = (0.25 * tau * rate.get()) as usize;
    let probes = [n / 4, n / 2, n - 1];
    let (mut seen, mut worst) = (0, 0.0f64);
    while seen < lookahead + n {
        let m = CHUNK_FRAMES.min(lookahead + n - seen);
        let mut x = vec![quiet; m];
        if seen == 0 {
            x[0] = 1.0;
        }
        let out = run_mono(&mut bank, &x);
        for &p in &probes {
            let k = lookahead + p;
            if (seen..seen + m).contains(&k) {
                let want = gt - (gt - g0) * (-c).ln_1p().exp().powi(p as i32);
                let got = f64::from(out[k - seen]) / f64::from(quiet);
                worst = worst.max((got - want).abs());
                assert!(want - g0 > 0.01, "the envelope has risen by the probe");
            }
        }
        seen += m;
    }
    let bound = 8.0 * 2f64.powi(-24);
    assert!(
        worst <= bound,
        "{worst} off the closed form, bound {bound:e}"
    );
}
