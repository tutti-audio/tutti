//! The trajectory against what a breakpoint curve is: the figures are written
//! out from each law's closed form.

use crate::Exponent;
use tutti_types::{Beat, Bipolar, Hz};

use super::{Breakpoint, Trajectory};
use crate::Law;

fn point(beat: f64, value: f32) -> Breakpoint<Hz> {
    Breakpoint::new(Beat(beat), Hz(value))
}

fn ramp() -> Trajectory<Hz> {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::with_law(
        Beat(1.0),
        Hz(0.0),
        Law::PowerIn(Exponent::of(2.0)),
    ));
    e.add_breakpoint(point(3.0, 1.0));
    e
}

/// No breakpoint, no value; one breakpoint is its value everywhere.
#[test]
fn an_empty_trajectory_has_no_value_and_one_breakpoint_is_everywhere() {
    assert_eq!(Trajectory::<Hz>::new().value_at(Beat(1.0)), None);
    let mut one = Trajectory::new();
    one.add_breakpoint(point(2.0, 0.7));
    for b in [-1.0, 2.0, 9.0] {
        assert_eq!(one.value_at(Beat(b)), Some(Hz(0.7)));
    }
}

/// Before the first breakpoint and from the last on, the end values hold; between
/// them a piece follows its starting breakpoint's law: here `t²` over `[1, 3]`, so
/// beat 2 is `t = 1/2`, a quarter, and beat 2.5 is `t = 3/4`, 9/16.
#[test]
fn a_piece_follows_its_starting_breakpoints_law() {
    let e = ramp();
    assert_eq!(e.value_at(Beat(0.0)), Some(Hz(0.0)));
    assert_eq!(e.value_at(Beat(1.0)), Some(Hz(0.0)));
    assert_eq!(e.value_at(Beat(2.0)), Some(Hz(0.25)));
    assert_eq!(e.value_at(Beat(2.5)), Some(Hz(0.5625)));
    assert_eq!(e.value_at(Beat(3.0)), Some(Hz(1.0)));
    assert_eq!(e.value_at(Beat(9.0)), Some(Hz(1.0)));
}

/// A linear piece between two breakpoints is the straight line: a quarter of
/// the way along `[0, 4]` from `100` to `300` is `150`.
#[test]
fn a_linear_piece_is_the_straight_line() {
    let mut e = Trajectory::new();
    e.add_breakpoint(point(0.0, 100.0))
        .add_breakpoint(point(4.0, 300.0));
    assert_eq!(e.value_at(Beat(1.0)), Some(Hz(150.0)));
    assert_eq!(e.value_at(Beat(2.0)), Some(Hz(200.0)));
}

/// A breakpoint reads its own value, whatever the law before it, and each
/// piece starts at its breakpoint.
#[test]
fn a_breakpoint_reads_its_own_value() {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::with_law(Beat(0.0), Hz(0.0), Law::Hold));
    e.add_breakpoint(Breakpoint::with_law(Beat(2.0), Hz(5.0), Law::Hold));
    e.add_breakpoint(point(4.0, 9.0));
    assert_eq!(e.value_at(Beat(1.999)), Some(Hz(0.0)));
    assert_eq!(e.value_at(Beat(2.0)), Some(Hz(5.0)));
    assert_eq!(e.value_at(Beat(3.999)), Some(Hz(5.0)));
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
    assert_eq!(e.breakpoints()[1].value, Hz(0.9));
    assert_eq!((e.len(), e.is_empty()), (3, false));
}

/// A range clamps every value read, a held end and a piece's middle alike.
#[test]
fn a_range_clamps_what_is_read() {
    let mut e = Trajectory::new().with_range(Hz(0.0), Hz(1.0));
    e.add_breakpoint(point(0.0, -1.0));
    e.add_breakpoint(point(2.0, 3.0));
    assert_eq!(e.range(), Some((Hz(0.0), Hz(1.0))));
    assert_eq!(e.value_at(Beat(0.0)), Some(Hz(0.0)));
    assert_eq!(e.value_at(Beat(0.25)), Some(Hz(0.0)));
    assert_eq!(e.value_at(Beat(1.0)), Some(Hz(1.0)));
    assert_eq!(e.value_at(Beat(1.5)), Some(Hz(1.0)));
    assert_eq!(e.value_at(Beat(2.0)), Some(Hz(1.0)));
}

/// A trajectory over a range-checked unit reads values that unit holds: a
/// `Bipolar` line from `-1` to `1` is `0` at its midpoint and `-0.5` a quarter in,
/// and the range narrows it to `-0.5..=0.5`.
#[test]
fn a_bipolar_trajectory_reads_in_its_unit() {
    let mut e = Trajectory::new();
    e.add_breakpoint(Breakpoint::new(Beat(0.0), Bipolar::MIN))
        .add_breakpoint(Breakpoint::new(Beat(4.0), Bipolar::MAX));
    assert_eq!(e.value_at(Beat(2.0)), Some(Bipolar::ZERO));
    assert_eq!(e.value_at(Beat(1.0)), Some(Bipolar::of(-0.5)));
    let e = e.with_range(Bipolar::of(-0.5), Bipolar::of(0.5));
    assert_eq!(e.value_at(Beat(0.0)), Some(Bipolar::of(-0.5)));
    assert_eq!(e.value_at(Beat(4.0)), Some(Bipolar::of(0.5)));
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
    let kept: Vec<(f64, f32)> = e
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
        e.add_breakpoint(Breakpoint::with_law(Beat(0.0), Hz(0.0), law));
        e.add_breakpoint(point(1.0, 0.25));
        e.add_breakpoint(point(2.0, 1.0));
        e.simplify(0.01);
        e.len()
    };
    assert_eq!(build(Law::PowerIn(Exponent::of(2.0))), 2);
    assert_eq!(build(Law::Linear), 3);
}
