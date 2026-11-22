use super::*;
use tutti_units::{Normalized, Samples};

use crate::Exponent;

fn at(t: f64) -> Normalized {
    Normalized::of(t)
}

/// `value` is `from + (to - from) * g(t)`, in either direction, and a hold
/// keeps `from` until the end.
#[test]
fn value_scales_the_law_onto_the_endpoints() {
    assert_eq!(Piece::new(2.0, 6.0).value(None, at(0.25)), 3.0);
    assert_eq!(
        Piece::new(2.0, 6.0)
            .law(Law::PowerIn(Exponent::of(2.0)))
            .value(None, at(0.5)),
        3.0
    );
    assert_eq!(
        Piece::new(6.0, 2.0)
            .law(Law::PowerIn(Exponent::of(2.0)))
            .value(None, at(0.5)),
        5.0
    );
    assert_eq!(
        Piece::new(2.0, 6.0).law(Law::Hold).value(None, at(0.5)),
        2.0
    );
    assert_eq!(
        Piece::new(2.0, 6.0).law(Law::Hold).value(None, at(1.0)),
        6.0
    );
}

/// An azimuth piece from 170 to -170 turns through 180, not around through 0:
/// at progress `t` it is `wrap(170 + 20 t)`, for several `t` and a shaped law.
#[test]
fn an_azimuth_piece_across_the_seam_passes_through_180_never_0() {
    let p = Piece::new(170.0, -170.0);
    let az = UnitParam::Azimuth;
    for t in [0.0f64, 0.125, 0.25, 0.5, 0.75, 0.875] {
        let want = az.wrap(170.0 + 20.0 * t);
        let got = p.value(Some(az), at(t));
        assert!((got - want).abs() < 1e-12, "t {t}: {got} vs {want}");
        assert!(got.abs() > 169.0, "t {t}: {got} is nowhere near 0");
    }
    assert_eq!(
        p.value(Some(az), at(0.5)),
        -180.0,
        "the midpoint is the seam itself"
    );
    let eased = Piece::new(170.0, -170.0).law(Law::PowerIn(Exponent::of(2.0)));
    let want = az.wrap(170.0 + 20.0 * 0.25);
    assert!((eased.value(Some(az), at(0.5)) - want).abs() < 1e-12);
}

/// A param that is a line interpolates as `value` does, across the same ends:
/// elevation from 170 to -170 passes through 0.
#[test]
fn a_linear_param_piece_is_unchanged_by_the_circular_rule() {
    let p = Piece::new(170.0, -170.0);
    for param in [
        UnitParam::Elevation,
        UnitParam::Spread,
        UnitParam::StereoWidth,
    ] {
        for t in [0.0f64, 0.25, 0.5, 0.9] {
            assert_eq!(
                p.value(Some(param), at(t)).to_bits(),
                p.value(None, at(t)).to_bits()
            );
        }
    }
    assert_eq!(p.value(Some(UnitParam::Elevation), at(0.5)), 0.0);
}

/// An azimuth ramp lands exactly on its target on its last frame, and takes
/// the short arc on the frames before.
#[test]
fn an_azimuth_ramp_takes_the_short_arc_and_lands_on_its_target() {
    let az = UnitParam::Azimuth;
    let n = 4;
    let got: Vec<f64> = (0..n)
        .map(|k| Piece::ramp_at(Some(az), 170.0, -170.0, Samples::new(k), Samples::new(n)))
        .collect();
    assert_eq!(got[3], -170.0);
    assert_eq!(got[1], -180.0);
    assert_eq!(got[0], az.wrap(175.0));
}
