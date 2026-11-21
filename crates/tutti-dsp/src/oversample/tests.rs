use core::f64::consts::TAU;

use tutti_buffers::MaxBlock;
use tutti_buffers::{Float, Mono, MonoMut};
use tutti_units::{Db, SampleRate, Samples};

use super::*;
use crate::testing::amplitude_at;

const RATE: SampleRate = SampleRate::SR_48K;
const BASE: f64 = 48_000.0;
const FACTORS: [OversamplingFactor; 3] = [
    OversamplingFactor::X2,
    OversamplingFactor::X4,
    OversamplingFactor::X8,
];
/// The window of the spectral tests, in base frames; a tone at bin `k` is `k / WINDOW` of the
/// base rate and completes `k` periods in it.
const WINDOW: usize = 4096;
/// Frames run before a window, past the round trip's response (`2 * latency`, under 200).
const SETTLE: usize = 512;
/// The tone in the passband: bin 1500 is 0.73 of the base Nyquist, under the 0.9 edge.
const TONE_BIN: usize = 1500;
/// A half-band's ripple and stopband for the default 100 dB: `10^(-100/20)`.
const DELTA: f64 = 1e-5;
/// A regression margin: the measured images and aliases sit at least 5 dB under
/// `-attenuation` (the bounds hold with the margin at -2 dB), and a bound 3 dB short of the
/// attenuation fails on a real loss of rejection.
const MARGIN_DB: f64 = 3.0;

fn plan(factor: OversamplingFactor) -> OversamplingPlan {
    Oversampling::new(factor).plan(RATE).unwrap()
}

fn db(amplitude: f64) -> f64 {
    20.0 * amplitude.log10()
}

fn sine(bin: usize, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| libm::sin(TAU * bin as f64 * n as f64 / WINDOW as f64) as f32)
        .collect()
}

/// `x` up and down in blocks of `block`, `process` between at the high rate (the oversampled
/// block in, its processed form out).
fn through(
    os: &mut Oversampler,
    x: &[f32],
    block: usize,
    mut process: impl FnMut(&[f32], &mut [f32]),
) -> Vec<f32> {
    let mut y = vec![0.0; x.len()];
    for (xs, ys) in x.chunks(block).zip(y.chunks_mut(block)) {
        os.upsample(Mono::new(xs));
        let (high, mut out) = os.high_rate();
        process(high.as_ref(), out.as_mut());
        os.downsample(MonoMut::new(ys));
    }
    y
}

fn pass_through(os: &mut Oversampler, x: &[f32], block: usize) -> Vec<f32> {
    through(os, x, block, |high, out| out.copy_from_slice(high))
}

/// The latency is the round trip's group delay: the stage delays `D_j` (odd, each at its own
/// rate) summed at the top rate, padded to whole base frames. Here from `FirDesign` alone, for
/// each factor.
#[test]
fn the_latency_is_the_padded_round_trip_of_the_stage_delays() {
    for factor in FACTORS {
        let stages = factor.get().trailing_zeros() as usize;
        let delays: Vec<usize> = (0..stages)
            .map(|j| {
                let wide = f64::from(1u32 << j);
                let transition = Hz::new(BASE * (wide - 0.9).min(wide / 2.0));
                FirDesign::new(FirResponse::HalfBand, transition)
                    .attenuation(Db::new(100.0))
                    .design(SampleRate::new(BASE * f64::from(2u32 << j)).unwrap())
                    .unwrap()
                    .delay()
                    .get()
            })
            .collect();
        assert!(delays.iter().all(|d| d % 2 == 1), "{factor:?}: {delays:?}");
        let top: usize = delays
            .iter()
            .enumerate()
            .map(|(j, d)| 2 * d * (1 << (stages - 1 - j)))
            .sum();
        let padded = top.next_multiple_of(factor.get());
        assert_eq!(
            plan(factor).latency(),
            Samples::new(padded / factor.get()),
            "{factor:?}: {delays:?}"
        );
    }
    // A pad is needed past 2x: with D_0 and D_1 odd, 4x's round trip is half a base frame over.
    let (two, four) = (plan(OversamplingFactor::X2), plan(OversamplingFactor::X4));
    assert!(four.shared.lead > 0 && two.shared.lead == 0);
}

/// An impulse through up and down peaks exactly `latency()` frames later, in blocks of any size,
/// the response sums to the unity DC gain, and it is zero past its support, twice the latency.
#[test]
fn an_impulse_peaks_at_the_latency() {
    for factor in FACTORS {
        let plan = plan(factor);
        let latency = plan.latency().get();
        for block in [1, 7, 64, 700] {
            let mut os = plan.state::<f32>(MaxBlock::of(Samples::new(block)));
            let mut x = vec![0.0; 1024];
            x[0] = 1.0;
            let y = pass_through(&mut os, &x, block);
            let peak = (0..y.len())
                .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
                .unwrap();
            assert_eq!(peak, latency, "{factor:?} block {block}");
            let dc: f64 = y.iter().map(|&s| f64::from(s)).sum();
            assert!(
                (dc - 1.0).abs() < 4.0 * 3.0 * DELTA,
                "{factor:?}: the response sums to {dc}, not the unity DC gain"
            );
            assert!(
                y[2 * latency + 1..].iter().all(|&s| s == 0.0),
                "{factor:?}: the response outlives twice the latency"
            );
        }
    }
}

/// A sine below the passband edge round-trips as itself delayed by `latency()`: the error is the
/// ripple of the `2 * stages` half-band passes it crosses, each `10^(-100/20)`, times 2 for
/// Kaiser's estimate.
#[test]
fn a_passband_sine_round_trips_delayed_within_the_ripple() {
    for factor in FACTORS {
        let stages = factor.get().trailing_zeros() as f64;
        let bound = 2.0 * 2.0 * stages * DELTA;
        let plan = plan(factor);
        let latency = plan.latency().get();
        for bin in [60, TONE_BIN, 1800] {
            let x = sine(bin, SETTLE + WINDOW);
            let mut os = plan.state::<f32>(MaxBlock::of(Samples::new(128)));
            let y = pass_through(&mut os, &x, 100);
            let worst = (SETTLE..x.len())
                .map(|n| (f64::from(y[n]) - f64::from(x[n - latency])).abs())
                .fold(0.0, f64::max);
            assert!(worst < bound, "{factor:?} bin {bin}: {worst} vs {bound}");
            let gain = amplitude_at(&y[SETTLE..], bin as f64 / WINDOW as f64 * BASE, BASE);
            assert!(
                (gain - 1.0).abs() < bound,
                "{factor:?} bin {bin}: gain {gain}"
            );
        }
    }
}

/// The images of a tone `f` sit at `m * base ± f` in the oversampled signal, `m = 1, 2, ..`
/// below its Nyquist, and are at least `attenuation - 3 dB` under it; the tone itself is unity.
#[test]
fn the_up_sampler_rejects_its_images() {
    for factor in FACTORS {
        let f = TONE_BIN as f64 / WINDOW as f64 * BASE;
        let f_hi = BASE * factor.get() as f64;
        let x = sine(TONE_BIN, SETTLE + WINDOW);
        let mut os = plan(factor).state::<f32>(MaxBlock::of(Samples::new(256)));
        let mut high = Vec::new();
        through(&mut os, &x, 256, |h, out| {
            high.extend_from_slice(h);
            out.fill(0.0);
        });
        let window = &high[SETTLE * factor.get()..];
        assert!(
            (amplitude_at(window, f, f_hi) - 1.0).abs() < 1e-4,
            "{factor:?}"
        );
        let mut checked = 0;
        for m in 1..=factor.get() / 2 {
            for image in [m as f64 * BASE - f, m as f64 * BASE + f] {
                if image >= f_hi / 2.0 {
                    continue;
                }
                checked += 1;
                let level = db(amplitude_at(window, image, f_hi));
                assert!(
                    level <= -100.0 + MARGIN_DB,
                    "{factor:?}: image at {image} Hz is {level} dB"
                );
            }
        }
        assert!(checked >= 2 * factor.get() / 2 - 1, "{factor:?}: {checked}");
    }
}

/// A tone in the oversampled signal that aliases onto `f` when decimated, `m * base - f` for
/// each `m` below the oversampled Nyquist, is at least `attenuation - 3 dB` down after the
/// down-sampler.
#[test]
fn the_down_sampler_rejects_what_would_alias() {
    for factor in FACTORS {
        let f = TONE_BIN as f64 / WINDOW as f64 * BASE;
        let f_hi = BASE * factor.get() as f64;
        for m in 1..=factor.get() / 2 {
            let g = m as f64 * BASE - f;
            let mut os = plan(factor).state::<f32>(MaxBlock::of(Samples::new(256)));
            let mut n_hi = 0usize;
            let y = through(&mut os, &vec![0.0; SETTLE + WINDOW], 256, |_, out| {
                for o in out.iter_mut() {
                    *o = libm::sin(TAU * g * n_hi as f64 / f_hi) as f32;
                    n_hi += 1;
                }
            });
            let level = db(amplitude_at(&y[SETTLE..], f, BASE));
            assert!(
                level <= -100.0 + MARGIN_DB,
                "{factor:?}: the tone at {g} Hz leaves {level} dB at {f} Hz"
            );
        }
    }
}

/// The oversampled signal of a block is `factor` times as long, and a block longer than the
/// prepared maximum is cut to it rather than overrun.
#[test]
fn a_block_has_factor_times_the_frames() {
    for factor in FACTORS {
        let mut os = plan(factor).state::<f32>(MaxBlock::of(Samples::new(32)));
        os.upsample(Mono::new(&[0.5f32; 20]));
        let (high, out) = os.high_rate();
        assert_eq!(high.frames(), Samples::new(20 * factor.get()));
        assert_eq!(out.frames(), Samples::new(20 * factor.get()));
        os.upsample(Mono::new(&[0.5f32; 100]));
        assert_eq!(os.high_rate().0.frames(), Samples::new(32 * factor.get()));
    }
}

/// A reset leaves every delay line zero: silence in is exact silence out, whatever ran before.
#[test]
fn a_reset_leaves_silence_exact() {
    for factor in FACTORS {
        let mut os = plan(factor).state::<f32>(MaxBlock::of(Samples::new(64)));
        pass_through(&mut os, &crate::testing::noise(3, 500, 1.0), 64);
        os.reset();
        let y = pass_through(&mut os, &[0.0; 500], 64);
        assert!(y.iter().all(|&s| s == 0.0), "{factor:?}");
    }
}

/// A block in `f32` or `f64` runs through the same filters, delayed by the latency within the
/// ripple.
#[test]
fn a_block_round_trips_in_either_format() {
    fn run<F: Float>() {
        let plan = plan(OversamplingFactor::X2);
        let latency = plan.latency().get();
        let mut os = plan.state::<F>(MaxBlock::of(Samples::new(1024)));
        let x: Vec<f64> = (0..1024)
            .map(|n| libm::sin(TAU * 100.0 * n as f64 / WINDOW as f64))
            .collect();
        let input: Vec<F> = x.iter().map(|&v| F::from_f64(v)).collect();
        os.upsample(Mono::new(&input));
        let (high, mut out) = os.high_rate();
        out.as_mut().copy_from_slice(high.as_ref());
        let mut y = vec![F::ZERO; 1024];
        os.downsample(MonoMut::new(&mut y));
        let worst = (4 * latency..1024)
            .map(|n| (y[n].to_f64() - x[n - latency]).abs())
            .fold(0.0, f64::max);
        assert!(worst < 4.0 * DELTA, "{worst}");
    }
    run::<f32>();
    run::<f64>();
}

/// A design the half-band cannot make is the FIR design's refusal.
#[test]
fn a_bad_attenuation_is_the_designs_refusal() {
    let bad = Oversampling::new(OversamplingFactor::X2).attenuation(Db::UNITY);
    assert!(matches!(
        bad.plan(RATE),
        Err(FirError::NonPositiveAttenuation(_))
    ));
}

/// A rate at which no half-band can be designed, where the transition width underflows or the
/// scaled rate overflows, is refused at `plan`, for every factor; 48 kHz plans.
#[test]
fn a_rate_no_half_band_fits_is_refused() {
    for factor in [
        OversamplingFactor::X2,
        OversamplingFactor::X4,
        OversamplingFactor::X8,
    ] {
        let o = Oversampling::new(factor);
        for hz in [5e-324, 1e308] {
            let rate = SampleRate::new(hz).expect("a positive, finite rate");
            assert!(o.plan(rate).is_err(), "{factor:?} at {hz} Hz");
        }
        assert!(o.plan(RATE).is_ok(), "{factor:?} at the test rate");
    }
}

/// `Passband` keeps the range-checked shape: `(0, 1)` of the base Nyquist.
#[test]
fn a_passband_keeps_the_range_checked_shape() {
    let c = tutti_fixtures::RangeContract {
        new: Passband::new,
        get: Passband::get,
        of: Some(Passband::of),
        valid: &[0.001, 0.5, 0.9, 0.999],
        invalid: &[0.0, 1.0, -0.5, 1.5, f32::NAN, f32::INFINITY],
    };
    tutti_fixtures::check_range_contract(&c);
}

/// A generator's block is `factor` times the frames at the higher rate, and a constant written
/// there is the constant at the base rate once the half-band's response has passed: the
/// down-sampler's DC gain is 1 within its ripple.
#[test]
fn a_source_block_is_written_at_the_higher_rate_and_down_sampled() {
    for factor in FACTORS {
        let mut os = plan(factor).state::<f64>(MaxBlock::of(Samples::new(256)));
        let mut out = vec![0.0f64; 256];
        for _ in 0..2 {
            let mut high = os.source(Samples::new(256));
            assert_eq!(high.frames().get(), 256 * factor.get());
            high.samples_mut().fill(1.0);
            os.downsample(MonoMut::new(&mut out));
        }
        assert!(
            out[128..].iter().all(|&y| (y - 1.0).abs() <= DELTA),
            "{factor:?}"
        );
    }
}

/// A sine written at the higher rate comes out at the base rate as `sin(w (n - d))` for `d =
/// down_delay()`, within the ripple of the `stages` half-bands (`2 * DELTA` each by Kaiser's
/// estimate): the delay is the exact linear-phase one, a fraction of a base frame for the
/// factors where the round trip is not a multiple of the factor.
#[test]
fn the_down_delay_is_the_delay_of_a_sine_written_at_the_higher_rate() {
    for factor in FACTORS {
        let stages = factor.get().trailing_zeros() as f64;
        let plan = plan(factor);
        let delay = plan.down_delay().get();
        let high = BASE * factor.get() as f64;
        let f = TONE_BIN as f64 / WINDOW as f64 * BASE;
        let mut os = plan.state::<f64>(MaxBlock::of(Samples::new(WINDOW)));
        let mut out = vec![0.0f64; WINDOW];
        for pass in 0..2 {
            let mut written = os.source(Samples::new(WINDOW));
            for (m, y) in written.samples_mut().iter_mut().enumerate() {
                let n = (pass * WINDOW * factor.get() + m) as f64;
                *y = (TAU * f * n / high).sin();
            }
            os.downsample(MonoMut::new(&mut out));
        }
        let worst = (SETTLE..WINDOW)
            .map(|n| {
                let want = (TAU * f * (n as f64 + WINDOW as f64 - delay) / BASE).sin();
                (out[n] - want).abs()
            })
            .fold(0.0, f64::max);
        assert!(worst < 2.0 * stages * DELTA, "{factor:?}: {worst}");
    }
}
