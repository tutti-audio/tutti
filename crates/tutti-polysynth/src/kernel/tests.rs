use super::*;
use wide::{f32x8, f64x4};

/// The `f32` lanes: the aliasing measurements and the width-specific tolerances run here.
type V = f32x8;

/// `sine` over a cycle against libm, at the lane width `L`, within `tolerance`.
fn sine_against_libm<L: Lane>(tolerance: f64) {
    for i in 0..4096 {
        let p = f64::from(i) / 4096.0;
        let got: f64 = sine(L::splat(L::Scalar::from_design(p))).as_slice()[0].into();
        // The reference folds the phase to the quarter wave first (`sin(pi - a) = sin a`,
        // exact in `f64` for these phases), so its own argument stays under `pi/2` and
        // rounds by under `1.7e-16`, where `2π·p` itself would round by `6e-16`.
        let x = p - 0.5;
        let x = if x > 0.25 {
            0.5 - x
        } else if x < -0.25 {
            -0.5 - x
        } else {
            x
        };
        let want = -(core::f64::consts::TAU * x).sin();
        assert!(
            (got - want).abs() < tolerance,
            "sin(2π·{p}) = {got}, libm {want}"
        );
    }
}

/// The single-precision polynomial stops at the 11th power: its error is the
/// first omitted term, `(π/2)^13 / 13! ≈ 5.7e-8`, plus the rounding of the evaluation
/// (a few ulp at unity, `1.2e-7` each); the bound is `2e-6`.
#[test]
fn sine_at_f32_matches_libm_within_the_polynomial_and_rounding_error() {
    sine_against_libm::<f32x8>(2e-6);
}

/// The double-precision polynomial runs to the 21st power, whose first omitted term is
/// `(π/2)^23 / 23! ≈ 1.2e-18`. The measured worst error is 2.2e-16, the rounding of the
/// evaluation (2 ulp of `f64` at unity, `1.1e-16` each); the bound is `2.5e-16`. A
/// 19th-order polynomial adds its omitted term, `2.6e-16`, at the quarter-wave phases
/// the sweep includes, and fails it.
#[test]
fn sine_at_f64_matches_libm_to_a_few_ulp() {
    sine_against_libm::<f64x4>(2.5e-16);
}

#[test]
fn tanh_is_close_to_libm_and_never_overshoots() {
    let mut worst = 0.0f32;
    for i in -8000..=8000 {
        let x = i as f32 / 1000.0;
        let got = tanh(V::splat(x)).to_array()[0];
        let want = f64::from(x).tanh() as f32;
        worst = worst.max((got - want).abs());
        assert!(got.abs() <= 1.0, "tanh({x}) = {got} overshoots unity");
        if x.abs() < 3.0 {
            assert!((got - want).abs() < 1e-5, "tanh({x}) = {got}, libm {want}");
        }
    }
    assert!(worst < 3e-4, "worst tanh error {worst}");
}

/// The generators of a group seeded `k + 1` in lane order.
fn noise_state<L: Lane>() -> L::Rng {
    L::seed_rng({
        let mut k = 0;
        move || {
            k += 1;
            k
        }
    })
}

/// Segments of the periodogram average and the length of each, a power of two.
const SEGMENTS: usize = 1024;
const SEGMENT: usize = 1024;

/// The Hann-windowed periodogram of lane 0's stream at the bin `k` of a `SEGMENT`-point DFT,
/// averaged over `SEGMENTS` segments.
fn band_power<L: Lane>(state: &mut L::Rng, bins: &[usize]) -> Vec<f64> {
    let hann: Vec<f64> = (0..SEGMENT)
        .map(|n| 0.5 - 0.5 * (core::f64::consts::TAU * n as f64 / SEGMENT as f64).cos())
        .collect();
    let mut power = vec![0.0; bins.len()];
    for _ in 0..SEGMENTS {
        let segment: Vec<f64> = (0..SEGMENT)
            .map(|_| L::noise(state).as_slice()[0].into())
            .collect();
        for (p, &k) in power.iter_mut().zip(bins) {
            let (mut re, mut im) = (0.0, 0.0);
            for (n, (&x, &w)) in segment.iter().zip(&hann).enumerate() {
                let angle = core::f64::consts::TAU * (k * n % SEGMENT) as f64 / SEGMENT as f64;
                re += w * x * angle.cos();
                im -= w * x * angle.sin();
            }
            *p += (re * re + im * im) / SEGMENTS as f64;
        }
    }
    power
}

/// The polysynth's noise oscillator is pink: its power per bin falls 3.01 dB each octave
/// across the band in which `tutti_dsp::Noise` holds the slope (47 Hz to 1.5 kHz at 48 kHz; bins
/// 8, 16 and 32 of a 1024-point DFT are 375 Hz, 750 Hz and 1.5 kHz). Each averaged bin power
/// has a relative standard deviation of `1 / sqrt(SEGMENTS)` (an exponential per segment), a
/// ratio of two bins `sqrt(2 / SEGMENTS)`; the bound is five of them each way, and the 0.1 dB
/// the filter's own response departs from the slope.
fn pink_power_falls_three_db_an_octave<L: Lane>() {
    let mut state = noise_state::<L>();
    let power = band_power::<L>(&mut state, &[8, 16, 32]);
    let deviation = 5.0 * (2.0 / SEGMENTS as f64).sqrt();
    let (high, low) = (
        10.0 * (1.0 + deviation).log10() + 0.1,
        10.0 * (1.0 - deviation).log10() - 0.1,
    );
    for octave in 0..2 {
        let fall = 10.0 * (power[octave] / power[octave + 1]).log10();
        let slope = 10.0 * 2.0_f64.log10();
        assert!(
            fall - slope < high && fall - slope > low,
            "octave {octave} falls {fall} dB, want {slope}"
        );
    }
}

#[test]
fn pink_noise_at_f32_falls_three_db_an_octave() {
    pink_power_falls_three_db_an_octave::<f32x8>();
}

#[test]
fn pink_noise_at_f64_falls_three_db_an_octave() {
    pink_power_falls_three_db_an_octave::<f64x4>();
}

/// Kellet's refined filter as `(pole, input gain)` per one-pole section, the delayed direct term
/// and the direct term, from the published coefficients the pink source is built on, and its
/// output scale.
const KELLET_SECTIONS: [(f64, f64); 6] = [
    (0.99886, 0.055_517_9),
    (0.99332, 0.075_075_9),
    (0.969, 0.153_852),
    (0.8665, 0.310_485_6),
    (0.55, 0.532_952_2),
    (-0.7616, -0.016_898),
];
const KELLET_DIRECT: f64 = 0.115_926 + 0.5362;
const KELLET_SCALE: f64 = 0.11;

/// The noise is zero-mean. It is the white stream (uniform on `[-1, 1)`, variance 1/3) through a
/// filter of DC gain `H(1) = 0.11 (sum g / (1 - a) + direct)`, so the mean of `n` samples has a
/// standard deviation of `sqrt(H(1)^2 / 3 / n)` for `n` far beyond the filter's longest
/// correlation time (`1 / (1 - 0.99886)` = 877 samples); the bound is five of them.
#[test]
fn pink_noise_has_a_mean_of_zero_within_its_standard_error() {
    let dc_gain = KELLET_SCALE
        * (KELLET_SECTIONS
            .iter()
            .map(|&(a, g)| g / (1.0 - a))
            .sum::<f64>()
            + KELLET_DIRECT);
    let mut state = noise_state::<f32x8>();
    let n = 1 << 20;
    let sum: f64 = (0..n)
        .map(|_| f64::from(f32x8::noise(&mut state).as_slice()[0]))
        .sum();
    let mean = sum / n as f64;
    let bound = 5.0 * (dc_gain * dc_gain / 3.0 / n as f64).sqrt();
    assert!(mean.abs() < bound, "mean {mean} over {bound}");
}

/// Sample rate and pitch for the aliasing measurements: 2990 Hz is exactly
/// 299 cycles in 4410 samples, so every harmonic *and every alias* lands on
/// an integer DFT bin — and since 4410 is not a multiple of 299, no alias
/// lands on a harmonic's bin.
const SR: f64 = 44_100.0;
const F0: f64 = 2_990.0;
const N: usize = 4_410;
const CYCLES: usize = 299;

/// Render `N` samples of `osc` at `F0`.
fn render(osc: impl Fn(V, V, V) -> V) -> Vec<f64> {
    let inc = (F0 / SR) as f32;
    let (dt, inv) = (V::splat(inc), V::splat(1.0 / inc));
    let mut phase = V::ZERO;
    (0..N)
        .map(|_| {
            let y = osc(phase, dt, inv).to_array()[0];
            phase = wrap_once(phase + dt);
            f64::from(y)
        })
        .collect()
}

/// Fraction of the signal's AC power that is *not* at a harmonic of `F0` —
/// i.e. aliasing, since a periodic waveform has nothing else.
fn inharmonic_fraction(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean = x.iter().sum::<f64>() / n;
    let total: f64 = x.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let mut harmonic = 0.0;
    let mut k = CYCLES;
    while k < N / 2 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, s) in x.iter().enumerate() {
            let a = core::f64::consts::TAU * (k * i % N) as f64 / n;
            re += s * a.cos();
            im -= s * a.sin();
        }
        harmonic += 2.0 * (re * re + im * im) / (n * n);
        k += CYCLES;
    }
    (total - harmonic).max(0.0) / total
}

fn naive_saw(p: V, _: V, _: V) -> V {
    p + p - V::ONE
}

fn naive_pulse(p: V, _: V, _: V) -> V {
    p.simd_lt(V::splat(0.5)).select(V::ONE, -V::ONE)
}

fn naive_triangle(p: V, _: V, _: V) -> V {
    let q = wrap_once(p + V::splat(0.25));
    V::ONE - V::splat(4.0) * (q - V::splat(0.5)).abs()
}

fn square(p: V, dt: V, inv: V) -> V {
    pulse(p, V::splat(0.5), dt, inv)
}

/// The band-limited waveforms must alias far less than their naive
/// counterparts at a pitch high enough for aliasing to matter.
///
/// Measured at 2990 Hz / 44.1 kHz, where the naive saw folds 8% of its AC
/// power back as inharmonic partials — an audible rasp. Measured margins:
/// saw 15.8 dB, pulse 18.3 dB, triangle 15.1 dB (its naive aliasing is
/// already low, since its harmonics fall at 12 dB/octave).
#[test]
fn band_limited_oscillators_alias_far_less_than_naive() {
    type Osc = fn(V, V, V) -> V;
    let cases: [(&str, Osc, Osc, f64); 3] = [
        ("saw", saw, naive_saw, 12.0),
        ("pulse", square, naive_pulse, 12.0),
        ("triangle", triangle, naive_triangle, 12.0),
    ];
    for (name, bl, naive, min_db) in cases {
        let a_bl = inharmonic_fraction(&render(bl));
        let a_naive = inharmonic_fraction(&render(naive));
        let improvement_db = 10.0 * (a_naive / a_bl).log10();
        assert!(
            improvement_db > min_db,
            "{name}: band-limited aliasing {a_bl:.2e} vs naive {a_naive:.2e} — \
             only {improvement_db:.1} dB better, want > {min_db} dB"
        );
    }
}

/// The band-limited waveforms keep the level of the naive ones: the
/// correction only touches the samples next to a corner or edge.
#[test]
fn band_limited_oscillators_keep_the_naive_level() {
    type Osc = fn(V, V, V) -> V;
    let cases: [(&str, Osc, Osc); 3] = [
        ("saw", saw, naive_saw),
        ("pulse", square, naive_pulse),
        ("triangle", triangle, naive_triangle),
    ];
    let rms = |x: &[f64]| (x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64).sqrt();
    for (name, bl, naive) in cases {
        let (a, b) = (rms(&render(bl)), rms(&render(naive)));
        assert!(
            (a / b - 1.0).abs() < 0.1,
            "{name}: band-limited RMS {a:.3} vs naive {b:.3}"
        );
    }
}
