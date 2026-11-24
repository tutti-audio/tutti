//! The true-peak detection: the alignment, the bound and the refusal, against a
//! windowed-sinc reconstruction of the output (a signal's property, not another render).

use super::*;
use crate::{Oversampling, OversamplingFactor};
use tutti_units::Amplitude;

/// The ceiling the cases limit to: 0.5, -6.02 dB.
const CEILING: f64 = 0.5;

fn ceiling_db() -> f64 {
    Db::from_amplitude(Amplitude::new(CEILING)).get()
}

/// A bank limiting to `CEILING` with no drive (threshold at the ceiling: the gain is
/// `min(1, ceiling / peak)`), at 48 kHz and a 5 ms lookahead.
fn bank(detection: PeakDetection) -> LimiterBank<f32> {
    bank_at(detection, LOOKAHEAD, RATE)
}

fn detecting(detection: PeakDetection, lookahead: Lookahead, rate: SampleRate) -> LimiterBank<f32> {
    match detection {
        PeakDetection::Sample => LimiterBank::new(ChannelLayout::MONO, lookahead, rate),
        PeakDetection::TruePeak => LimiterBank::true_peak(ChannelLayout::MONO, lookahead, rate)
            .expect("the up-sampler designs at the rate"),
    }
}

fn bank_at(detection: PeakDetection, lookahead: Lookahead, rate: SampleRate) -> LimiterBank<f32> {
    let mut bank = detecting(detection, lookahead, rate);
    bank.retarget(&controls(ceiling_db(), ceiling_db(), 0.1));
    bank
}

fn render(bank: &mut LimiterBank<f32>, x: &[f32]) -> Vec<f32> {
    run(bank, &[x]).remove(0)
}

/// The shortest lookahead a true-peak bank takes at the test rate: the floor `1 + U / 4` frames.
fn shortest() -> Lookahead {
    let plan = Oversampling::new(OversamplingFactor::X4)
        .plan(RATE)
        .unwrap();
    let needed = 1 + plan.upsample_delay() / 4;
    Lookahead::of(Seconds::new(needed as f64 / 48_000.0))
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..60 {
        term *= (x / (2.0 * f64::from(k))).powi(2);
        sum += term;
    }
    sum
}

/// The largest magnitude of `x` between its samples, read by a Kaiser-windowed sinc (beta 10,
/// 64 taps per phase) at 16 phases per frame, away from the 40 frames at each end.
///
/// Beta 10 puts the window's stopband near -100 dB; over 64 taps its transition is about 0.1
/// of the rate wide, centred on 0.5, so a tone at 0.44 of the rate (under the 0.45 edge) is
/// read to about 1e-5 relative: the 2e-4 slack the cases allow covers it.
fn true_peak(x: &[f32]) -> f64 {
    const HALF: i64 = 32;
    const PHASES: i64 = 16;
    const BETA: f64 = 10.0;
    let norm = bessel_i0(BETA);
    let kernel = |t: f64| {
        let sinc = if t == 0.0 {
            1.0
        } else {
            (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
        };
        let r = t / HALF as f64;
        if r.abs() >= 1.0 {
            0.0
        } else {
            sinc * bessel_i0(BETA * (1.0 - r * r).sqrt()) / norm
        }
    };
    let taps: Vec<Vec<f64>> = (0..PHASES)
        .map(|p| {
            (1 - HALF..=HALF)
                .map(|k| kernel(p as f64 / PHASES as f64 - k as f64))
                .collect()
        })
        .collect();
    let mut peak = 0.0f64;
    for n in 40..x.len() - 40 {
        for phase in &taps {
            let y: f64 = phase
                .iter()
                .enumerate()
                .map(|(i, &h)| h * f64::from(x[n + i + 1 - HALF as usize]))
                .sum();
            peak = peak.max(y.abs());
        }
    }
    peak
}

/// `frames` of a sine at `cycles` of the sample rate per frame, `phase` radians, amplitude `a`.
fn tone(cycles: f64, phase: f64, a: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| (a * (std::f64::consts::TAU * cycles * n as f64 + phase).sin()) as f32)
        .collect()
}

fn db(ratio: f64) -> f64 {
    Amplitude::new(ratio).to_db().get()
}

/// The bound a true-peak bank keeps: the ceiling and its margin, in amplitude.
fn bound() -> f64 {
    CEILING * LimiterBank::<f32>::TRUE_PEAK_MARGIN.to_amplitude().get()
}

/// The bound of a tone of `cycles` of the rate: its own band limit in place of the passband's,
/// `1 / cos(pi * cycles / 4)` (the factor of an eighth of a frame at that frequency), and 2e-4
/// of slack for the passband ripple and the reference's.
fn tone_bound(cycles: f64) -> f64 {
    CEILING / (std::f64::consts::PI * cycles / 4.0).cos() * 1.0002
}

/// The adversarial inputs: each is over the ceiling in true peak and within the oversampler's
/// passband (a tone at most 0.44 of the rate, under the 0.45 of a 0.9 passband).
fn signals() -> Vec<(&'static str, Vec<f32>)> {
    let frames = 4_800;
    let mut all = vec![];
    // A sine at a quarter of the rate at four phases against the sampling grid: at 45 degrees
    // the samples sit at 0.707 of the amplitude and the signal reaches the amplitude
    // between them.
    for (name, deg) in [
        ("fs/4 at 0 degrees", 0.0),
        ("fs/4 at 22.5 degrees", 22.5),
        ("fs/4 at 45 degrees", 45.0),
        ("fs/4 at 67.5 degrees", 67.5),
    ] {
        all.push((name, tone(0.25, f64::to_radians(deg), 1.0, frames)));
    }
    // A tone near the top of the band, whose peak falls between the 4x points.
    all.push(("0.44 fs sine", tone(0.44, 0.3, 1.0, frames)));
    // Bursts of a near-Nyquist tone (40 frames) in gaps of silence (560), at a phase that
    // shifts from burst to burst.
    let mut bursts = vec![0.0f32; frames];
    for (b, chunk) in bursts.chunks_mut(600).enumerate() {
        let t = tone(0.42, 0.7 * b as f64, 1.0, 40);
        chunk[..40].copy_from_slice(&t);
    }
    all.push(("0.42 fs bursts", bursts));
    // Band-limited noise: 300 tones of random phase to 0.44 of the rate, at an amplitude
    // whose peak is well over the ceiling.
    let mut rng = SplitMix64::new(77);
    let mut noise = vec![0.0f64; frames];
    for k in 1..=300 {
        let phase = uniform(&mut rng, 0.0, std::f64::consts::TAU);
        let cycles = 0.44 * f64::from(k) / 300.0;
        for (n, x) in noise.iter_mut().enumerate() {
            *x += (std::f64::consts::TAU * cycles * n as f64 + phase).sin();
        }
    }
    let top = noise.iter().fold(0.0f64, |a, x| a.max(x.abs()));
    all.push((
        "band-limited noise",
        noise.iter().map(|x| (x / top * 1.2) as f32).collect(),
    ));
    all
}

/// The signal at a quarter of the rate and a 45 degree phase has samples at 0.707 of its
/// amplitude: a sample-peak limiter at a ceiling of 0.5 leaves a signal of 0.5 / 0.707 =
/// 0.707, `+3.01 dB` over the ceiling, between its samples.
#[test]
fn a_sample_peak_limiter_leaves_an_fs_4_sine_3_db_over_its_ceiling_between_samples() {
    let x = tone(0.25, f64::to_radians(45.0), 1.0, 4_800);
    let out = render(&mut bank(PeakDetection::Sample), &x);
    let samples = out[2_000..]
        .iter()
        .fold(0.0f64, |a, &y| a.max(f64::from(y).abs()));
    assert!(
        (samples - CEILING).abs() < 1e-4,
        "the samples sit at the ceiling: {samples}"
    );
    let over = db(true_peak(&out[1_000..]) / CEILING);
    let analytic = db(1.0 / std::f64::consts::FRAC_1_SQRT_2);
    assert!(
        (over - analytic).abs() < 0.01,
        "{over} dB over, analytic {analytic} dB"
    );
    assert!(over > LimiterBank::<f32>::TRUE_PEAK_MARGIN.get() + 2.0);
}

/// The same signal through a true-peak limiter reaches the ceiling between its samples, to
/// the passband's ripple: its 4x grid holds the peak.
#[test]
fn a_true_peak_limiter_holds_an_fs_4_sine_at_the_ceiling() {
    let x = tone(0.25, f64::to_radians(45.0), 1.0, 4_800);
    let out = render(&mut bank(PeakDetection::TruePeak), &x);
    let peak = true_peak(&out);
    assert!(
        (db(peak / CEILING)).abs() < 0.01,
        "{} dB from the ceiling",
        db(peak / CEILING)
    );
}

/// Every adversarial signal's output has a true peak within the ceiling and the margin.
#[test]
fn the_true_peak_of_the_output_is_within_the_ceiling_and_its_margin() {
    for (name, x) in signals() {
        let before = true_peak(&x);
        assert!(
            before > CEILING * 1.2,
            "{name} is over the ceiling: {before}"
        );
        let out = render(&mut bank(PeakDetection::TruePeak), &x);
        let after = true_peak(&out);
        assert!(
            after <= bound(),
            "{name}: true peak {after} over the bound {}",
            bound()
        );
    }
}

/// The audio path is the delayed input times a gain: an impulse under the ceiling (a gain of
/// exactly one) comes out whole at the declared latency, at 48 and 44.1 kHz and at a long
/// lookahead, and the latency is the lookahead.
#[test]
fn the_declared_latency_is_the_impulse_peak_delay_of_the_audio_path() {
    for (rate, lookahead) in [
        (SampleRate::SR_48K, Seconds::new(0.005)),
        (SampleRate::SR_44K1, Seconds::new(0.005)),
        (SampleRate::SR_48K, Seconds::new(0.02)),
        (SampleRate::SR_48K, Seconds::new(0.0009)),
    ] {
        let lookahead = Lookahead::of(lookahead);
        let mut bank = bank_at(PeakDetection::TruePeak, lookahead, rate);
        let sample = bank_at(PeakDetection::Sample, lookahead, rate);
        assert_eq!(bank.latency(), sample.latency());
        let mut x = vec![0.0f32; 4_000];
        x[100] = 0.25;
        let out = render(&mut bank, &x);
        let at = out.iter().position(|&y| y != 0.0).expect("an impulse");
        assert_eq!(out[at], 0.25);
        assert_eq!(at, 100 + bank.latency().get());
    }
}

/// The up-sampler's delay is what the oversampler's cascade sums to: an impulse peaks at that
/// point of the 4x signal.
#[test]
fn the_upsample_delay_is_the_impulse_peak_of_the_4x_signal() {
    let plan = Oversampling::new(OversamplingFactor::X4)
        .plan(RATE)
        .unwrap();
    let mut state = plan.state::<f32>(tutti_buffers::MaxBlock::of(Samples::new(256)));
    let mut x = vec![0.0f32; 256];
    x[0] = 1.0;
    state.upsample(Mono::new(&x)).unwrap();
    let (high, _) = state.high_rate();
    let peak = high
        .samples()
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .unwrap()
        .0;
    assert_eq!(peak, plan.upsample_delay());
}

/// `true_peak` takes the lookahead as a floor: a true-peak lookahead under the
/// up-sampler's `1 + U / 4` frames is raised to exactly that, a longer one is kept, and sample
/// detection keeps any.
#[test]
fn the_lookahead_is_a_floor_raised_to_the_upsampler_delay() {
    let needed = 1 + Oversampling::new(OversamplingFactor::X4)
        .plan(RATE)
        .unwrap()
        .upsample_delay()
        / 4;
    let at = |lookahead, detection| detecting(detection, lookahead, RATE).latency();
    let short = Lookahead::of(Seconds::new(0.0002));
    assert_eq!(at(short, PeakDetection::TruePeak), Samples::new(needed));
    assert_eq!(at(short, PeakDetection::Sample), Samples::new(10));
    assert_eq!(at(LOOKAHEAD, PeakDetection::TruePeak), Samples::new(240));
}

/// A lone sample over the ceiling is limited whatever frame it lands on: the detection lines up
/// with the audio.
#[test]
fn an_isolated_peak_is_limited_at_every_alignment() {
    for p in 100..140 {
        let mut x = vec![0.0f32; 1_000];
        x[p] = 1.0;
        let out = render(&mut bank(PeakDetection::TruePeak), &x);
        let at = p + bank(PeakDetection::TruePeak).latency().get();
        assert!(
            f64::from(out[at]) <= CEILING * 1.0001,
            "p = {p}: {}",
            out[at]
        );
        assert!(true_peak(&out) <= bound(), "p = {p}");
    }
}

/// A loud sample followed by one at the ceiling: the second needs no gain by itself, and its
/// gain is right only if its window takes the groups that cover the signal between the two,
/// so the pair's overshoot between them stays within the margin.
#[test]
fn the_gain_of_a_quiet_sample_takes_the_groups_covering_the_instants_before_it() {
    for (lookahead, block) in [(LOOKAHEAD, CHUNK), (shortest(), 1)] {
        for p in 100..108 {
            let mut x = vec![0.0f32; 1_000];
            x[p] = 1.0;
            x[p + 1] = CEILING as f32;
            let out = run_in(
                &mut bank_at(PeakDetection::TruePeak, lookahead, RATE),
                &[&x],
                [None, None],
                block,
            )
            .remove(0);
            let after = true_peak(&out);
            assert!(after <= bound(), "p = {p}: {after} over {}", bound());
        }
    }
}

/// A NaN sample is delayed to its own frame and nowhere else; an infinity is a finite,
/// limited sample; and a finite sample past the detector's 2^100 clamp is still limited to the
/// ceiling (the frame's own sample is read as it is).
#[test]
fn non_finite_and_huge_input_is_limited_and_does_not_spread() {
    let latency = bank(PeakDetection::TruePeak).latency().get();
    let mut x = tone(0.1, 0.0, 0.4, 900);
    x[200] = f32::NAN;
    x[300] = f32::INFINITY;
    x[301] = f32::NEG_INFINITY;
    x[500] = 1.0e36;
    x[501] = f32::MAX;
    let out = render(&mut bank(PeakDetection::TruePeak), &x);
    for (i, y) in out.iter().enumerate() {
        if i == 200 + latency {
            assert!(y.is_nan(), "the NaN is delayed to frame {i}");
        } else {
            assert!(y.is_finite(), "frame {i}: {y}");
        }
    }
    for p in [300, 301, 500, 501] {
        let y = f64::from(out[p + latency]).abs();
        assert!(y <= CEILING * 1.0001, "frame {p}: {y}");
    }
}

/// At the shortest lookahead and in one-frame blocks, as at the default and in whole blocks:
/// the steady tones stay within the bound of their own frequency (the fs/4 tone at every
/// phase, and a 0.44 tone), the bursts and the noise within the margin, and a lone sample is
/// limited at every alignment.
#[test]
fn the_shortest_lookahead_and_one_frame_blocks_keep_the_bounds() {
    for (lookahead, block) in [
        (LOOKAHEAD, CHUNK),
        (LOOKAHEAD, 1),
        (shortest(), CHUNK),
        (shortest(), 1),
    ] {
        let make = || bank_at(PeakDetection::TruePeak, lookahead, RATE);
        for (name, x) in signals() {
            let out = run_in(&mut make(), &[&x], [None, None], block).remove(0);
            let cycles = if name.starts_with("fs/4") {
                Some(0.25)
            } else if name == "0.44 fs sine" {
                Some(0.44)
            } else {
                None
            };
            let after = true_peak(&out[1_000..]);
            let limit = cycles.map_or(bound(), tone_bound);
            assert!(after <= limit, "{name} at {block}: {after} over {limit}");
            if cycles.is_some() {
                // The bound is not slack: the tone reaches the ceiling.
                assert!(after > CEILING * 0.98, "{name}: {after}");
            }
        }
        for p in 100..140 {
            let mut x = vec![0.0f32; 1_000];
            x[p] = 1.0;
            let mut b = make();
            let latency = b.latency().get();
            let out = run_in(&mut b, &[&x], [None, None], block).remove(0);
            assert!(f64::from(out[p + latency]) <= CEILING * 1.0001, "p = {p}");
        }
    }
}
