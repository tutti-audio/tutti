//! Interpolation kernels shared by the delay line and the sampler's readers.

use tutti_buffers::Float;

/// The linear blend of `a` toward `b` at `t`, `a (1 - t) + b t`: exactly `a` at
/// `t = 0` and exactly `b` at `t = 1`, where `a + (b - a) t` rounds off `b`.
/// Monotone between the ends for `t` in `0..=1`. One expression, so every blend in
/// the workspace rounds the same way.
///
/// ```
/// use tutti_dsp::lerp;
///
/// assert_eq!(lerp(0.1_f32, 0.7, 0.0), 0.1);
/// assert_eq!(lerp(0.1_f32, 0.7, 1.0), 0.7);
/// assert_eq!(lerp(2.0_f64, 4.0, 0.5), 3.0);
/// ```
#[inline]
pub fn lerp<F: Float>(a: F, b: F, t: F) -> F {
    a * (F::ONE - t) + b * t
}

/// The vertex of a parabola through three equally spaced samples around a
/// maximum: where [`parabolic_peak`] puts the peak, and how high.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParabolicPeak<F> {
    /// The vertex's distance from the middle sample, in sample spacings,
    /// within `-0.5..=0.5`.
    pub offset: F,
    /// The parabola's value at the vertex, at least the middle sample's.
    pub value: F,
}

/// The vertex of the parabola through `below`, `mid` and `above`, the samples
/// at offsets `-1`, `0` and `1`, for a peak whose middle sample is the largest.
///
/// The offset is `(below - above) / (2 (below - 2 mid + above))`, clamped to
/// half a spacing: a middle sample that is not the largest has its vertex
/// beyond a neighbour, and the clamp keeps the refinement within the sample it
/// refines. The three points must bend down (`below - 2 mid + above < 0`); a
/// flat top, a minimum or a NaN has no vertex, and the result is offset `0`
/// at `mid`. For the minimum of a curve, pass the negated samples.
///
/// ```
/// use tutti_dsp::parabolic_peak;
///
/// // y = -(x - 0.3)^2 sampled at -1, 0, 1 peaks at 0.3, where it is 0.
/// let peak = parabolic_peak(-1.69_f64, -0.09, -0.49);
/// assert!((peak.offset - 0.3).abs() < 1e-12 && peak.value.abs() < 1e-12);
/// ```
#[inline]
#[must_use]
pub fn parabolic_peak<F: Float>(below: F, mid: F, above: F) -> ParabolicPeak<F> {
    let half = F::from_f32(0.5);
    let curvature = below - (mid + mid) + above;
    if curvature.is_nan() || curvature >= F::ZERO {
        return ParabolicPeak {
            offset: F::ZERO,
            value: mid,
        };
    }
    let slope = below - above;
    let offset = (half * slope / curvature).clamp(-half, half);
    ParabolicPeak {
        offset,
        value: mid - F::from_f32(0.25) * slope * offset,
    }
}

/// Catmull-Rom cubic Hermite interpolation across four consecutive taps.
///
/// `y1` is the sample at the integer position, `y0`/`y2`/`y3` its neighbours
/// (`y0` one behind, `y2`/`y3` ahead); `t` is the fractional offset in
/// `[0, 1)` between `y1` and `y2`.
///
/// The operation order is part of the contract: the sampler's readers and the
/// delay line render bit for bit what this expression gives, so a
/// re-association changes their output. Per-sample, so `#[inline]` lets the
/// sampler's loops fuse it across the crate boundary.
///
/// ```
/// use tutti_dsp::cubic_hermite;
///
/// // Along a line the cubic is the line.
/// assert_eq!(cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.5), 1.5);
/// ```
#[inline]
pub fn cubic_hermite<F: Float>(y0: F, y1: F, y2: F, y3: F, t: F) -> F {
    let (half, two, two_and_half, one_and_half) = (
        F::from_f64(0.5),
        F::from_f64(2.0),
        F::from_f64(2.5),
        F::from_f64(1.5),
    );
    let c0 = y1;
    let c1 = half * (y2 - y0);
    let c2 = y0 - two_and_half * y1 + two * y2 - half * y3;
    let c3 = half * (y3 - y0) + one_and_half * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blend `a (1 - t) + b t` is exactly `a` at 0 and exactly `b` at 1 for ends where
    /// `a + (b - a) t` rounds off `b` (1e8 to 1 in `f32`: `1 - 1e8` rounds to `-1e8`, so that
    /// form lands on 0), is the midpoint at 1/2 and a quarter of the way at 1/4.
    #[test]
    fn lerp_is_exact_at_both_ends_and_linear_between() {
        let (a, b) = (1.0e8_f32, 1.0_f32);
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        assert_ne!(a + (b - a) * 1.0, b, "the other form misses the end");
        assert_eq!(lerp(2.0_f32, 4.0, 0.5), 3.0);
        assert_eq!(lerp(-1.0_f64, 3.0, 0.25), 0.0);
        assert_eq!(lerp(5.0_f32, 5.0, 0.37), 5.0);
    }

    /// The curve passes through its inner taps at `t = 0` and `t = 1`. For
    /// the taps -1, 0, 1, 8 the coefficients are `c1 = 1`, `c2 = -3`,
    /// `c3 = 3`, so the curve is `3t^3 - 3t^2 + t` and its midpoint is 0.125.
    #[test]
    fn passes_through_its_inner_taps_and_follows_the_cubic() {
        let (y0, y1, y2, y3) = (-1.0, 0.0, 1.0, 8.0);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 0.0), y1);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 1.0), y2);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 0.5), 0.125);
    }

    /// The parabola `y = c - k (x - v)^2` sampled at -1, 0, 1 has its vertex at
    /// offset `v` and value `c`, for any `v` within half a spacing.
    #[test]
    fn the_vertex_of_a_sampled_parabola_is_its_offset_and_height() {
        for &(c, k, v) in &[(0.0_f64, 1.0, 0.3), (2.0, 0.5, -0.45), (-1.0, 3.0, 0.0)] {
            let y = |x: f64| c - k * (x - v) * (x - v);
            let peak = parabolic_peak(y(-1.0), y(0.0), y(1.0));
            assert!(
                (peak.offset - v).abs() < 1e-12,
                "offset {} vs {v}",
                peak.offset
            );
            assert!(
                (peak.value - c).abs() < 1e-12,
                "value {} vs {c}",
                peak.value
            );
        }
    }

    /// A middle sample below a neighbour puts the vertex past it: the points
    /// -4, -1, 0 (whose vertex lies at offset 1) clamp to half a spacing.
    #[test]
    fn a_vertex_beyond_half_a_spacing_is_clamped_to_it() {
        let peak = parabolic_peak(-4.0_f32, -1.0, -0.0);
        assert_eq!(peak.offset, 0.5);
        let peak = parabolic_peak(-0.0_f32, -1.0, -4.0);
        assert_eq!(peak.offset, -0.5);
    }

    /// Three points that do not bend down have no vertex: a flat top, a
    /// minimum and a NaN give offset 0 at the middle sample.
    #[test]
    fn a_flat_top_a_minimum_and_a_nan_have_no_vertex() {
        for (a, b, c) in [(1.0_f32, 1.0, 1.0), (1.0, 0.0, 2.0), (f32::NAN, 1.0, 0.0)] {
            let peak = parabolic_peak(a, b, c);
            assert_eq!((peak.offset, peak.value), (0.0, b));
        }
    }
}
