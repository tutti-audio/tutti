use super::*;

/// The peak is returned as a bound raised by a part in a million.
const RAISE: f64 = 1e-6;

/// A one-pole low-pass `H(w) = (1 - a) / (1 - a e^(-jw))` has unit gain at DC and falls
/// monotonically to `(1 - a) / (1 + a)` at Nyquist, so its peak is 1 at `w = 0`, where
/// the grid has a point.
#[test]
fn a_one_pole_low_pass_peaks_at_dc_with_unit_gain() {
    let a = 0.9f64;
    let mag = |w: f64| (1.0 - a) / (1.0 - a * (-w).cos()).hypot(a * (-w).sin());
    let peak = peak_magnitude(vec![core::f64::consts::PI, 1.0, 0.0, 0.5], mag);
    assert!(
        (1.0..=1.0 + RAISE * 1.001).contains(&peak),
        "peak {peak} against 1 raised by {RAISE}"
    );
}

/// A peak between grid points is found by the refinement: `1 / (1 + (w - 1)^2)` peaks at
/// `w = 1` with value 1, while the best grid point (`w = 1.4`) reads `1 / 1.16`.
#[test]
fn a_peak_between_grid_points_is_refined_and_bounded_from_above() {
    let mag = |w: f64| 1.0 / (1.0 + (w - 1.0).powi(2));
    let peak = peak_magnitude(vec![0.0, 0.5, 1.4, core::f64::consts::PI], mag);
    assert!(
        (1.0..=1.0 + RAISE * 1.001).contains(&peak),
        "peak {peak} against 1 raised by {RAISE}"
    );
}

/// An angle outside `0..=pi` is not probed, and duplicates change nothing.
#[test]
fn angles_outside_zero_to_pi_are_ignored() {
    let mag = |w: f64| {
        if w > core::f64::consts::PI {
            100.0
        } else {
            1.0 - w / 10.0
        }
    };
    let peak = peak_magnitude(vec![4.0, 0.0, 0.0, -1.0, 2.0], mag);
    assert!((1.0..=1.0 + RAISE * 1.001).contains(&peak), "peak {peak}");
}
