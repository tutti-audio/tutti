use std::time::Duration;
use tutti_platform::Instant;

use super::Pending;

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn a_wait_is_expired_at_its_deadline_and_not_before() {
    let t0 = Instant::now();
    let wait = Pending::after(t0, SECOND);
    assert!(!wait.expired(t0 + SECOND - Duration::from_nanos(1)));
    assert!(wait.expired(t0 + SECOND));
    assert!(wait.expired(t0 + SECOND * 2));
}

#[test]
fn extending_a_wait_adds_to_its_deadline() {
    let t0 = Instant::now();
    let mut wait = Pending::after(t0, SECOND);
    wait.extend(SECOND * 2);
    assert_eq!(wait.deadline(), t0 + SECOND * 3);
    wait.extend(Duration::ZERO);
    assert_eq!(wait.deadline(), t0 + SECOND * 3);
}

#[test]
fn restarting_a_wait_runs_the_timeout_from_the_new_now() {
    let t0 = Instant::now();
    let mut wait = Pending::after(t0, SECOND);
    wait.extend(SECOND);
    wait.restart(t0 + SECOND, SECOND);
    assert_eq!(wait.deadline(), t0 + SECOND * 2);
}

#[test]
fn the_earliest_of_the_waits_is_the_least_deadline_and_none_without_any() {
    let t0 = Instant::now();
    let waits = [
        Pending::after(t0, SECOND * 3),
        Pending::after(t0, SECOND),
        Pending::after(t0, SECOND * 2),
    ];
    assert_eq!(Pending::earliest(waits), Some(t0 + SECOND));
    assert_eq!(Pending::earliest([]), None);
}
