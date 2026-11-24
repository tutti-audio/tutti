//! Signal helpers for the unit tests.

use tutti_rt::SplitMix64;

pub(crate) use tutti_fixtures::uniform;

/// `frames` of seeded uniform noise in `-level..=level`.
pub(crate) fn noise(seed: u64, frames: usize, level: f32) -> Vec<f32> {
    let mut rng = SplitMix64::new(seed);
    (0..frames)
        .map(|_| uniform(&mut rng, -level, level))
        .collect()
}

/// `x` through `b` from rest, in `f64` direct form I: `y[n] = b0 x[n] + b1 x[n-1] + b2 x[n-2]
/// - a1 y[n-1] - a2 y[n-2]`.
pub(crate) fn direct_form(b: &crate::Biquad, x: &[f64]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&v| {
            let y = b.b0 * v + b.b1 * x1 + b.b2 * x2 - b.a1 * y1 - b.a2 * y2;
            (x2, x1, y2, y1) = (x1, v, y1, y);
            y
        })
        .collect()
}

/// `x` through the second-order Butterworth all-pass of `split` Hz at `rate`, from rest:
/// `(s^2 - sqrt2 s + 1) / (s^2 + sqrt2 s + 1)` under the bilinear map
/// `s = (1 / g) (1 - z^-1) / (1 + z^-1)`, `g = tan(pi split / rate)`. The sum of a
/// Linkwitz-Riley split's low-pass and high-pass.
pub(crate) fn butterworth_all_pass(x: &[f64], split: f64, rate: f64) -> Vec<f64> {
    let g = (core::f64::consts::PI * split / rate).tan();
    let (k, g2) = (core::f64::consts::SQRT_2, g * g);
    let a0 = 1.0 + k * g + g2;
    let b = crate::Biquad {
        b0: (1.0 - k * g + g2) / a0,
        b1: 2.0 * (g2 - 1.0) / a0,
        b2: 1.0,
        a1: 2.0 * (g2 - 1.0) / a0,
        a2: (1.0 - k * g + g2) / a0,
    };
    direct_form(&b, x)
}

/// The sine `|h| sin(w n + arg h)` that fits `y[from..]` best in least squares, as the phasor `h
/// = a + j b` of `a sin(w n) + b cos(w n)`, `n` counted from the start of `y`. Exact for a
/// whole number of periods or not.
pub(crate) fn sine_fit(y: &[f64], w: f64, from: usize) -> crate::Complex64 {
    let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &v) in y.iter().enumerate().skip(from) {
        let (s, c) = libm::sincos(w * n as f64);
        (ss, sc, cc, ys, yc) = (ss + s * s, sc + s * c, cc + c * c, ys + v * s, yc + v * c);
    }
    let det = ss * cc - sc * sc;
    crate::Complex64::new((ys * cc - yc * sc) / det, (yc * ss - ys * sc) / det)
}

/// The largest distance of `y[from..]` from the steady state `|h| sin(w n + arg h)`.
pub(crate) fn steady_deviation(y: &[f64], w: f64, from: usize, h: crate::Complex64) -> f64 {
    y.iter()
        .enumerate()
        .skip(from)
        .map(|(n, &v)| (v - h.norm() * libm::sin(w * n as f64 + h.arg())).abs())
        .fold(0.0, f64::max)
}
