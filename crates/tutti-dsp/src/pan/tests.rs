use super::*;

fn gains(pan: f32, law: PanLaw) -> (f32, f32) {
    let (l, r) = pan_gains(Pan(pan), law);
    (l.get(), r.get())
}

fn near(got: (f32, f32), want: (f64, f64)) {
    assert!(
        (f64::from(got.0) - want.0).abs() < 1e-6 && (f64::from(got.1) - want.1).abs() < 1e-6,
        "{got:?} against {want:?}"
    );
}

/// The square-root law at stated points: centre `sqrt(1/2)` each, a quarter right
/// `(sqrt(3/8), sqrt(5/8))`, the ends `(1, 0)` and `(0, 1)`; the pair carries unit
/// power everywhere.
#[test]
fn the_square_root_law_is_sqrt_of_the_half_sums() {
    let half = 0.5_f64.sqrt();
    near(gains(0.0, PanLaw::SquareRoot), (half, half));
    near(
        gains(0.25, PanLaw::SquareRoot),
        ((0.375_f64).sqrt(), (0.625_f64).sqrt()),
    );
    near(gains(-1.0, PanLaw::SquareRoot), (1.0, 0.0));
    near(gains(1.0, PanLaw::SquareRoot), (0.0, 1.0));
    for p in [-0.9, -0.3, 0.1, 0.77] {
        let (l, r) = gains(p, PanLaw::SquareRoot);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }
}

/// RP-036's Default Pan Formula, `cos` and `sin` of `pi/4 (1 + p)`: the centre is
/// `cos(pi/4) = sqrt(1/2)` each side, a pan of 1/2 is `(cos 3pi/8, sin 3pi/8)`, and
/// the ends are exactly `(1, 0)` and `(0, 1)` (not `cos(pi/2)`'s 4e-8).
#[test]
fn rp036_is_cos_and_sin_of_the_angle_along_the_pan() {
    use core::f64::consts::{FRAC_PI_4, FRAC_PI_8};
    let half = 0.5_f64.sqrt();
    near(gains(0.0, PanLaw::Rp036), (half, half));
    near(
        gains(0.5, PanLaw::Rp036),
        ((3.0 * FRAC_PI_8).cos(), (3.0 * FRAC_PI_8).sin()),
    );
    near(
        gains(-0.5, PanLaw::Rp036),
        ((FRAC_PI_8).cos(), (FRAC_PI_8).sin()),
    );
    near(
        gains(0.5, PanLaw::Rp036),
        ((FRAC_PI_4 * 1.5).cos(), (FRAC_PI_4 * 1.5).sin()),
    );
    assert_eq!(gains(-1.0, PanLaw::Rp036), (1.0, 0.0));
    assert_eq!(gains(1.0, PanLaw::Rp036), (0.0, 1.0));
}

/// A pan past an end reads as that end, and a NaN as the centre.
#[test]
fn a_pan_out_of_range_is_the_nearer_end_and_nan_is_the_centre() {
    for law in [PanLaw::SquareRoot, PanLaw::Rp036] {
        assert_eq!(gains(3.0, law), gains(1.0, law));
        assert_eq!(gains(-3.0, law), gains(-1.0, law));
        assert_eq!(gains(f32::NAN, law), gains(0.0, law));
    }
}
