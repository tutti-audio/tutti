use super::*;
use core::f64::consts::TAU;
use tutti_rt::SplitMix64;

const RATE: SampleRate = SampleRate::SR_48K;

/// The round-off of a forward and an inverse FFT of `n` points over values of order one, in
/// `f64`: `16 n` units of the last place.
fn fft_tolerance(n: usize) -> f64 {
    16.0 * n as f64 * f64::EPSILON
}

fn table(frames: &[Vec<f64>]) -> Arc<Wavetable<f64>> {
    let views: Vec<Mono<'_, f64>> = frames.iter().map(|f| Mono::new(f)).collect();
    Wavetable::new(&views).expect("a table")
}

fn uniform(rng: &mut SplitMix64) -> f64 {
    f64::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0
}

/// A frame of `n` samples of white noise on `-1..1`.
fn noise_frame(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..n).map(|_| uniform(&mut rng)).collect()
}

/// A frame of harmonics `1..=10` with random amplitudes and phases.
fn few_harmonics(n: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    let parts: Vec<(f64, f64)> = (0..10)
        .map(|_| (uniform(&mut rng) / 4.0, uniform(&mut rng) * TAU))
        .collect();
    (0..n)
        .map(|i| {
            parts
                .iter()
                .enumerate()
                .map(|(h, &(a, p))| a * libm::sin(TAU * (h + 1) as f64 * i as f64 / n as f64 + p))
                .sum()
        })
        .collect()
}

fn spectrum(x: &[f64]) -> Vec<Complex<f64>> {
    let fft = RealFft::<f64>::new(x.len());
    let mut bins = vec![Complex::new(0.0, 0.0); fft.bins()];
    fft.forward(Mono::new(x), &mut bins, &mut fft.new_scratch());
    bins
}

fn play(table: &Arc<Wavetable<f64>>, hz: f64, position: f64, len: usize) -> Vec<f64> {
    let step = WavetableStep::new(table, Hz::new(hz), RATE, Normalized::of(position));
    let mut osc = WavetableOscillator::new(Arc::clone(table));
    (0..len).map(|_| osc.step(step)).collect()
}

/// **Level `j` is its frame with the DFT bins above `K_j = N / 2^(j+2)` zeroed**: those bins
/// are zero to the FFT's round-off, and the bins up to `K_j` are the frame's own.
#[test]
fn each_level_has_no_energy_above_its_harmonic_limit_and_keeps_the_rest() {
    let n = 256;
    let frame = noise_frame(n, 7);
    let t = table(std::slice::from_ref(&frame));
    assert_eq!(t.levels(), 7);
    let original = spectrum(&frame);
    for level in 0..t.levels() {
        assert_eq!(t.harmonics(level), n >> (level + 2));
        let bins = spectrum(t.level(0, level).samples());
        for (k, (b, o)) in bins.iter().zip(&original).enumerate() {
            let want = if k <= t.harmonics(level) {
                *o
            } else {
                Complex::new(0.0, 0.0)
            };
            assert!(
                (b - want).norm() <= fft_tolerance(n),
                "level {level} bin {k}: {b} against {want}"
            );
        }
    }
    assert_eq!(t.harmonics(t.levels() - 1), 1);
}

/// **A level is alias-free where it is played**: `K_m f <= fs / 2` for the lower level `m` of
/// every frequency, the weight is in `0..=1`, and the weight is the closed form
/// `log2(f / f_{m-1})`: a frequency `sqrt 2` above `f_{m-1}` is half way, level `m + 1`
/// alone from `f_m`, and level 0 alone below `f_0 / 2`.
#[test]
fn a_level_is_never_played_above_its_alias_free_limit() {
    let n = 2048;
    let t = table(&[noise_frame(n, 1)]);
    let limit = |m: usize| RATE.get() * f64::from(1u32 << (m + 1)) / n as f64;
    let mut f = 1.0;
    while f <= RATE.get() / 2.0 {
        let step = WavetableStep::new(&t, Hz::new(f), RATE, Normalized::of(0.0));
        let (m, w) = step.level();
        assert!((0.0..=1.0).contains(&w), "weight at {f} Hz");
        assert!(
            t.harmonics(m) as f64 * f <= RATE.get() / 2.0 * (1.0 + 1e-12),
            "level {m} played at {f} Hz"
        );
        f *= 1.013;
    }
    for m in 1..t.levels() - 1 {
        let (lo, w) = WavetableStep::new(
            &t,
            Hz::new(limit(m - 1) * core::f64::consts::SQRT_2),
            RATE,
            Normalized::of(0.0),
        )
        .level();
        assert_eq!(lo, m);
        assert!((w - 0.5).abs() < 1e-12, "weight {w}");
        let (lo, w) = WavetableStep::new(&t, Hz::new(limit(m)), RATE, Normalized::of(0.0)).level();
        assert_eq!((lo, w), (m, 1.0));
    }
    let (lo, w) =
        WavetableStep::new(&t, Hz::new(limit(0) / 2.0), RATE, Normalized::of(0.0)).level();
    assert_eq!((lo, w), (0, 0.0));
    let top = t.levels() - 1;
    let (lo, w) =
        WavetableStep::new(&t, Hz::new(RATE.get() / 2.0), RATE, Normalized::of(0.0)).level();
    assert_eq!((lo, w), (top, 0.0));
}

/// **A sine table played at `f` is `sin(2 pi f t / fs)`** within the interpolation bound
/// `E(1) = (4/81) w^3 + w^4 / 384` at `w = 2 pi / N`, plus the FFT's round-off, in a pure
/// level, in a crossfade of two levels and in the top level. Linear interpolation is off by
/// `w^2 / 8`, a thousand times the bound.
#[test]
fn a_sine_table_played_at_f_is_a_sine_at_f_within_the_interpolation_bound() {
    let n = 2048;
    let frame: Vec<f64> = (0..n)
        .map(|i| libm::sin(TAU * i as f64 / n as f64))
        .collect();
    let t = table(&[frame]);
    let bound = t.interpolation_error(1) + fft_tolerance(n);
    for hz in [40.0, 997.0, 5_000.0, 17_000.0] {
        let y = play(&t, hz, 0.0, 4_800);
        for (i, &v) in y.iter().enumerate() {
            let exact = libm::sin(TAU * hz * i as f64 / RATE.get());
            assert!(
                (v - exact).abs() <= bound,
                "{hz} Hz, sample {i}: {}",
                v - exact
            );
        }
    }
}

/// **A saw table near Nyquist is its fundamental and nothing above**: at 0.45 of the rate the
/// top level plays, so the output is the table's DC and its first harmonic (from the table's
/// own DFT, `X_0 / N + (2 / N) Re(X_1 e^(2 pi i phi))`) within the interpolation bound, which
/// also bounds every aliased partial. Level 0 puts the saw's harmonics 2 and up on aliases at
/// a few percent of the fundamental.
#[test]
fn a_saw_table_near_nyquist_has_no_partial_beyond_the_interpolation_bound() {
    let n = 2048;
    let saw: Vec<f64> = (0..n).map(|i| 2.0 * i as f64 / n as f64 - 1.0).collect();
    let bins = spectrum(&saw);
    let first = (2.0 / n as f64) * bins[1].norm();
    let t = table(&[saw]);
    let bound = first * t.interpolation_error(1) + fft_tolerance(n);
    let hz = 0.45 * RATE.get();
    let y = play(&t, hz, 0.0, 4_800);
    let mut peak = 0.0f64;
    for (i, &v) in y.iter().enumerate() {
        let phi = hz * i as f64 / RATE.get();
        let (s, c) = libm::sincos(TAU * phi);
        let exact = bins[0].re / n as f64 + (2.0 / n as f64) * (bins[1].re * c - bins[1].im * s);
        assert!((v - exact).abs() <= bound, "sample {i}: {}", v - exact);
        peak = peak.max(v.abs());
    }
    assert!(peak > 0.5, "the fundamental sounds: {peak}");
}

/// **The frame position at a frame's centre plays that frame**, and half way between two it
/// plays their mean: at `fs / N` the phase steps one table sample, level 0 plays alone (a
/// frame of harmonics up to 10 is level 0's own), and each output is the frame's sample.
#[test]
fn the_frame_position_at_a_frame_centre_plays_that_frame_exactly() {
    let n = 256;
    let frames: Vec<Vec<f64>> = (0..3).map(|s| few_harmonics(n, 100 + s)).collect();
    assert!(
        frames[0]
            .iter()
            .zip(&frames[1])
            .any(|(a, b)| (a - b).abs() > 0.1),
        "the frames differ"
    );
    let t = table(&frames);
    let hz = RATE.get() / n as f64;
    let tolerance = fft_tolerance(n);
    for (position, frame) in [(0.0, 0), (0.5, 1), (1.0, 2)] {
        let y = play(&t, hz, position, 2 * n);
        for (i, &v) in y.iter().enumerate() {
            assert!(
                (v - frames[frame][i % n]).abs() <= tolerance,
                "frame {frame}, sample {i}"
            );
        }
    }
    let y = play(&t, hz, 0.25, n);
    for (i, &v) in y.iter().enumerate() {
        let mean = 0.5 * (frames[0][i] + frames[1][i]);
        assert!((v - mean).abs() <= tolerance, "sample {i}");
    }
}

/// **A table is built from frames that are one power-of-two length and finite**, and says
/// which is not.
#[test]
fn a_table_refuses_frames_that_are_not_one_table_size_and_finite() {
    let build = |frames: &[Vec<f64>]| {
        let views: Vec<Mono<'_, f64>> = frames.iter().map(|f| Mono::new(f)).collect();
        Wavetable::new(&views).err()
    };
    assert_eq!(build(&[]), Some(WavetableError::NoFrames));
    assert_eq!(
        build(&[vec![0.0; 100]]),
        Some(WavetableError::Size(TableSizeError(100)))
    );
    assert_eq!(
        build(&[vec![0.0; 8]]),
        Some(WavetableError::Size(TableSizeError(8)))
    );
    assert_eq!(
        build(&[vec![0.0; 64], vec![0.0; 32]]),
        Some(WavetableError::Mismatch { frame: 1 })
    );
    let mut bad = vec![0.0; 64];
    bad[3] = f64::NAN;
    assert_eq!(
        build(&[vec![0.0; 64], bad]),
        Some(WavetableError::NotFinite { frame: 1 })
    );
    assert_eq!(build(&[vec![0.0; 64]]), None);
}
