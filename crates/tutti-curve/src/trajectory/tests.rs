//! The trajectory against what a breakpoint curve is: the figures are written
//! out from each law's closed form.

use crate::Exponent;
use tutti_units::{Beat, Bipolar, Db};

use super::{Breakpoint, Trajectory};
use crate::Law;

fn point(beat: f64, value: f64) -> Breakpoint<Db> {
    Breakpoint::new(Beat::new(beat), Db::new(value))
}

fn ramp() -> Trajectory<Db> {
    let mut e = Trajectory::new();
    e.add_breakpoint(
        Breakpoint::new(Beat::new(1.0), Db::new(0.0)).law(Law::PowerIn(Exponent::of(2.0))),
    );
    e.add_breakpoint(point(3.0, 1.0));
    e
}

/// No breakpoint, no value; one breakpoint is its value everywhere.
#[test]
fn an_empty_trajectory_has_no_value_and_one_breakpoint_is_everywhere() {
    assert_eq!(Trajectory::<Db>::new().value_at(Beat::new(1.0)), None);
    let mut one = Trajectory::new();
    one.add_breakpoint(point(2.0, 0.7));
    for b in [-1.0, 2.0, 9.0] {
        assert_eq!(one.value_at(Beat::new(b)), Some(Db::new(0.7)));
    }
}

/// Before the first breakpoint and from the last on, the end values hold; between
/// them a piece follows its starting breakpoint's law: here `t²` over `[1, 3]`, so
/// beat 2 is `t = 1/2`, a quarter, and beat 2.5 is `t = 3/4`, 9/16.
#[test]
fn a_piece_follows_its_starting_breakpoints_law() {
    let e = ramp();
    assert_eq!(e.value_at(Beat::new(0.0)), Some(Db::new(0.0)));
    assert_eq!(e.value_at(Beat::new(1.0)), Some(Db::new(0.0)));
    assert_eq!(e.value_at(Beat::new(2.0)), Some(Db::new(0.25)));
    assert_eq!(e.value_at(Beat::new(2.5)), Some(Db::new(0.5625)));
    assert_eq!(e.value_at(Beat::new(3.0)), Some(Db::new(1.0)));
    assert_eq!(e.value_at(Beat::new(9.0)), Some(Db::new(1.0)));
}

/// A linear piece between two breakpoints is the straight line: a quarter of
/// the way along `[0, 4]` from `100` to `300` is `150`.
#[test]
fn a_linear_piece_is_the_straight_line() {
    let mut e = Trajectory::new();
    e.add_breakpoint(point(0.0, 100.0))
        .add_breakpoint(point(4.0, 300.0));
    assert_eq!(e.value_at(Beat::new(1.0)), Some(Db::new(150.0)));
    assert_eq!(e.value_at(Beat::new(2.0)), Some(Db::new(200.0)));
}

/// A breakpoint reads its own value, whatever the law before it, and each
/// piece starts at its breakpoint.
#[test]
fn a_breakpoint_reads_its_own_value() {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Db::new(0.0)).law(Law::Hold));
    e.add_breakpoint(Breakpoint::new(Beat::new(2.0), Db::new(5.0)).law(Law::Hold));
    e.add_breakpoint(point(4.0, 9.0));
    assert_eq!(e.value_at(Beat::new(1.999)), Some(Db::new(0.0)));
    assert_eq!(e.value_at(Beat::new(2.0)), Some(Db::new(5.0)));
    assert_eq!(e.value_at(Beat::new(3.999)), Some(Db::new(5.0)));
}

/// Breakpoints sort by time as they come, and a breakpoint at an existing time replaces
/// it.
#[test]
fn add_breakpoint_keeps_time_order_and_replaces_a_tie() {
    let mut e = Trajectory::new();
    e.add_breakpoint(point(2.0, 0.2))
        .add_breakpoint(point(0.0, 0.0))
        .add_breakpoint(point(1.0, 0.1));
    e.add_breakpoint(point(1.0, 0.9));
    let times: Vec<f64> = e.breakpoints().iter().map(|p| p.time.get()).collect();
    assert_eq!(times, [0.0, 1.0, 2.0]);
    assert_eq!(e.breakpoints()[1].value, Db::new(0.9));
    assert_eq!((e.len(), e.is_empty()), (3, false));
}

/// A range clamps every value read, a held end and a piece's middle alike.
#[test]
fn a_range_clamps_what_is_read() {
    let mut e = Trajectory::new().with_range(Db::new(0.0), Db::new(1.0));
    e.add_breakpoint(point(0.0, -1.0));
    e.add_breakpoint(point(2.0, 3.0));
    assert_eq!(e.range(), Some((Db::new(0.0), Db::new(1.0))));
    assert_eq!(e.value_at(Beat::new(0.0)), Some(Db::new(0.0)));
    assert_eq!(e.value_at(Beat::new(0.25)), Some(Db::new(0.0)));
    assert_eq!(e.value_at(Beat::new(1.0)), Some(Db::new(1.0)));
    assert_eq!(e.value_at(Beat::new(1.5)), Some(Db::new(1.0)));
    assert_eq!(e.value_at(Beat::new(2.0)), Some(Db::new(1.0)));
}

/// A trajectory over a range-checked unit reads values that unit holds: a
/// `Bipolar` line from `-1` to `1` is `0` at its midpoint and `-0.5` a quarter in,
/// and the range narrows it to `-0.5..=0.5`.
#[test]
fn a_bipolar_trajectory_reads_in_its_unit() {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Bipolar::MIN))
        .add_breakpoint(Breakpoint::new(Beat::new(4.0), Bipolar::MAX));
    assert_eq!(e.value_at(Beat::new(2.0)), Some(Bipolar::ZERO));
    assert_eq!(e.value_at(Beat::new(1.0)), Some(Bipolar::of(-0.5)));
    let e = e.with_range(Bipolar::of(-0.5), Bipolar::of(0.5));
    assert_eq!(e.value_at(Beat::new(0.0)), Some(Bipolar::of(-0.5)));
    assert_eq!(e.value_at(Beat::new(4.0)), Some(Bipolar::of(0.5)));
}

/// Simplify drops a breakpoint its neighbours' piece already gives and keeps a
/// corner; the ends always stay.
#[test]
fn simplify_drops_the_redundant_and_keeps_the_corner() {
    let mut e = Trajectory::new();
    for (b, v) in [(0.0, 0.0), (1.0, 0.25), (2.0, 0.5), (3.0, 1.0), (4.0, 0.0)] {
        e.add_breakpoint(point(b, v));
    }
    e.simplify(0.01);
    let kept: Vec<(f64, f64)> = e
        .breakpoints()
        .iter()
        .map(|p| (p.time.get(), p.value.get()))
        .collect();
    assert_eq!(kept, [(0.0, 0.0), (2.0, 0.5), (3.0, 1.0), (4.0, 0.0)]);
}

/// The test of redundancy is the neighbour's own law: a breakpoint on a `t²`
/// piece is redundant, the same breakpoint on a line is not.
#[test]
fn simplify_reads_the_previous_breakpoints_law() {
    let build = |law| {
        let mut e = Trajectory::new();
        e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Db::new(0.0)).law(law));
        e.add_breakpoint(point(1.0, 0.25));
        e.add_breakpoint(point(2.0, 1.0));
        e.simplify(0.01);
        e.len()
    };
    assert_eq!(build(Law::PowerIn(Exponent::of(2.0))), 2);
    assert_eq!(build(Law::Linear), 3);
}

/// A trajectory of an `f64` unit keeps its width: the endpoint a billionth of a
/// degree past 90 is read back exactly, where an `f32` would round it to 90,
/// and the midpoint of two bearings is their mean.
#[test]
fn an_f64_unit_is_read_at_its_own_width() {
    use tutti_units::Azimuth;
    let tiny = 90.0 + 1e-9;
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat::new(0.0), Azimuth::new(10.0)))
        .add_breakpoint(Breakpoint::new(Beat::new(4.0), Azimuth::new(tiny)));
    assert_eq!(e.value_at(Beat::new(4.0)), Some(Azimuth::new(tiny)));
    assert_ne!(tiny as f32 as f64, tiny, "the figure needs more than f32");
    assert_eq!(
        e.value_at(Beat::new(2.0)),
        Some(Azimuth::new(10.0 + (tiny - 10.0) * 0.5))
    );
}

/// `simplify` takes its tolerance in the unit's own width.
#[test]
fn simplify_takes_its_tolerance_in_the_units_width() {
    use tutti_units::Elevation;
    let mut e = Trajectory::new();
    for (b, v) in [(0.0, 0.0), (1.0, 10.0 + 1e-9), (2.0, 20.0)] {
        e.add_breakpoint(Breakpoint::new(Beat::new(b), Elevation::new(v)));
    }
    e.simplify(1e-12);
    assert_eq!(e.len(), 3, "a 1e-9 corner is above a 1e-12 tolerance");
    e.simplify(1e-6);
    assert_eq!(e.len(), 2);
}
