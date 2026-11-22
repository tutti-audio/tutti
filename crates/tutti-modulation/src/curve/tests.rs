use super::*;
use tutti_curve::Breakpoint;
use tutti_curve::Exponent;
use tutti_units::Mix;

fn trajectory() -> Trajectory<Mix> {
    let mut e = Trajectory::new();
    e.add_breakpoint(
        Breakpoint::new(Beat::new(1.0), Mix::new(0.2)).law(Law::PowerIn(Exponent::of(2.0))),
    );
    e.add_breakpoint(Breakpoint::new(Beat::new(2.0), Mix::new(0.8)));
    e.add_breakpoint(Breakpoint::new(Beat::new(3.0), Mix::new(0.4)));
    e
}

/// A trajectory's pieces are its breakpoints: a hold before the first
/// and from the last, a move between, each agreeing with `value_at`
/// over its span; a breakpoint starts the piece after it.
#[test]
fn trajectory_pieces_are_its_breakpoints() {
    let e = trajectory();
    assert_eq!(
        e.segment_at(Beat::new(0.5)),
        Some(CurveSegment::Hold {
            value: 0.2,
            until: Some(Beat::new(1.0))
        })
    );
    let exp = CurveSegment::Move {
        start: Beat::new(1.0),
        end: Beat::new(2.0),
        from: 0.2,
        to: 0.8,
        shape: Law::PowerIn(Exponent::of(2.0)),
    };
    assert_eq!(e.segment_at(Beat::new(1.0)), Some(exp));
    assert_eq!(e.segment_at(Beat::new(1.7)), Some(exp));
    assert!(matches!(
        e.segment_at(Beat::new(2.0)),
        Some(CurveSegment::Move {
            shape: Law::Linear,
            ..
        })
    ));
    assert_eq!(
        e.segment_at(Beat::new(9.0)),
        Some(CurveSegment::Hold {
            value: 0.4,
            until: None
        })
    );
}

/// A piece the trajectory's range would bend is no segment: a clamp cutting
/// into its endpoints. A clamp both endpoints stay inside leaves it one.
#[test]
fn a_bent_piece_is_no_segment() {
    assert!(
        trajectory()
            .with_range(Mix::new(0.0), Mix::new(0.5))
            .segment_at(Beat::new(1.5))
            .is_none()
    );
    assert!(
        trajectory()
            .with_range(Mix::new(0.0), Mix::new(1.0))
            .segment_at(Beat::new(1.5))
            .is_some()
    );
}
