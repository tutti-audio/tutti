use super::{bar_step, snap, whole_tick_step};

/// The grid position nearest a tick (a tie later) is analytic: a step of 20
/// from 0 puts 29 at 20, the tie 30 at 40, and 31 at 40.
/// Spec: m2-104/7.5.3#4
#[test]
fn snap_takes_the_nearest_position_and_a_tie_later() {
    assert_eq!([29, 30, 31].map(|t| snap(t, 0, 20, u64::MAX)), [20, 40, 40]);
    // From an origin of 384 with a step of 384.
    assert_eq!(snap(200, 0, 384, u64::MAX), 384);
    // A tick at or before the origin is at the origin, never before it.
    assert_eq!(snap(300, 384, 384, u64::MAX), 384);
    assert_eq!(snap(384, 384, 384, u64::MAX), 384);
}

/// At the top of the range nothing overflows: the nearest position past `end`
/// is the last one at or before it, and a tick past the last whole step of
/// `u64` is clamped the same way.
/// Spec: m2-104/7.5.3#4, m2-104/7.5.4#3
#[test]
fn snap_cannot_overflow_near_the_top_of_u64() {
    let top = u64::MAX - 3;
    let at = snap(top, 0, 20, top);
    assert!(at <= top && top - at < 20 && at.is_multiple_of(20));
    // The nearest position would be past u64::MAX itself.
    let at = snap(u64::MAX, 7, 1 << 62, u64::MAX);
    assert!((at - 7).is_multiple_of(1 << 62));
    let at = snap(u64::MAX - 1, u64::MAX - 5, 10, u64::MAX);
    assert_eq!(at, u64::MAX - 5);
    // An end before the origin still gives the origin.
    assert_eq!(snap(100, 50, 20, 10), 50);
}

/// 4/4 at 96 per quarter is a 384 tick bar, 6/8 is 3 quarters, 7/16 at 100
/// per quarter is 175 ticks (7 sixteenths of 100 ticks per quarter, whole), and a
/// quarter of 100 ticks has a clock grid every 25 ticks.
/// Spec: m2-104/7.5.3#4, m2-104/7.5.4#3
#[test]
fn bar_and_clock_steps_are_whole_ticks() {
    assert_eq!(bar_step(96, 4, 2), 384);
    assert_eq!(bar_step(480, 6, 3), 1440);
    assert_eq!(bar_step(100, 7, 4), 175);
    assert_eq!(whole_tick_step(100, 24), 25);
    assert_eq!(whole_tick_step(480, 24), 20);
}
