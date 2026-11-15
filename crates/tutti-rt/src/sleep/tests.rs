use std::time::Instant;

use super::*;
use crate::monotonic_ns;

/// A sleep never wakes before its deadline, at several distances.
#[test]
fn a_sleep_never_returns_before_its_deadline() {
    for ahead_us in [0, 50, 1_000, 5_000, 20_000] {
        let deadline = monotonic_ns() + ahead_us * 1_000;
        sleep_until_ns(deadline);
        assert!(monotonic_ns() >= deadline, "woke {ahead_us} us ahead early");
    }
}

/// A deadline that has passed returns without sleeping a whole interval.
#[test]
fn a_deadline_in_the_past_returns_at_once() {
    let began = Instant::now();
    sleep_until_ns(monotonic_ns() - 10_000_000_000);
    sleep_until_ns(i64::MIN);
    assert!(began.elapsed().as_millis() < 1_000);
}

/// A sleep to a deadline ahead waits for it: the call lasts at least the
/// distance to the deadline.
#[test]
fn a_sleep_lasts_at_least_the_distance_to_its_deadline() {
    let began = Instant::now();
    sleep_until_ns(monotonic_ns() + 30_000_000);
    assert!(began.elapsed().as_millis() >= 30);
}

/// The relative path, which the absolute one falls back to and every target
/// without an absolute sleep takes, never wakes early and returns at once for
/// a deadline in the past.
#[test]
fn the_relative_sleep_never_returns_before_its_deadline() {
    let deadline = monotonic_ns() + 15_000_000;
    super::relative::sleep_until_ns(deadline);
    assert!(monotonic_ns() >= deadline);
    let began = Instant::now();
    super::relative::sleep_until_ns(i64::MIN);
    assert!(began.elapsed().as_millis() < 1_000);
}
