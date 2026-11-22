use tutti_platform::Instant;

use super::*;
use tutti_ump::U7;

#[test]
fn the_wait_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: TimeoutWait::new,
        get: TimeoutWait::tenths,
        of: Some(TimeoutWait::of),
        valid: &[0, 1, 127],
        invalid: &[128, 255],
    });
}

#[test]
fn only_a_timeout_wait_ack_is_a_wait() {
    let wait = Ack::new(U7::of(0x34), AckStatus::TimeoutWait(TimeoutWait::of(30)));
    assert_eq!(TimeoutWait::from_ack(&wait), Some(TimeoutWait::of(30)));
    assert_eq!(
        TimeoutWait::from_ack(&Ack::new(U7::of(0x34), AckStatus::FlowControlNextChunks)),
        None
    );
}

// M2-103 §12.2.1, Example 1: a wait of 0x3C (6 s) is added to the 3 s in hand,
// 9 s in all; a second wait adds again, and a wait of 0 adds nothing.
/// Spec: m2-103/12.2.1#2
#[test]
fn a_wait_is_added_to_the_time_left() {
    let now = Instant::now();
    let mut wait = Deadline::after(now, Duration::from_secs(3));
    TimeoutWait::of(0x3C).extend(&mut wait);
    assert_eq!(wait.deadline(), now + Duration::from_secs(9));
    TimeoutWait::of(0x1E).extend(&mut wait);
    assert_eq!(wait.deadline(), now + Duration::from_secs(12));
    TimeoutWait::of(0).extend(&mut wait);
    assert_eq!(wait.deadline(), now + Duration::from_secs(12));
}
