//! Signal helpers for the unit tests.

/// The complex amplitude of the `hz` component of `x`, as `(re, im)` of
/// `2/N * sum(x[n] * e^(-j w n))` (a unit sine at phase 0 reads `(0, -1)`).
///
/// Exact (no leakage) when `x` spans a whole number of periods of `hz`.
pub(crate) fn lock_in(x: &[f32], hz: f64, rate: f64) -> (f64, f64) {
    let w = 2.0 * core::f64::consts::PI * hz / rate;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let a = w * n as f64;
        re += f64::from(v) * libm::cos(a);
        im -= f64::from(v) * libm::sin(a);
    }
    let k = 2.0 / x.len() as f64;
    (re * k, im * k)
}

/// The amplitude of [`lock_in`].
pub(crate) fn amplitude_at(x: &[f32], hz: f64, rate: f64) -> f64 {
    let (re, im) = lock_in(x, hz, rate);
    re.hypot(im)
}

/// `frames` of seeded uniform noise in `-level..=level`.
pub(crate) fn noise(seed: u64, frames: usize, level: f32) -> Vec<f32> {
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    let mut rng = StdRng::seed_from_u64(seed);
    (0..frames)
        .map(|_| rng.random_range(-level..=level))
        .collect()
}

/// `frames` of an `f64` signal that needs more than 24 significant bits: frame `k` is
/// `(0x2000_0001 + 3k) / 2^31`, an odd 29-bit integer over a power of two, so an `f32`
/// cannot hold it and a processor that narrows to `f32` anywhere returns a different
/// value.
pub(crate) fn wide_signal(frames: usize) -> Vec<f64> {
    const BASE: i64 = 0x2000_0001;
    const SCALE: f64 = 2_147_483_648.0;
    (0..frames as i64)
        .map(|k| (BASE + 3 * k) as f64 / SCALE)
        .collect()
}

/// Asserts `got` and `want` have one length and agree to `tolerance` at every frame; the
/// failure names the first frame that does not, with both values.
#[track_caller]
pub(crate) fn assert_close_f64(got: &[f64], want: &[f64], tolerance: f64) {
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
