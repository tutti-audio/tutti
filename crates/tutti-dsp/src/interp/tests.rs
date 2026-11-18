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
