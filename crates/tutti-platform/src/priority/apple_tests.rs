use super::*;

/// macOS: `Raised` lies strictly between the default-ish middle and the
/// maximum of `SCHED_OTHER`, `Highest` is the maximum. Compiled only
/// there; not run in this lane.
#[test]
fn raised_is_between_the_middle_and_the_maximum() {
    use thread_priority::{NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy};
    let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
    let max = ThreadPriority::max_value_for_policy(policy).unwrap();
    let min = ThreadPriority::min_value_for_policy(policy).unwrap();
    let raised = wanted(PriorityRequest::Raised).to_posix(policy).unwrap();
    assert!((min + max) / 2 < raised && raised < max);
    assert_eq!(
        wanted(PriorityRequest::Highest).to_posix(policy).unwrap(),
        max
    );
}
