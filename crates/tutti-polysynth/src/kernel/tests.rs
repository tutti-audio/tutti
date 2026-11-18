use super::*;
use wide::{f32x8, f64x4, u32x4, u32x8};

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

/// White noise on `-1..1` is uniform: mean 0 and variance 1/3. The mean of `n` samples
/// has standard deviation `sqrt(1/3 / n)`; the bound is five of them.
fn white_noise_is_uniform_and_centred<L: Lane>(mut state: L::Rng) {
    let (mut sum, mut sum_sq, mut n) = (0.0f64, 0.0f64, 0usize);
    for _ in 0..10_000 {
        for &s in L::white(&mut state).as_slice() {
            let s: f64 = s.into();
            assert!((-1.0..1.0).contains(&s), "white sample {s} out of range");
            sum += s;
            sum_sq += s * s;
            n += 1;
        }
    }
    let n = n as f64;
    let bound = 5.0 * (1.0 / 3.0 / n).sqrt();
    assert!((sum / n).abs() < bound, "white noise mean {}", sum / n);
    // The variance of `n` squares of a uniform is `4/45 / n`: five standard deviations.
    let var_bound = 5.0 * (4.0 / 45.0 / n).sqrt();
    assert!(
        (sum_sq / n - 1.0 / 3.0).abs() < var_bound,
        "white noise power {}",
        sum_sq / n
    );
}

#[test]
fn white_noise_at_f32_is_in_range_uniform_and_centred() {
    white_noise_is_uniform_and_centred::<f32x8>(u32x8::new(core::array::from_fn(|k| {
        0x9E37_79B9 ^ (k as u32 + 1)
    })));
}

#[test]
fn white_noise_at_f64_is_in_range_uniform_and_centred() {
    white_noise_is_uniform_and_centred::<f64x4>(u32x4::new(core::array::from_fn(|k| {
        0x9E37_79B9 ^ (k as u32 + 1)
    })));
}

/// Every lane of the eight-lane generator steps as the scalar generator does
/// from its seed: the state after each call is the scalar's next word, and the
/// output is that word's top 23 bits as the mantissa of a float in 1..2, mapped
/// onto -1..1 by `2m - 3`.
#[test]
fn each_noise_lane_is_the_scalar_xorshift32() {
    let seeds: [u32; 8] = core::array::from_fn(|k| 0x9E37_79B9 ^ (k as u32 + 1));
    let mut state = u32x8::new(seeds);
    let mut scalar =
        seeds.map(|s| tutti_dsp::Xorshift32::new(core::num::NonZeroU32::new(s).unwrap()));
    for _ in 0..64 {
        let out = V::white(&mut state).to_array();
        for (k, g) in scalar.iter_mut().enumerate() {
            let word = g.next_u32();
            assert_eq!(state.to_array()[k], word, "lane {k} state");
            let mantissa = f32::from_bits((word >> 9) | 0x3F80_0000);
            assert_eq!(out[k], 2.0 * mantissa - 3.0, "lane {k} output");
        }
    }
}

/// The same at `f64`: the output is the word's top 31 bits over `2^30`, less 1.
#[test]
fn each_f64_noise_lane_is_the_scalar_xorshift32() {
    let seeds: [u32; 4] = core::array::from_fn(|k| 0x9E37_79B9 ^ (k as u32 + 1));
    let mut state = u32x4::new(seeds);
    let mut scalar =
        seeds.map(|s| tutti_dsp::Xorshift32::new(core::num::NonZeroU32::new(s).unwrap()));
    for _ in 0..64 {
        let out = f64x4::white(&mut state).to_array();
        for (k, g) in scalar.iter_mut().enumerate() {
            let word = g.next_u32();
            assert_eq!(state.to_array()[k], word, "lane {k} state");
            assert_eq!(
                out[k],
                f64::from(word >> 1) / f64::from(1_u32 << 30) - 1.0,
                "lane {k} output"
            );
        }
    }
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
