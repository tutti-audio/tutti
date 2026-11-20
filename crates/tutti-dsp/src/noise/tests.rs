use super::*;
use crate::{Complex64, RealFft};
use tutti_buffers::Mono;

/// The length of one Welch segment, a power of two.
const SEGMENT: usize = 1 << 14;
/// The segments averaged: 512 of them, 8.4 million samples.
const SEGMENTS: usize = 512;
/// The standard deviations a tolerance spans.
const SIGMAS: f64 = 5.0;

/// The Hann-windowed periodogram of `SEGMENTS` consecutive segments, averaged per bin and
/// normalised by the window's energy, so a white stream of variance `v` reads `v` in every
/// interior bin.
fn welch(noise: &mut Noise<f64>) -> Vec<f64> {
    let fft = RealFft::<f64>::new(SEGMENT);
    let mut scratch = fft.new_scratch();
    let mut bins = vec![Complex64::default(); fft.bins()];
    let window: Vec<f64> = (0..SEGMENT)
        .map(|n| 0.5 - 0.5 * libm::cos(core::f64::consts::TAU * n as f64 / SEGMENT as f64))
        .collect();
    let energy: f64 = window.iter().map(|w| w * w).sum();
    let mut psd = vec![0.0; fft.bins()];
    let mut x = vec![0.0; SEGMENT];
    for _ in 0..SEGMENTS {
        noise.process(MonoMut::new(&mut x));
        let windowed: Vec<f64> = x.iter().zip(&window).map(|(v, w)| v * w).collect();
        fft.forward(Mono::new(&windowed), &mut bins, &mut scratch);
        for (p, b) in psd.iter_mut().zip(&bins) {
            *p += (b.re * b.re + b.im * b.im) / energy;
        }
    }
    psd.iter().map(|p| p / SEGMENTS as f64).collect()
}

/// The mean of `psd` over the bins `lo..hi`.
fn band(psd: &[f64], lo: usize, hi: usize) -> f64 {
    psd[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
}

/// The standard deviation, relative, of a band mean of `bins` bins over `SEGMENTS` segments.
///
/// Each periodogram bin is exponentially distributed about its expectation (a chi-squared of
/// two degrees of freedom), a relative standard deviation of 1 over each segment. A band mean
/// over a spectrum that varies as a power law across the octave is at most 1.2 times the
/// variance of a flat one (`1.04` for `1/f`, `1.17` for `1/f^2`). The Hann window correlates
/// the power of bins 1 and 2 apart by `(2/3)^2` and `(1/6)^2`, which inflates the variance of a
/// sum of bins by `1 + 2 (4/9) + 2 (1/36) < 2`.
fn relative_deviation(bins: usize) -> f64 {
    libm::sqrt(1.2 * 2.0 / (bins * SEGMENTS) as f64)
}

/// A tolerance in dB for the ratio of two band means of `bins` bins each, `SIGMAS` deviations
/// wide: the ratio's relative deviation is the two bands' in quadrature.
fn ratio_tolerance_db(bins: usize) -> f64 {
    let relative = SIGMAS * libm::sqrt(2.0) * relative_deviation(bins);
    10.0 * libm::log10(1.0 + relative)
}

fn db(ratio: f64) -> f64 {
    10.0 * libm::log10(ratio)
}

/// White noise is uniform in `[-1, 1)`, so its mean over `N` samples has a standard deviation
/// of `sqrt(1 / (3 N))`; the mean over 2^20 samples is within `SIGMAS` of 0.
#[test]
fn white_has_a_mean_of_zero_within_its_standard_error() {
    let n = 1 << 20;
    let mut noise = Noise::<f64>::new(NoiseColour::White, 1);
    let mean = (0..n).map(|_| noise.next_sample()).sum::<f64>() / n as f64;
    let bound = SIGMAS * libm::sqrt(1.0 / (3.0 * n as f64));
    assert!(mean.abs() < bound, "{mean} against {bound}");
}

/// White noise is bounded by `[-1, 1)`.
#[test]
fn white_stays_in_the_open_unit_interval() {
    let mut noise = Noise::<f32>::new(NoiseColour::White, 2);
    for _ in 0..100_000 {
        let w = noise.next_sample();
        assert!((-1.0..1.0).contains(&w), "{w}");
    }
}

/// The band power of white noise is its variance, 1/3, in every eighth of the spectrum.
#[test]
fn white_band_power_is_flat_at_one_third() {
    let psd = welch(&mut Noise::new(NoiseColour::White, 3));
    let bins = SEGMENT / 2 / 8;
    let bound = SIGMAS * relative_deviation(bins) / 3.0;
    for k in 0..8 {
        let p = band(&psd, k * bins + 1, (k + 1) * bins);
        assert!(
            (p - 1.0 / 3.0).abs() < bound,
            "band {k}: {p} against {bound}"
        );
    }
}

/// The ratio of the power in adjacent octaves between bins `first` and `last` of the
/// periodogram of `noise`, in dB, with the tolerance the run's length allows.
fn octave_slopes(noise: &mut Noise<f64>, first: usize, last: usize) -> (Vec<f64>, f64) {
    let psd = welch(noise);
    let mut edges = vec![first];
    while *edges.last().unwrap() < last {
        edges.push(edges.last().unwrap() * 2);
    }
    let powers: Vec<f64> = edges.windows(2).map(|w| band(&psd, w[0], w[1])).collect();
    let slopes = powers.windows(2).map(|p| db(p[1] / p[0])).collect();
    (slopes, ratio_tolerance_db(first))
}

/// Pink noise's power per bin falls by `10 log10(2)` = 3.01 dB each octave from bin 32 (94 Hz
/// at 48 kHz) to bin 512 (1.5 kHz). The tolerance is the run's statistical bound for the
/// narrowest band plus 0.1 dB, the most the filter's own transfer function departs from the
/// slope over these octaves (0.09 dB at the worst, evaluated at the bins); above 3 kHz it
/// departs by more, by 0.27 dB in the octave to 3 kHz and 1.8 dB in the last, which this
/// test does not claim.
#[test]
fn pink_power_falls_three_db_an_octave() {
    let (slopes, tolerance) = octave_slopes(&mut Noise::new(NoiseColour::Pink, 4), 32, 512);
    assert_eq!(slopes.len(), 3);
    for (k, s) in slopes.iter().enumerate() {
        assert!(
            (s + db(2.0)).abs() < tolerance + 0.1,
            "octave {k}: {s} dB, tolerance {tolerance}"
        );
    }
}

/// Brown noise's power per bin falls by `10 log10(4)` = 6.02 dB each octave from bin 64 (188 Hz)
/// to bin 1024 (3 kHz). Beside the statistical bound, the integrator departs from `1/w^2` by
/// its leak, a relative `(2^-9)^2 / w^2` at the lowest edge (`w = 2 pi 64 / 16384`) and by its
/// sampling, a relative `w^2 / 12` at the highest (`w = 2 pi 1024 / 16384`); the bound adds
/// both.
#[test]
fn brown_power_falls_six_db_an_octave() {
    let (slopes, tolerance) = octave_slopes(&mut Noise::new(NoiseColour::Brown, 5), 64, 1024);
    assert_eq!(slopes.len(), 3);
    let w = |bin: f64| core::f64::consts::TAU * bin / SEGMENT as f64;
    let leak = (1.0 / 512.0f64).powi(2) / w(64.0).powi(2);
    let sampling = w(1024.0).powi(2) / 12.0;
    let bend = db(1.0 + leak + sampling);
    for (k, s) in slopes.iter().enumerate() {
        assert!(
            (s + db(4.0)).abs() < tolerance + bend,
            "octave {k}: {s} dB, tolerance {tolerance} + {bend}"
        );
    }
}

/// The leak bounds the integrator: `|y| <= 0.02 / 2^-9` for white of magnitude at most 1, so a
/// long run never leaves `[-10.24, 10.24]`, and its mean is within `SIGMAS` standard errors of
/// 0. The mean's standard error uses the integrator's correlation time `1 / (1 - a)` = 512
/// samples: an effective count of `N / (2 * 512)`, over the stationary variance
/// `0.02^2 / 3 / (1 - a^2)`.
#[test]
fn brown_is_bounded_and_does_not_drift() {
    let n = 1 << 22;
    let mut noise = Noise::<f64>::new(NoiseColour::Brown, 6);
    let (mut sum, mut peak) = (0.0f64, 0.0f64);
    for _ in 0..n {
        let y = noise.next_sample();
        sum += y;
        peak = peak.max(y.abs());
    }
    assert!(peak < 10.24, "{peak}");
    let a = 1.0 - 1.0 / 512.0;
    let variance = 0.02 * 0.02 / 3.0 / (1.0 - a * a);
    let bound = SIGMAS * libm::sqrt(variance * 2.0 * 512.0 / n as f64);
    assert!(
        (sum / n as f64).abs() < bound,
        "{} against {bound}",
        sum / n as f64
    );
}

/// The same seed is the same stream, and `reset` replays it from the start; another seed is
/// another stream.
#[test]
fn the_same_seed_gives_the_same_samples() {
    for colour in [NoiseColour::White, NoiseColour::Pink, NoiseColour::Brown] {
        let take =
            |noise: &mut Noise<f32>| (0..256).map(|_| noise.next_sample()).collect::<Vec<_>>();
        let mut a = Noise::<f32>::new(colour, 77);
        let first = take(&mut a);
        assert_eq!(first, take(&mut Noise::<f32>::new(colour, 77)));
        a.reset();
        assert_eq!(first, take(&mut a));
        assert_ne!(first, take(&mut Noise::<f32>::new(colour, 78)));
    }
}
