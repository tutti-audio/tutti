//! The signals a test feeds a node or checks one against, defined once: a sine,
//! an impulse, the tone readings [`lock_in`] and [`amplitude_at`], the level
//! readings [`peak`], [`peak_planes`], [`rms`] and [`rms_f64`], the bit patterns
//! of a render ([`bits`]), the impulse responses [`decay_ir`] and [`room_ir`],
//! and a float WAV on disk ([`write_wav_f32`], [`read_wav_f32`]), and the
//! narrowing check [`wide_signal`] with [`assert_close_f64`].
//!
//! A stretched step edge's smear is [`stretched_edge_spread`].
//!
//! Noise is [`noise`](crate::noise) and a bit hash is [`Fnv`](crate::Fnv), both in
//! this crate's root. The figures are plain `f32` samples, except the `f64` narrowing
//! check's; only the arguments that name a quantity are units.
//!
//! These are definitions, not references: a test pins an analytic figure with
//! them (a sine's amplitude, an impulse's position), never a render against
//! another render.

use std::path::Path;
use tutti_buffers::Sample;
use tutti_units::{Amplitude, Hz, SampleRate, Samples, Seconds, StretchFactor};

/// `len` samples of a sine: `amplitude * sin(2 pi * hz * i / rate)` for sample
/// `i`, with phase 0 at sample 0 (the first sample is exactly `0.0`).
///
/// The phase is `step * i` in `f64` (no running sum) and the result narrowed once to
/// the `f32` sample, so the figure carries no error that grows with `i`.
pub fn sine(hz: Hz, amplitude: Amplitude, rate: SampleRate, len: Samples) -> Vec<f32> {
    let step = std::f64::consts::TAU * hz.get() / rate.get();
    let amp = amplitude.get();
    (0..len.get())
        .map(|i| (amp * (step * i as f64).sin()) as f32)
        .collect()
}

/// The complex amplitude of the `hz` component of `x`, as `(re, im)` of
/// `2/N * sum(x[n] * e^(-j w n))` (a unit sine at phase 0 reads `(0, -1)`).
///
/// Exact (no leakage) when `x` spans a whole number of periods of `hz`.
pub fn lock_in<S: Sample>(x: &[S], hz: Hz, rate: SampleRate) -> (f64, f64) {
    let w = std::f64::consts::TAU * hz.get() / rate.get();
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let a = w * n as f64;
        re += v.to_f64() * libm::cos(a);
        im -= v.to_f64() * libm::sin(a);
    }
    let k = 2.0 / x.len() as f64;
    (re * k, im * k)
}

/// The amplitude of [`lock_in`].
pub fn amplitude_at<S: Sample>(x: &[S], hz: Hz, rate: SampleRate) -> f64 {
    let (re, im) = lock_in(x, hz, rate);
    re.hypot(im)
}

/// `len` samples of silence but for a single `1.0` at sample 0.
///
/// An empty `len` gives an empty vector.
pub fn impulse(len: Samples) -> Vec<f32> {
    impulse_at(len, Samples::new(0))
}

/// `len` samples of silence but for a single `1.0` at sample `at`.
///
/// # Panics
///
/// If `at` is not inside `len`.
pub fn impulse_at(len: Samples, at: Samples) -> Vec<f32> {
    let mut x = vec![0.0; len.get()];
    if len.get() > 0 || at.get() > 0 {
        x[at.get()] = 1.0;
    }
    x
}

/// The largest absolute sample of `x` (`0.0` for an empty slice).
///
/// A `NaN` sample is skipped, as `f32::max` skips it.
pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// The largest absolute sample over every plane of `planes` (`0.0` when there are none).
///
/// A `NaN` sample is skipped, as in [`peak`].
pub fn peak_planes<P: AsRef<[f32]>>(planes: &[P]) -> f32 {
    planes.iter().fold(0.0, |m, p| m.max(peak(p.as_ref())))
}

/// The root mean square of **every** sample of `x`: the square root of the
/// mean of the squares, summed in `f64` and narrowed once. `0.0` for an empty
/// slice.
///
/// The denominator is `x.len()`, so a stereo reading is taken over each plane
/// on its own (or over an interleaved slice as one signal), never over frames.
/// [`rms_f64`] is the same figure before the narrowing.
pub fn rms(x: &[f32]) -> f32 {
    rms_f64(x) as f32
}

/// [`rms`] without the final narrowing to `f32`. An empty slice reads `0.0`.
pub fn rms_f64(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f64 = x.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / x.len() as f64).sqrt()
}

/// The IEEE bit pattern of each sample of `x`: what a bit-exact comparison
/// reads, so that `0.0` and `-0.0` differ and every `NaN` payload is its own.
pub fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|s| s.to_bits()).collect()
}

/// [`bits`] of each plane of `planes`.
pub fn plane_bits(planes: &[Vec<f32>]) -> Vec<Vec<u32>> {
    planes.iter().map(|p| bits(p)).collect()
}

/// Writes `interleaved` (frames of `channels` samples each) to `path` as a
/// 32-bit float WAV at `rate`.
///
/// # Panics
///
/// If the file cannot be written, or `rate` is not a whole number of hertz.
pub fn write_wav_f32(path: &Path, rate: SampleRate, channels: u16, interleaved: &[f32]) {
    assert!(rate.get().fract() == 0.0, "a WAV rate is whole hertz");
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate.get() as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create the wav");
    for &s in interleaved {
        w.write_sample(s).expect("write the wav");
    }
    w.finalize().expect("finalize the wav");
}

/// Reads a 32-bit float WAV from `path` into one plane per channel.
///
/// # Panics
///
/// If the file is not a readable 32-bit float WAV.
pub fn read_wav_f32(path: &Path) -> Vec<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).expect("the file is a WAV");
    let channels = usize::from(reader.spec().channels);
    let samples: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.expect("a float sample"))
        .collect();
    (0..channels)
        .map(|c| samples.iter().skip(c).step_by(channels).copied().collect())
        .collect()
}

/// An impulse response of `len` taps decaying exponentially: `exp(-t / decay_time)` at tap
/// `t`, sampled at `rate`.
#[must_use]
pub fn decay_ir(
    len: usize,
    decay_time: impl Into<Seconds>,
    rate: impl Into<SampleRate>,
) -> Vec<f32> {
    let (decay_time, rate) = (decay_time.into(), rate.into().get());
    // One coefficient: computed exactly, rounded once to the `f32` the loop runs in.
    let decay_rate = (-1.0 / (decay_time.get() * rate)) as f32;
    (0..len).map(|i| (decay_rate * i as f32).exp()).collect()
}

/// A synthetic room impulse response of `1.5 * decay_time` seconds at `rate`: a unit
/// impulse, six early reflections at delays scaled by `room_size` and a noisy exponential
/// tail from 50 ms on.
#[must_use]
pub fn room_ir(
    room_size: f32,
    decay_time: impl Into<Seconds>,
    rate: impl Into<SampleRate>,
) -> Vec<f32> {
    let (decay_time, rate) = (decay_time.into(), rate.into().get());
    let mut out = vec![0.0f32; (decay_time.get() * rate * 1.5) as usize];
    let Some(first) = out.first_mut() else {
        return out;
    };
    *first = 1.0;
    let reflections = [
        (0.010 * room_size, 0.7_f32),
        (0.015 * room_size, 0.5),
        (0.023 * room_size, 0.4),
        (0.031 * room_size, 0.3),
        (0.041 * room_size, 0.25),
        (0.053 * room_size, 0.2),
    ];
    for (delay_factor, amplitude) in reflections {
        let idx = (delay_factor * rate as f32) as usize;
        if let Some(tap) = out.get_mut(idx) {
            *tap = amplitude;
        }
    }
    let tail_start = (0.05 * rate as f32) as usize;
    // One coefficient: computed exactly, rounded once to the `f32` the loop runs in.
    let decay_rate = (-1.0 / (decay_time.get() * rate)) as f32;
    let mut rng = 12345u32;
    for (offset, slot) in out.iter_mut().skip(tail_start).enumerate() {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let noise = ((rng >> 16) as f32 / 32768.0) - 1.0;
        *slot += noise * (decay_rate * offset as f32).exp() * 0.1;
    }
    out
}

/// `frames` of an `f64` signal that needs more than 24 significant bits: frame `k` is
/// `(0x2000_0001 + 3k) / 2^31`, an odd 29-bit integer over a power of two, so an `f32`
/// cannot hold it and a processor that narrows to `f32` anywhere returns a different
/// value.
pub fn wide_signal(frames: usize) -> Vec<f64> {
    const BASE: i64 = 0x2000_0001;
    const SCALE: f64 = 2_147_483_648.0;
    (0..frames as i64)
        .map(|k| (BASE + 3 * k) as f64 / SCALE)
        .collect()
}

/// The Hann-windowed DFT of `x` at exactly `at`, as `(re, im)`: the sum over `n` of
/// `w[n] x[n] e^{-j 2 pi at n / rate}`, with `w` the periodic Hann window of `x.len()` points.
pub fn hann_dft(x: &[f64], at: Hz, rate: SampleRate) -> (f64, f64) {
    let m = x.len() as f64;
    let step = -std::f64::consts::TAU * at.get() / rate.get();
    x.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &v)| {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / m).cos();
        let a = step * n as f64;
        (re + v * w * a.cos(), im + v * w * a.sin())
    })
}

/// The frequency of the sine in `x`, to within `measure` samples of it: the phase advance of
/// its exact-bin DFT ([`hann_dft`]) at `near` between the segments of `measure` samples at 0
/// and at `step`, read as a deviation from `near` (unambiguous within `rate / (2 step)` of it).
///
/// The estimator's only systematic error is the window's leakage of the tone's
/// negative-frequency image ([`hann_image_leak`]), a phase error of at most that many radians
/// in each segment.
///
/// # Panics
///
/// If `x` is shorter than `step + measure`.
pub fn sine_frequency(
    x: &[f64],
    near: Hz,
    rate: SampleRate,
    measure: Samples,
    step: Samples,
) -> Hz {
    let (measure, step) = (measure.get(), step.get());
    let (a, b) = (&x[..measure], &x[step..step + measure]);
    let (ar, ai) = hann_dft(a, near, rate);
    let (br, bi) = hann_dft(b, near, rate);
    // The phase of `b * conj(a)`.
    let advance = (bi * ar - br * ai).atan2(br * ar + bi * ai);
    let expected = std::f64::consts::TAU * near.get() * step as f64 / rate.get();
    let deviation = (advance - expected + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
        - std::f64::consts::PI;
    Hz::new(near.get() + deviation * rate.get() / (std::f64::consts::TAU * step as f64))
}

/// The leakage of a Hann window of `length` samples at the negative-frequency image of a tone
/// at `f`, `2 f` away, relative to the window's gain at the tone: the closed form
/// `|sin(pi v) / (pi v (1 - v^2))|` at `v` bins, bounded by `1 / (pi v (v^2 - 1))`, which is
/// returned.
pub fn hann_image_leak(f: Hz, length: Samples, rate: SampleRate) -> f64 {
    let v = 2.0 * f.get() * length.get() as f64 / rate.get();
    1.0 / (std::f64::consts::PI * v * (v * v - 1.0))
}

/// Asserts `got` and `want` have one length and agree to `tolerance` at every frame; the
/// failure names the first frame that does not (a NaN never agrees), with both values.
///
/// # Panics
///
/// If the lengths differ or a frame is farther than `tolerance`.
#[track_caller]
pub fn assert_close_f64(got: &[f64], want: &[f64], tolerance: f64) {
    assert_eq!(got.len(), want.len(), "block lengths differ");
    if let Some(i) = (0..got.len()).find(|&i| {
        let diff = (got[i] - want[i]).abs();
        diff.is_nan() || diff > tolerance
    }) {
        panic!(
            "frame {i}: got {} want {} (|diff| {:e} over tolerance {tolerance:e})",
            got[i],
            want[i],
            (got[i] - want[i]).abs()
        );
    }
}

/// How far from `edge x stretch` the half-level frame of a step edge through a phase
/// vocoder can sit: the spread of the edge's smear about its centroid, for a vocoder on a Hann
/// window of `window` frames stretching by `stretch` with a read aligned to the stretch (see
/// the sampler's seat). Add the resolution of the level reading.
///
/// With `c = 1 / stretch`, analysis frame `f` starts at input `f hop c` and its content at
/// output `t` is the input at `f hop c + t - f hop`, so the frame holds an edge at input `e`
/// from output `T_f = e + f hop (1 - c)` on, and the frames covering `t` are weighted by the
/// window energy `w(t - f hop)^2` the overlap-add divides by. At offset `i = t - f hop` in the
/// frame, `T_f = e + (t - i)(1 - c)`: the edge's smear is the window's energy distribution
/// over `i`, scaled by `|1 - c|`. The aligned read puts its centroid at `edge x stretch`
/// (the offsets average to the window's middle), the level at `t` is the energy held by the
/// frames whose `T_f <= t`, so its half-level frame is the smear's median, and a median is
/// within one standard deviation of the mean. The bound is that deviation,
/// `|1 - c| sigma`, `sigma` the standard deviation of `i` under `w(i)^2` (about
/// `0.146 window`), 100 frames at a stretch of 1.5 on a 2048 window, and zero at unity.
#[must_use]
pub fn stretched_edge_spread(stretch: StretchFactor, window: Samples) -> Samples {
    let w = window.get();
    let energy =
        |i: usize| (0.5 - 0.5 * (core::f64::consts::TAU * i as f64 / w as f64).cos()).powi(2);
    let total: f64 = (0..w).map(energy).sum();
    let mean: f64 = (0..w).map(|i| energy(i) * i as f64).sum::<f64>() / total;
    let variance: f64 = (0..w)
        .map(|i| energy(i) * (i as f64 - mean).powi(2))
        .sum::<f64>()
        / total;
    let c = 1.0 / stretch.get();
    Samples::new((variance.sqrt() * (1.0 - c).abs()).ceil() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at a frequency off every bin is found to within the image leakage of its two
    /// segments, by the closed-form bound.
    #[test]
    fn sine_frequency_finds_the_sine_within_the_image_leakage() {
        let rate = SampleRate::SR_48K;
        let (f, measure, step) = (1234.5, Samples::new(16_384), Samples::new(4096));
        let x: Vec<f64> = sine(Hz::new(f), Amplitude::new(0.5), rate, Samples::new(30_000))
            .iter()
            .map(|&v| f64::from(v))
            .collect();
        let found = sine_frequency(&x, Hz::new(1240.0), rate, measure, step).get();
        let bound = 2.0 * hann_image_leak(Hz::new(f), measure, rate) * rate.get()
            / (std::f64::consts::TAU * step.get() as f64);
        // The `f32` samples' rounding adds `2^-24` of phase per segment.
        let rounding = 2.0 * 2f64.powi(-24) * rate.get() / (std::f64::consts::TAU * 4096.0);
        assert!((found - f).abs() < bound + rounding, "{found} for {f}");
    }

    /// A sine starts at 0, peaks at the amplitude a quarter period in, and its
    /// RMS is the amplitude over root two across whole periods.
    #[test]
    fn a_sine_is_the_analytic_one() {
        // 12 kHz at 48 kHz: four samples a period, 0, A, 0, -A.
        let x = sine(
            Hz::new(12_000.0),
            Amplitude::new(0.5),
            SampleRate::SR_48K,
            Samples::new(8),
        );
        assert_eq!(x[0], 0.0);
        assert!((x[1] - 0.5).abs() < 1e-6 && (x[3] + 0.5).abs() < 1e-6);
        let y = sine(
            Hz::new(1_000.0),
            Amplitude::new(0.5),
            SampleRate::SR_48K,
            Samples::new(48_000),
        );
        assert!((rms(&y) - 0.5 / 2f32.sqrt()).abs() < 1e-6);
        assert!((peak(&y) - 0.5).abs() < 1e-6);
    }

    /// An impulse is one `1.0` at the asked frame and nothing else.
    #[test]
    fn an_impulse_is_one_sample() {
        assert_eq!(impulse(Samples::new(4)), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            impulse_at(Samples::new(4), Samples::new(2)),
            [0.0, 0.0, 1.0, 0.0]
        );
        assert!(impulse(Samples::new(0)).is_empty());
    }

    /// The readings of a known buffer, the empty one included.
    #[test]
    fn the_levels_of_a_known_buffer() {
        assert_eq!(peak(&[0.25, -0.75, 0.5]), 0.75);
        assert_eq!(rms(&[3.0, -3.0, 3.0, -3.0]), 3.0);
        assert_eq!((peak(&[]), rms(&[])), (0.0, 0.0));
    }

    /// The peak over planes is the largest of the planes' peaks, whatever
    /// the container, and nothing at all reads zero.
    #[test]
    fn the_peak_over_planes_is_the_largest_plane_peak() {
        let planes = [vec![0.25, -0.5], vec![0.125, 0.75, -0.25]];
        assert_eq!(peak_planes(&planes), 0.75);
        assert_eq!(peak_planes(&[[0.5f32, -1.0], [0.25, 0.0]]), 1.0);
        assert_eq!(peak_planes::<Vec<f32>>(&[]), 0.0);
    }

    /// A full-scale sine over whole periods reads `1 / sqrt 2`, at `f64`
    /// precision, and `rms` is that figure narrowed once.
    #[test]
    fn a_full_scale_sine_reads_one_over_root_two_in_f64() {
        let x = sine(
            Hz::new(1_000.0),
            Amplitude::new(1.0),
            SampleRate::SR_48K,
            Samples::new(48_000),
        );
        let want = std::f64::consts::FRAC_1_SQRT_2;
        assert!((rms_f64(&x) - want).abs() < 1e-7, "{}", rms_f64(&x));
        assert_eq!(rms(&x), rms_f64(&x) as f32);
        assert_eq!(rms_f64(&[]), 0.0);
    }

    /// Negative zero has its own bits.
    #[test]
    fn bits_tell_the_signed_zeros_apart() {
        assert_ne!(bits(&[0.0]), bits(&[-0.0]));
        assert_eq!(
            plane_bits(&[vec![1.0], vec![2.0]]),
            [[1.0f32.to_bits()], [2.0f32.to_bits()]]
        );
    }

    /// A WAV written is the WAV read back, plane by plane.
    #[test]
    fn a_wav_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav_f32(&path, SampleRate::SR_48K, 2, &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(read_wav_f32(&path), [vec![0.1, 0.3], vec![0.2, 0.4]]);
    }
}
