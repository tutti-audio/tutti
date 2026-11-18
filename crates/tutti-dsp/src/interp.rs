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
mod tests;
