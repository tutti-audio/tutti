use super::*;
use crate::monotonic_ns;

/// A sleep never wakes before its deadline, at several distances, and a
/// deadline already past returns instead of sleeping the distance back.
#[test]
fn a_sleep_never_returns_before_its_deadline() {
    for ahead_us in [-10_000_000, 0, 50, 1_000, 5_000, 20_000] {
        let deadline = monotonic_ns() + ahead_us * 1_000;
        sleep_until_ns(deadline);
        assert!(monotonic_ns() >= deadline, "woke {ahead_us} us ahead early");
    }
    // A negative deadline is past on any clock reading.
    for deadline in [i64::MIN, -1] {
        sleep_until_ns(deadline);
        assert!(monotonic_ns() >= deadline);
    }
}

/// The time left is the distance to the deadline, and nothing once the
/// deadline is reached or past, at any distance.
#[test]
fn the_time_left_is_the_distance_to_the_deadline() {
    use std::time::Duration;
    assert_eq!(
        relative::left(1_500, 1_000),
        Some(Duration::from_nanos(500))
    );
    assert_eq!(relative::left(1_000, 1_000), None);
    assert_eq!(relative::left(999, 1_000), None);
    assert_eq!(relative::left(i64::MIN, 5), None);
    assert_eq!(relative::left(5, i64::MAX), None);
    assert_eq!(
        relative::left(i64::MAX, i64::MIN),
        Some(Duration::from_nanos(i64::MAX.cast_unsigned()))
    );
}

/// The relative path, which the absolute one falls back to and every target
/// without an absolute sleep takes, never wakes early and returns for a
/// deadline in the past.
#[test]
fn the_relative_sleep_never_returns_before_its_deadline() {
    let deadline = monotonic_ns() + 15_000_000;
    relative::sleep_until_ns(deadline);
    assert!(monotonic_ns() >= deadline);
    relative::sleep_until_ns(i64::MIN);
}
