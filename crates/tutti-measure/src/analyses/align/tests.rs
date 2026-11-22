use super::*;
use crate::cross_correlate;
use crate::test_rng::uniform;
use crate::{AboveNyquist, EmptyFrequencyRange};
use tutti_rt::SplitMix64;
use tutti_units::Hz;

fn noise(len: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..len).map(|_| uniform(&mut rng, -1.0, 1.0)).collect()
}

/// `x` delayed by `delay` samples (negative: advanced), same length.
fn shifted(x: &[f64], delay: isize) -> Vec<f64> {
    (0..x.len() as isize)
        .map(|n| {
            usize::try_from(n - delay)
                .ok()
                .and_then(|i| x.get(i))
                .copied()
                .unwrap_or(0.0)
        })
        .collect()
}

fn find(reference: &[f64], take: &[f64], max_lag: usize) -> Alignment {
    let cfg = AlignConfig::new(Samples::new(max_lag));
    align(reference, take, &cfg).unwrap()
}

/// The lag plain cross-correlation picks for the same pair.
fn plain_lag(reference: &[f64], take: &[f64], max_lag: usize) -> isize {
    cross_correlate(Mono::new(take), Mono::new(reference))
        .peak_within(Samples::new(max_lag))
        .unwrap()
        .lag()
}

/// `align` at 48 kHz, the rate the tests that do not name one read at.
fn align(reference: &[f64], take: &[f64], config: &AlignConfig) -> Result<Alignment, AlignError> {
    align_at(SampleRate::SR_48K, reference, take, config)
}

fn align_at(
    rate: SampleRate,
    reference: &[f64],
    take: &[f64],
    config: &AlignConfig,
) -> Result<Alignment, AlignError> {
    super::align(config, Mono::new(reference), Mono::new(take), rate)
}

fn band(low: f64, high: f64) -> FrequencyBand {
    FrequencyBand::new(Hz::new(low), Hz::new(high)).unwrap()
}

/// A signal that fills the band, evaluable at fractional times: four
/// hundred sines with seeded frequencies below 0.4 cycles per sample.
fn sines(len: usize, delay: f64) -> Vec<f64> {
    let mut rng = SplitMix64::new(9);
    let partials: Vec<(f64, f64, f64)> = (0..400)
        .map(|_| {
            (
                uniform(&mut rng, 0.02, 0.4),
                uniform(&mut rng, 0.0, core::f64::consts::TAU),
                uniform(&mut rng, 0.5, 1.0),
            )
        })
        .collect();
    (0..len)
        .map(|n| {
            let t = n as f64 - delay;
            partials
                .iter()
                .map(|&(f, p, a)| a * (core::f64::consts::TAU * f * t + p).sin())
                .sum()
        })
        .collect()
}

/// A sine of `amp` and the given period, in samples, added to `x`.
fn with_tone(mut x: Vec<f64>, amp: f64, period: f64, phase: f64) -> Vec<f64> {
    for (n, v) in x.iter_mut().enumerate() {
        *v += amp * (core::f64::consts::TAU * n as f64 / period + phase).sin();
    }
    x
}

/// White noise with everything outside `low..high` (cycles per sample)
/// removed in the frequency domain.
fn band_noise(len: usize, seed: u64, low: f64, high: f64) -> Vec<f64> {
    let mut fft = FftScratch::new();
    let mut bins = vec![Complex64::default(); len / 2 + 1];
    fft.forward_unwindowed(Mono::new(&noise(len, seed)), &mut bins);
    for (k, b) in bins.iter_mut().enumerate() {
        let f = k as f64 / len as f64;
        if f < low || f > high {
            *b = Complex64::default();
        }
    }
    let mut out = vec![0.0; len];
    fft.inverse(&bins, tutti_buffers::MonoMut::new(&mut out));
    out
}

/// Noise through two one-pole lowpasses: a spectrum falling 12 dB per
/// octave, some 70 dB from the lowest bins to the highest.
fn tilted_noise(len: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    let (mut a, mut b) = (0.0f64, 0.0f64);
    (0..len)
        .map(|_| {
            a = 0.95 * a + uniform(&mut rng, -1.0, 1.0);
            b = 0.95 * b + a;
            b
        })
        .collect()
}

/// A take delayed by a whole number of samples is found at exactly that
/// lag, either sign, short or long, with a distinct peak.
#[test]
fn a_delayed_copy_is_found_at_exactly_its_delay() {
    let x = noise(4096, 1);
    for delay in [0isize, 1, -1, 123, -123, 1500, -1500] {
        let a = find(&x, &shifted(&x, delay), 2000);
        assert_eq!(a.lag(), delay, "delay {delay}");
        assert!(
            a.offset().abs() < 0.05,
            "delay {delay}: offset {}",
            a.offset()
        );
        assert!(
            a.confidence().get() > 0.8,
            "delay {delay}: {:?}",
            a.confidence()
        );
    }
}

/// A band-limited signal delayed by a fraction of a sample is recovered
/// to within a tenth of a sample by the parabolic refinement.
#[test]
fn a_fractional_delay_is_recovered_within_a_tenth_of_a_sample() {
    for delay in [10.5f64, 3.25, -7.75, 100.5] {
        let a = find(&sines(4096, 0.0), &sines(4096, delay), 300);
        let err = (a.delay_samples() - delay).abs();
        assert!(err < 0.1, "delay {delay}: found {}", a.delay_samples());
    }
}

/// Scaling either input by a large or tiny gain changes neither lag nor
/// confidence: the PHAT floor is relative to the spectrum's own level.
/// The inputs carry a hum, so that a weighting that falls back to energy
/// at low level (plain correlation) would follow the hum, not the delay.
#[test]
fn scaling_an_input_changes_nothing() {
    let x = noise(8192, 2);
    let reference = with_tone(x.clone(), 3.0, 480.0, 0.0);
    let take = with_tone(shifted(&x, 77), 3.0, 480.0, 1.3);
    let base = find(&reference, &take, 1000);
    assert_eq!(base.lag(), 77);
    let scaled = |v: &[f64], g: f64| v.iter().map(|s| s * g).collect::<Vec<f64>>();
    for (gr, gt) in [
        (1000.0f64, 1.0f64),
        (1.0, 1000.0),
        (1e-6, 1.0),
        (1.0, 1e-6),
        (1e-6, 1e-6),
    ] {
        let a = find(&scaled(&reference, gr), &scaled(&take, gt), 1000);
        assert_eq!(a.lag(), 77, "gains {gr}, {gt}");
        let drift = (a.confidence().get() - base.confidence().get()).abs();
        assert!(drift < 0.02, "gains {gr}, {gt}: drift {drift}");
    }
}

/// A loud interferer (a tone with its own phase in each take) steals plain
/// cross-correlation; PHAT weights every bin alike and keeps the delay.
#[test]
fn phat_keeps_the_delay_where_plain_correlation_follows_a_hum() {
    let x = noise(8192, 2);
    let reference = with_tone(x.clone(), 10.0, 480.0, 0.0);
    let take = with_tone(shifted(&x, 77), 10.0, 480.0, 1.3);
    assert_ne!(
        plain_lag(&reference, &take, 1000),
        77,
        "plain no longer fails"
    );
    let a = find(&reference, &take, 1000);
    assert_eq!(a.lag(), 77);
    assert!(a.confidence().get() > 0.3, "{:?}", a.confidence());
}

/// The same on a source whose spectrum falls 12 dB per octave, with a hum
/// well above it: whitening has to span the source's own dynamic range, and
/// plain correlation has neither the hum nor the tilt in its favour.
#[test]
fn phat_keeps_the_delay_of_a_steeply_tilted_source_under_a_hum() {
    let x = tilted_noise(8192, 21);
    let reference = with_tone(x.clone(), 100.0, 480.0, 0.0);
    let take = with_tone(shifted(&x, 77), 100.0, 480.0, 1.3);
    assert_ne!(
        plain_lag(&reference, &take, 1000),
        77,
        "plain no longer fails"
    );
    let a = find(&reference, &take, 1000);
    assert_eq!(a.lag(), 77);
    assert!(a.confidence().get() > 0.5, "{:?}", a.confidence());
}

/// A band keeps what two takes share: a narrow common signal under
/// independent broadband noise is sharper within the band than outside it.
#[test]
fn a_band_limit_leaves_out_what_does_not_correlate() {
    let n = 2048;
    let mut rng = SplitMix64::new(31);
    let partials: Vec<(f64, f64)> = (0..12)
        .map(|_| {
            (
                uniform(&mut rng, 0.04, 0.08),
                uniform(&mut rng, 0.0, core::f64::consts::TAU),
            )
        })
        .collect();
    let common = |t: f64| -> f64 {
        partials
            .iter()
            .map(|&(f, p)| (core::f64::consts::TAU * f * t + p).sin())
            .sum()
    };
    let (a, b) = (noise(n, 41), noise(n, 42));
    let reference: Vec<f64> = (0..n).map(|i| 3.0 * a[i] + common(i as f64)).collect();
    let take: Vec<f64> = (0..n)
        .map(|i| 3.0 * b[i] + common(i as f64 - 77.0))
        .collect();

    let open = AlignConfig::new(Samples::new(500));
    let banded = open.band(band(0.03, 0.09));
    assert_eq!(
        banded.band.map(|b| (b.min(), b.max())),
        Some((Hz::new(0.03), Hz::new(0.09)))
    );
    // A rate of one sample per second: the band is cycles per sample.
    let rate = SampleRate::of(1.0);
    let open = align_at(rate, &reference, &take, &open).unwrap();
    let banded = align_at(rate, &reference, &take, &banded).unwrap();
    assert_eq!(banded.lag(), 77);
    assert!(
        banded.confidence().get() > 2.0 * open.confidence().get(),
        "banded {:?}, open {:?}",
        banded.confidence(),
        open.confidence()
    );
}

/// Out-of-band energy does not set the PHAT level: with bins a million
/// times stronger outside the band, the in-band bins still come out at
/// unit magnitude, and out-of-band bins are zero.
#[test]
fn out_of_band_energy_does_not_set_the_level() {
    // 9 bins of a 16-point transform; at rate 16, bin `k` is `k` Hz.
    let mut bins = vec![Complex64::new(3.0, -4.0); 9];
    bins[1] = Complex64::new(5.0e6, 0.0);
    let band = AlignConfig::new(Samples::new(1)).band(band(4.0, 6.0)).band;
    assert!(phase_transform(&mut bins, band, SampleRate::of(16.0)));
    for (k, bin) in bins.iter().enumerate() {
        if (4..=6).contains(&k) {
            assert!((bin.norm() - 1.0).abs() < 1e-3, "bin {k}: {bin}");
            assert!((bin.re - 0.6).abs() < 1e-3 && (bin.im + 0.8).abs() < 1e-3);
        } else {
            assert_eq!(*bin, Complex64::default(), "bin {k}");
        }
    }
}

/// A decaying, reverb-like filter on the take leaves the lag where it was.
///
/// It is a robustness pin, not a discriminator: plain correlation also finds this lag,
/// and the hum tests are the ones that tell PHAT from it.
#[test]
fn a_decaying_filter_on_the_take_keeps_the_lag() {
    let x = noise(8192, 4);
    let delayed = shifted(&x, 77);
    let taps: Vec<f64> = (0..64).map(|k| 0.9f64.powi(k)).collect();
    let take: Vec<f64> = (0..delayed.len())
        .map(|n| {
            taps.iter()
                .enumerate()
                .filter(|&(k, _)| k <= n)
                .map(|(k, h)| h * delayed[n - k])
                .sum()
        })
        .collect();
    let a = find(&x, &take, 1000);
    assert_eq!(a.lag(), 77);
    assert!(a.confidence().get() > 0.5, "{:?}", a.confidence());
}

/// Only lags within the bound are chosen, the bound itself included; a
/// window of one lag has no competitor and reads no confidence.
#[test]
fn the_max_lag_bound_is_respected() {
    let x = noise(2048, 6);
    let take = shifted(&x, 200);
    assert_eq!(find(&x, &take, 200).lag(), 200);
    let a = find(&x, &take, 150);
    assert!(a.lag().abs() <= 150, "lag {}", a.lag());
    let one = find(&x, &x, 0);
    assert_eq!(one.lag(), 0);
    assert_eq!(one.confidence(), Confidence::NONE);
}

/// A peak that spans several lags (a band-limited signal, a perfect delay)
/// still reads as clear: the flanks of its lobe are not a second peak.
#[test]
fn a_wide_peak_is_not_its_own_runner_up() {
    let cfg = AlignConfig::new(Samples::new(300)).band(band(1000.0, 4000.0));
    let x = band_noise(4096, 5, 1000.0 / 48_000.0, 4000.0 / 48_000.0);
    let clean = align(&x, &shifted(&x, 40), &cfg).unwrap();
    assert_eq!(clean.lag(), 40);
    assert!(clean.confidence().get() > 0.7, "{:?}", clean.confidence());

    let (n1, n2) = (noise(4096, 6), noise(4096, 7));
    let reference: Vec<f64> = x.iter().zip(&n1).map(|(a, b)| a + 0.5 * b).collect();
    let take: Vec<f64> = shifted(&x, 40)
        .iter()
        .zip(&n2)
        .map(|(a, b)| a + 0.5 * b)
        .collect();
    let noisy = align(&reference, &take, &cfg).unwrap();
    assert_eq!(noisy.lag(), 40);
    assert!(noisy.confidence().get() > 0.7, "{:?}", noisy.confidence());
}

/// The lobe rule on hand-made profiles (peak at index 5, lag = index):
/// ripple above half height stays in the lobe, the flank beyond half height
/// is walked down before the runner-up is read, an exact tie at the top
/// does not read as zero, and a lobe that fills the window has no runner-up.
#[test]
fn the_lobe_rule_on_made_up_profiles() {
    let read = |profile: &[f64], lag: isize| {
        let hi = profile.len() as isize - 1;
        distinctness(
            |l| {
                profile
                    .get(l as usize)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY)
            },
            lag,
            profile[lag as usize],
            (0, hi),
        )
        .get()
    };
    let ripple = [
        0.0, 0.0, 0.1, 0.5, 0.9, 1.0, 0.9, 0.95, 0.7, 0.3, 0.1, 0.35, 0.1,
    ];
    assert!((read(&ripple, 5) - 0.65).abs() < 1e-12);
    let flank = [0.0, 0.0, 0.1, 0.4, 0.8, 1.0, 0.8, 0.45, 0.3, 0.1, 0.25, 0.1];
    assert!((read(&flank, 5) - 0.75).abs() < 1e-12);
    let tie = [0.0, 0.1, 0.6, 1.0, 1.0, 0.6, 0.1, 0.0, 0.2, 0.0];
    assert!((read(&tie, 3) - 0.8).abs() < 1e-12);
    assert_eq!(read(&[1.0, 0.9, 0.8], 0), 0.0);
}

/// Unrelated noise has no distinct peak. Over the roughly 2000 lags of the
/// window the top two of that many Gaussian-like values differ by about
/// `1 / sqrt(2 ln n)` of a deviation against a maximum near `3.5`
/// deviations, so confidence sits near 0.07; the bound allows four times
/// that.
#[test]
fn unrelated_noise_has_low_confidence() {
    for seed in 10..14 {
        let a = find(&noise(8192, seed), &noise(8192, seed + 100), 1000);
        assert!(
            a.confidence().get() < 0.3,
            "seed {seed}: {:?}",
            a.confidence()
        );
    }
}

/// The offset is the parabola's only where the three points bend down and
/// sit inside the overlap: a delta one lag right of the fitted lag, a NaN
/// and a lag at the edge of the overlap (a neighbour missing) all give zero.
#[test]
fn the_offset_is_zero_unless_three_points_bend_down_inside_the_overlap() {
    let x = noise(8, 1);
    let mut fft = FftScratch::new();
    let delta_at_one = cross_correlate_shaped(&x, &x, &mut fft, |bins| {
        let size = ((bins.len() - 1) * 2) as f64;
        for (k, bin) in bins.iter_mut().enumerate() {
            let w = -core::f64::consts::TAU * k as f64 / size;
            *bin = Complex64::new(w.cos(), w.sin());
        }
    });
    assert_eq!(delta_at_one.peak().unwrap().lag(), 1);
    assert_eq!(parabolic_offset(&delta_at_one, 0), 0.0);
    assert_eq!(parabolic_offset(&delta_at_one, delta_at_one.min_lag()), 0.0);
    assert_eq!(parabolic_offset(&delta_at_one, delta_at_one.max_lag()), 0.0);

    let nan = cross_correlate_shaped(&x, &x, &mut fft, |bins| {
        bins.fill(Complex64::new(f64::NAN, 0.0));
    });
    assert_eq!(parabolic_offset(&nan, 0), 0.0);
}

/// Delay in samples and seconds, and the input checks.
#[test]
fn delay_converts_and_bad_input_is_refused() {
    let a = Alignment {
        lag: 48,
        offset: 0.25,
        confidence: Confidence::CERTAIN,
    };
    assert_eq!(a.delay_samples(), 48.25);
    assert!((a.delay(SampleRate::SR_48K).get() - 48.25 / 48_000.0).abs() < 1e-9);

    let cfg = AlignConfig::new(Samples::new(10));
    assert_eq!(cfg.band, None);
    let one = [1.0f64];
    assert!(matches!(
        align(&[], &one, &cfg),
        Err(AlignError::InsufficientInput { .. })
    ));
    assert!(matches!(
        align(&one, &[], &cfg),
        Err(AlignError::InsufficientInput { .. })
    ));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let x = [1.0, bad, 2.0];
        assert_eq!(align(&x, &one, &cfg), Err(AlignError::NonFiniteInput));
        assert_eq!(align(&one, &x, &cfg), Err(AlignError::NonFiniteInput));
    }
    // Silence, and a band between two bins: nothing to correlate.
    let quiet = [0.0f64; 64];
    let x = noise(64, 3);
    assert_eq!(align(&x, &quiet, &cfg), Err(AlignError::NoSignal));
    assert_eq!(align(&quiet, &x, &cfg), Err(AlignError::NoSignal));
    let gap = cfg.band(band(100.0, 200.0));
    assert_eq!(align(&x, &x, &gap), Err(AlignError::NoSignal));

    // A band above Nyquist is refused where the rate binds.
    let high = cfg.band(band(100.0, 30_000.0));
    assert_eq!(
        align(&x, &x, &high),
        Err(AlignError::Nyquist(AboveNyquist {
            freq: Hz::new(30_000.0),
            nyquist: Hz::new(24_000.0)
        }))
    );
    assert!(matches!(
        FrequencyBand::try_from(Hz::new(500.0)..=Hz::new(100.0)),
        Err(EmptyFrequencyRange { .. })
    ));
}

/// The meter fed the take in uneven chunks finds the alignment `align` finds of
/// the whole take, and a reference at another rate is refused.
#[test]
fn the_align_meter_over_uneven_chunks_equals_align() {
    let reference = noise(4000, 3);
    let take = shifted(&reference, 25);
    let config = AlignConfig::new(Samples::new(100));
    let rate = SampleRate::SR_48K;
    let whole = super::align(&config, Mono::new(&reference), Mono::new(&take), rate).unwrap();
    assert_eq!(whole.lag(), 25);
    let wave = Wave::from_mono(rate, Mono::new(&reference));
    let measure = AlignTo::new(config, wave.clone());
    let (items, found) = crate::fold_uneven(&measure, rate, take.len(), |meter, at, out| {
        meter.push(Mono::new(&take[at]), out)
    })
    .unwrap();
    assert!(items.is_empty());
    assert_eq!(found, Ok(whole));
    assert!(matches!(
        Measure::<f64>::measure(&measure, SampleRate::SR_44K1),
        Err(MeterError::ReferenceRate { .. })
    ));
}
