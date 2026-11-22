use std::time::Duration;
use tutti_platform::Instant;

use super::Retry;

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn a_retry_is_due_one_timeout_after_the_last_send() {
    let t0 = Instant::now();
    let mut retry = Retry::first(t0, SECOND);
    assert!(!retry.due(t0 + SECOND - Duration::from_nanos(1)));
    assert!(retry.due(t0 + SECOND));
    retry.resend(t0 + SECOND, SECOND);
    assert!(!retry.due(t0 + SECOND * 2 - Duration::from_nanos(1)));
    assert!(retry.due(t0 + SECOND * 2));
    assert_eq!(retry.deadline(), t0 + SECOND * 2);
}

#[test]
fn a_retry_is_spent_after_max_sends_the_first_included() {
    let t0 = Instant::now();
    let mut retry = Retry::first(t0, SECOND);
    assert!(!retry.spent(2));
    retry.resend(t0, SECOND);
    assert!(retry.spent(2));
    assert!(!retry.spent(3));
    retry.resend(t0, SECOND);
    assert!(retry.spent(3));
}

#[test]
fn a_retry_is_not_due_at_an_earlier_now_than_its_send() {
    let t0 = Instant::now() + SECOND;
    let retry = Retry::first(t0, SECOND);
    assert!(!retry.due(t0 - SECOND));
}
