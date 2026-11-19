use super::*;
use tutti_buffers::ChannelLayout;
use tutti_rt::SplitMix64;

/// `apply` over a mono run of samples.
fn apply<F: Float>(n: &mut DitherNoise, shape: DitherShape, lsb: Amplitude, s: &mut [F]) {
    n.apply(
        shape,
        lsb,
        InterleavedMut::new(s, ChannelLayout::MONO).unwrap(),
    );
}

const LSB: Amplitude = Amplitude::new(1.0);

/// The seed every export starts from.
const SEED: u64 = 0x1234_5678;

/// The `k`th word of the seeded stream, as the `f64` fraction `word >> 11` over `2^53`.
fn seeded_fraction(k: usize) -> f64 {
    let mut rng = SplitMix64::new(SEED);
    let word = (0..=k).map(|_| rng.next_u64()).last().unwrap();
    (word >> 11) as f64 / (1u64 << 53) as f64
}

/// The first rectangular draw is the first word from the seed, scaled to `[-0.5, 0.5)`:
/// `word / 2^64 - 0.5` (to the `f32` grid of `2^-24`).
#[test]
fn the_first_draw_is_the_seeded_word_scaled() {
    let mut s = [0.0f32; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Rectangular,
        LSB,
        &mut s,
    );
    let want = seeded_fraction(0) - 0.5;
    assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
}

/// The first triangular sample is `(r1 - r2) * lsb` with `r1`, `r2` the
/// first two seeded words over `2^64`.
#[test]
fn the_first_triangular_draw_is_the_difference_of_two_seeded_words() {
    let mut s = [0.0f32; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Triangular,
        Amplitude::new(0.25),
        &mut s,
    );
    let want = (seeded_fraction(0) - seeded_fraction(1)) * 0.25;
    assert!((f64::from(s[0]) - want).abs() < 1e-7, "{} vs {want}", s[0]);
}

#[test]
fn rectangular_stays_within_half_an_lsb() {
    let mut n = DitherNoise::new();
    let mut s = [0.0f32; 4096];
    apply(&mut n, DitherShape::Rectangular, LSB, &mut s);
    assert!(s.iter().all(|x| x.abs() <= 0.5));
    assert!(s.iter().any(|x| x.abs() > 0.4), "the range is used");
}

#[test]
fn triangular_stays_within_one_lsb() {
    let mut n = DitherNoise::new();
    let mut s = [0.0f32; 4096];
    apply(&mut n, DitherShape::Triangular, LSB, &mut s);
    assert!(s.iter().all(|x| x.abs() <= 1.0));
    assert!(s.iter().any(|x| x.abs() > 0.8), "the range is used");
}

/// One block of N equals two of N/2.
#[test]
fn the_sequence_continues_across_calls() {
    let mut whole = [0.0f32; 16];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Triangular,
        LSB,
        &mut whole,
    );
    let mut split = [0.0f32; 16];
    let mut n = DitherNoise::new();
    let (a, b) = split.split_at_mut(6);
    apply(&mut n, DitherShape::Triangular, LSB, a);
    apply(&mut n, DitherShape::Triangular, LSB, b);
    assert_eq!(whole, split);
}

/// In `f64` the first rectangular draw is the seeded word over `2^64` less a half, to the
/// `2^-53` grid, and it is added to the signal at full `f64` precision: a signal of 0.1 (not
/// an `f32`) leaves that draw behind to the rounding of 0.1 itself, `2^-56`.
#[test]
fn an_f64_block_keeps_its_precision_and_gets_the_seeded_draw() {
    let mut s = [0.1f64; 1];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Rectangular,
        LSB,
        &mut s,
    );
    let want = seeded_fraction(0) - 0.5;
    let added = s[0] - 0.1;
    assert!((added - want).abs() < 2f64.powi(-50), "{added} vs {want}");
    assert!(
        (f64::from(added as f32) - added).abs() > 1e-12,
        "the draw is finer than an f32: {added}"
    );
}

/// The triangular draw is the difference of two uniforms on `[0, 1)`: its variance is
/// `lsb^2 / 6` and its mean 0, at `f32` and at a 24-bit `lsb` of `2^-23` as at 1 (the draws are
/// on a `2^-24` grid whatever the scale). The sample variance of `N = 65536` draws has standard
/// deviation `sigma^2 sqrt((kurtosis - 1) / N)` with kurtosis 2.4 for the triangle, and the mean
/// `sigma / sqrt(N)`; the bound is `1.5 sqrt(2 ln N)` of each. The `f32` grid adds at most
/// `2^-24` of bias to a draw, far under the bound.
#[test]
fn f32_triangular_noise_has_the_variance_of_a_triangle() {
    const N: usize = 65_536;
    let sigma2 = 1.0_f64 / 6.0;
    let factor = 1.5 * (2.0 * (N as f64).ln()).sqrt();
    let var_bound = factor * sigma2 * (1.4 / N as f64).sqrt();
    let mean_bound = factor * (sigma2 / N as f64).sqrt();
    for lsb in [1.0_f64, 2f64.powi(-23)] {
        let mut s = vec![0.0f32; N];
        apply(
            &mut DitherNoise::new(),
            DitherShape::Triangular,
            Amplitude::new(lsb),
            &mut s,
        );
        let draws: Vec<f64> = s.iter().map(|&x| f64::from(x) / lsb).collect();
        let mean = draws.iter().sum::<f64>() / N as f64;
        let var = draws.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / N as f64;
        assert!(
            mean.abs() < mean_bound,
            "lsb {lsb}: mean {mean} vs bound {mean_bound}"
        );
        assert!(
            (var - sigma2).abs() < var_bound,
            "lsb {lsb}: variance {var} vs {sigma2} +- {var_bound}"
        );
    }
}

/// The rectangular draw is one uniform on `[0, 1)` less a half: its variance is `lsb^2 / 12`
/// and its mean 0. The sample variance of `N = 65536` draws has standard deviation
/// `sigma^2 sqrt((kurtosis - 1) / N)` with kurtosis 1.8 for the uniform, and the mean
/// `sigma / sqrt(N)`; the bound is `1.5 sqrt(2 ln N)` of each.
#[test]
fn f32_rectangular_noise_has_the_variance_of_a_uniform() {
    const N: usize = 65_536;
    let sigma2 = 1.0_f64 / 12.0;
    let factor = 1.5 * (2.0 * (N as f64).ln()).sqrt();
    let var_bound = factor * sigma2 * (0.8 / N as f64).sqrt();
    let mean_bound = factor * (sigma2 / N as f64).sqrt();
    let mut s = vec![0.0f32; N];
    apply(
        &mut DitherNoise::new(),
        DitherShape::Rectangular,
        LSB,
        &mut s,
    );
    let draws: Vec<f64> = s.iter().map(|&x| f64::from(x)).collect();
    let mean = draws.iter().sum::<f64>() / N as f64;
    let var = draws.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / N as f64;
    assert!(mean.abs() < mean_bound, "mean {mean} vs bound {mean_bound}");
    assert!(
        (var - sigma2).abs() < var_bound,
        "variance {var} vs {sigma2} +- {var_bound}"
    );
}
