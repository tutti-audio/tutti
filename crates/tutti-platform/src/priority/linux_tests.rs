use super::*;
use crate::request_priority;

/// The mapping, without privilege: what each request converts to for the
/// time-sharing policy. Linux: nice -10 and -20.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads /proc and calls the OS scheduler (sched_get_priority_*, setpriority), which miri isolation refuses"
)]
fn the_requests_map_to_nice_minus_10_and_minus_20() {
    use thread_priority::{NormalThreadSchedulePolicy, ThreadSchedulePolicy};
    let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
    let nice = |r| wanted(r).to_posix(policy).unwrap();
    assert_eq!(nice(PriorityRequest::Raised), -10);
    assert_eq!(nice(PriorityRequest::Highest), -20);
}

/// The nice value of the calling thread, from `/proc/thread-self/stat`
/// (field 19; `comm` is parenthesised and may hold spaces).
fn nice() -> i64 {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    // `rest` starts at field 3 (state).
    rest.split_whitespace()
        .nth(19 - 3)
        .unwrap()
        .parse()
        .unwrap()
}

fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().unwrap()
}

/// The outcome agrees with the OS: a grant of `Raised` leaves the thread
/// at nice -10 and time-sharing; a refusal leaves its nice untouched.
/// Unprivileged runs see the refusal arm, privileged ones the grant.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads /proc and calls the OS scheduler (sched_get_priority_*, setpriority), which miri isolation refuses"
)]
fn raised_reports_what_the_os_did() {
    let (outcome, before, after) = on_a_thread(|| {
        let before = nice();
        let outcome = request_priority(PriorityRequest::Raised);
        (outcome, before, nice())
    });
    match outcome {
        PriorityOutcome::Granted {
            requested,
            scheduling,
        } => {
            assert_eq!(requested, PriorityRequest::Raised);
            assert_eq!(scheduling, Scheduling::TimeSharing, "never real time");
            assert_eq!(after, -10);
        }
        PriorityOutcome::Refused { requested, .. } => {
            assert_eq!(requested, PriorityRequest::Raised);
            assert_eq!(after, before, "a refusal changes nothing");
        }
    }
}

/// `Highest` on a time-sharing thread is nice -20 when granted, and is
/// never reported as real time.
///
/// The unprivileged refusal arm checks only that the nice value did not move.
#[test]
#[cfg_attr(
    miri,
    ignore = "reads /proc and calls the OS scheduler (sched_get_priority_*, setpriority), which miri isolation refuses"
)]
fn highest_is_nice_minus_20_not_real_time() {
    let (outcome, before, after) = on_a_thread(|| {
        let before = nice();
        (request_priority(PriorityRequest::Highest), before, nice())
    });
    match outcome {
        PriorityOutcome::Granted { scheduling, .. } => {
            assert_eq!(scheduling, Scheduling::TimeSharing);
            assert_eq!(after, -20);
        }
        PriorityOutcome::Refused { .. } => assert_eq!(after, before),
    }
    assert!(!outcome.to_string().is_empty());
}
