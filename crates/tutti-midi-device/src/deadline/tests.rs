use std::time::{Duration, Instant};

use super::{Pending, Retry};

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

#[test]
fn a_retry_is_due_one_timeout_after_the_last_send() {
    let t0 = Instant::now();
    let mut retry = Retry::first(t0);
    assert!(!retry.due(t0 + SECOND - Duration::from_nanos(1), SECOND));
    assert!(retry.due(t0 + SECOND, SECOND));
    retry.resend(t0 + SECOND);
    assert!(!retry.due(t0 + SECOND * 2 - Duration::from_nanos(1), SECOND));
    assert!(retry.due(t0 + SECOND * 2, SECOND));
    assert_eq!(retry.deadline(SECOND), t0 + SECOND * 2);
}

#[test]
fn a_retry_is_spent_after_max_sends_the_first_included() {
    let t0 = Instant::now();
    let mut retry = Retry::first(t0);
    assert!(!retry.spent(2));
    retry.resend(t0);
    assert!(retry.spent(2));
    assert!(!retry.spent(3));
    retry.resend(t0);
    assert!(retry.spent(3));
}

#[test]
fn a_retry_is_due_at_an_earlier_now_than_its_send_never() {
    let t0 = Instant::now() + SECOND;
    let retry = Retry::first(t0);
    assert!(!retry.due(t0 - SECOND, SECOND));
}
