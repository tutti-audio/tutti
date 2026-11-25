use super::*;
use crate::{DitherShape, Quantizer};
use core::f64::consts::PI;
use tutti_buffers::{ChannelLayout, Interleaved, InterleavedMut};
use tutti_units::Amplitude;

/// One i16 step in the float domain.
const LSB: f64 = 1.0 / 32768.0;

/// The workspace's dither seed.
const SEED: u64 = 0x1234_5678;

fn rate(hz: f64) -> SampleRate {
    SampleRate::new(hz).unwrap()
}

/// A sine of amplitude 0.25 at 997 Hz, far from clipping and incommensurate with every block
/// size used here.
fn tone(n: usize, sample_rate: f64) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(997.0),
        Amplitude::new(0.25),
        rate(sample_rate),
        tutti_units::Samples::new(n),
    )
}

/// `x` (mono) quantized to `i16` in writes of `piece` samples, dithered with TPDF from the
/// workspace's seed.
fn quantize(shaping: Option<&NoiseShaping>, x: &[f64], piece: usize) -> Vec<i16> {
    let mut quantizer =
        Quantizer::<f64>::new(Some(DitherShape::Triangular), ChannelLayout::MONO, SEED);
    if let Some(s) = shaping {
        quantizer = quantizer.shaped(s);
    }
    let mut out = vec![0i16; x.len()];
    for (part, out) in x.chunks(piece).zip(out.chunks_mut(piece)) {
        quantizer.quantize(
            Interleaved::new(part, ChannelLayout::MONO).unwrap(),
            InterleavedMut::new(out, ChannelLayout::MONO).unwrap(),
        );
    }
    out
}

/// The error `y - x` of a run, in LSB.
fn error_lsb(shaping: Option<&NoiseShaping>, x: &[f64]) -> Vec<f64> {
    quantize(shaping, x, 4096)
        .iter()
        .zip(x)
        .map(|(&y, &x)| (f64::from(y) * LSB - x) / LSB)
        .collect()
}

/// A Welch estimate of the power spectral density of `x` on bins `0..=SEG/2`: Hann segments of
/// `SEG` at half overlap, each periodogram `|X_k|^2 / sum(w^2)`, averaged. It is scaled so
/// the mean over the bins is the variance of a white `x` (the one-sided bins `0..=SEG/2`
/// stand for the circle).
const SEG: usize = 1024;
fn welch(x: &[f64]) -> Vec<f64> {
    let fft = RealFft::<f64>::new(SEG);
    let mut scratch = fft.new_scratch();
    let w: Vec<f64> = (0..SEG)
        .map(|n| 0.5 - 0.5 * libm::cos(2.0 * PI * n as f64 / SEG as f64))
        .collect();
    let energy: f64 = w.iter().map(|v| v * v).sum();
    let mut psd = vec![0.0; fft.bins()];
    let mut bins = vec![Complex::new(0.0, 0.0); fft.bins()];
    let mut frame = vec![0.0; SEG];
    let mut count = 0.0;
    for seg in x.windows(SEG).step_by(SEG / 2) {
        for ((f, &v), &w) in frame.iter_mut().zip(seg).zip(&w) {
            *f = v * w;
        }
        fft.forward(tutti_buffers::Mono::new(&frame), &mut bins, &mut scratch);
        for (p, b) in psd.iter_mut().zip(&bins) {
            *p += b.norm_sqr() / energy;
        }
        count += 1.0;
    }
    psd.iter().map(|p| p / count).collect()
}

/// `|1 + sum c_k e^(-j k w)|^2` at `w = 2 pi f / rate`.
fn ntf_power(taps: &[f64], hz: f64, sample_rate: f64) -> f64 {
    let w = 2.0 * PI * hz / sample_rate;
    let (mut re, mut im) = (1.0, 0.0);
    for (k, c) in taps.iter().enumerate() {
        let a = w * (k + 1) as f64;
        re += c * libm::cos(a);
        im -= c * libm::sin(a);
    }
    re * re + im * im
}

/// The mean of `ln |NTF|` over `[0, pi]`, by the trapezoid rule on `M` points.
fn mean_ln_ntf(taps: &[f64]) -> f64 {
    const M: usize = 1 << 14;
    let f = |i: usize| 0.5 * libm::log(ntf_power(taps, i as f64 / M as f64 * 0.5, 1.0));
    let inner: f64 = (1..M).map(f).sum();
    (inner + 0.5 * (f(0) + f(M))) / M as f64
}

fn band_means(psd: &[f64], bands: usize) -> Vec<f64> {
    let per = (psd.len() - 1) / bands;
    (0..bands)
        .map(|b| psd[b * per..(b + 1) * per].iter().sum::<f64>() / per as f64)
        .collect()
}

/// Terhardt's threshold in dB at `hz`, held at its 100 Hz and 12 kHz values outside them.
fn terhardt_db(hz: f64) -> f64 {
    let k = hz.clamp(100.0, 12_000.0) / 1000.0;
    3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3) * (k - 3.3)).exp() + 1e-3 * k.powi(4)
}

/// The length of a statistical run.
const N: usize = 1 << 20;

/// The reference for the shaped test below: the unshaped TPDF error has variance `1/6` (the dither) plus `1/12` (the rounding), in LSB^2,
/// and the two are uncorrelated for a triangular dither (Lipshitz, Wannamaker and Vanderkooy,
/// "Quantization and dither: a theory", JAES 1992). The sample variance of `N = 2^20` values
/// of a bounded, platykurtic error (kurtosis under 3) has standard deviation under
/// `sigma^2 sqrt(2 / N)`, and the mean `sigma / sqrt(N)`; the bound is `1.5 sqrt(2 ln N)` of each.
#[test]
fn unshaped_tpdf_error_variance_is_a_sixth_plus_a_twelfth() {
    let e = error_lsb(None, &tone(N, 44_100.0));
    let sigma2 = 1.0 / 6.0 + 1.0 / 12.0;
    let factor = 1.5 * (2.0 * (N as f64).ln()).sqrt();
    let mean = e.iter().sum::<f64>() / N as f64;
    let var = e.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / N as f64;
    assert!(
        mean.abs() < factor * (sigma2 / N as f64).sqrt(),
        "mean {mean}"
    );
    assert!(
        (var - sigma2).abs() < factor * sigma2 * (2.0 / N as f64).sqrt(),
        "variance {var} vs {sigma2}"
    );
}

/// The reference for the shaped test below: the unshaped error is white: each of 16 equal bands of a Welch estimate (1023 half-overlapped
/// segments, 32 bins per band: the relative standard deviation of a band is about
/// `1 / sqrt(segments * bins)` at most, 0.6 %, and the estimate's overlap and window raise it
/// by a factor the bound treats conservatively) is the variance `1/4` to within 5 %.
#[test]
fn unshaped_tpdf_error_spectrum_is_flat() {
    let e = error_lsb(None, &tone(N, 44_100.0));
    for (b, level) in band_means(&welch(&e), 16).iter().enumerate() {
        assert!(
            (level / 0.25 - 1.0).abs() < 0.05,
            "band {b}: {level} vs 1/4"
        );
    }
}

/// The shaped error spectrum is `|NTF|^2` times the unshaped error's `1/4` LSB^2: the error
/// `e` the loop records is white at that variance and `y - x = e * NTF` exactly. Each of 16 equal
/// bands of the Welch estimate matches the band mean of `|NTF|^2 / 4` over the same bins to
/// 5 % (the same seven standard deviations as the flat case, plus the Hann window's leakage
/// of the 25 dB dynamic range, under 0.1 %).
#[test]
fn shaped_error_spectrum_is_the_ntf_power_times_the_unshaped_variance() {
    for sr in [44_100.0, 96_000.0] {
        let shaping = NoiseShaping::new(rate(sr));
        let e = error_lsb(Some(&shaping), &tone(N, sr));
        let measured = band_means(&welch(&e), 16);
        let per = SEG / 2 / 16;
        for (b, got) in measured.iter().enumerate() {
            let want = (b * per..(b + 1) * per)
                .map(|k| ntf_power(shaping.taps(), k as f64 / SEG as f64 * sr, sr) / 4.0)
                .sum::<f64>()
                / per as f64;
            assert!(
                (got / want - 1.0).abs() < 0.05,
                "rate {sr} band {b}: {got} vs {want}"
            );
        }
    }
}

/// The filter is monic and minimum phase, so `integral of ln |NTF| over [0, pi]` is zero
/// (Gerzon and Craven, "Optimal noise shaping and dither of digital signals", AES 87th
/// Convention, 1989). The integral is exactly zero while every zero of the truncated filter
/// stays inside the unit circle, and the trapezoid rule on 2^14 points of the response is
/// exact to well under `1e-9`, so it is below `1e-9` at each rate swept.
#[test]
fn the_ntf_log_magnitude_integrates_to_zero() {
    for sr in [
        8_000.0, 11_025.0, 16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 88_200.0, 96_000.0,
        176_400.0, 192_000.0,
    ] {
        let shaping = NoiseShaping::new(rate(sr));
        let mean = mean_ln_ntf(shaping.taps());
        assert!(mean.abs() < 1e-9, "rate {sr}: {mean}");
    }
}

/// The NTF follows Terhardt's threshold in Hz: its dB response at `f` is the threshold at `f` less
/// the threshold's mean over `[0, rate / 2]` (the monic filter's mean log magnitude is zero),
/// at 44.1 kHz and at 96 kHz alike, within 2.5 dB from 2 kHz to 16 kHz. The tolerance is
/// measured, not derived: the worst deviation seen at these rates is 2.1 dB, at 2 kHz, the fit
/// of a 32-tap truncation (70 taps at 96 kHz) of the target. Below 2 kHz the weighting's slope is steeper than the taps resolve.
#[test]
fn the_ntf_follows_the_threshold_in_hz_at_every_rate() {
    for sr in [44_100.0, 96_000.0] {
        let shaping = NoiseShaping::new(rate(sr));
        let m = 20_000;
        let mean = (1..m)
            .map(|i| terhardt_db(i as f64 / m as f64 * sr / 2.0))
            .sum::<f64>()
            / m as f64;
        for f in [2000.0, 3300.0, 5000.0, 8000.0, 12_000.0, 16_000.0] {
            let got = 10.0 * libm::log10(ntf_power(shaping.taps(), f, sr));
            let want = terhardt_db(f) - mean;
            assert!(
                (got - want).abs() < 2.5,
                "rate {sr} at {f} Hz: {got} dB vs {want} dB"
            );
        }
    }
}

/// `feedback_gain` bounds the shaped output's departure from its input: every error the loop
/// records is within `1.5` LSB (TPDF dither of one LSB plus half an LSB of rounding), so
/// `y - x = e[n] + Σ c_k e[n-k]` stays within `(1 + feedback_gain()) * 1.5` LSB.
#[test]
fn the_feedback_gain_bounds_the_shaped_error() {
    for sr in [44_100.0, 96_000.0] {
        let shaping = NoiseShaping::new(rate(sr));
        let gain = shaping.feedback_gain().get();
        let bound = 1.0 + gain;
        assert!(gain > 1.0, "rate {sr}: it shapes");
        let e = error_lsb(Some(&shaping), &tone(1 << 18, sr));
        let peak = e.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= 1.5 * bound + 1e-9, "rate {sr}: {peak}");
        // The bound is not slack by an order of magnitude: the run gets past the unshaped 1.5.
        assert!(peak > 1.5, "rate {sr}: {peak}");
    }
}
