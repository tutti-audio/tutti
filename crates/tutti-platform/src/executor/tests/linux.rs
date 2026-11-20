use super::*;
use crate::{PriorityOutcome, PriorityRequest};

/// The nice value of the calling thread, from `/proc/thread-self/stat` (field 19;
/// `comm` is parenthesised and may hold spaces).
fn nice() -> i64 {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    rest.split_whitespace()
        .nth(19 - 3)
        .unwrap()
        .parse()
        .unwrap()
}

/// The request reaches the dedicated thread: granted, it is nice -10; refused, the
/// thread keeps the nice an unrequested dedicated thread has.
#[test]
#[cfg_attr(miri, ignore = "reads /proc and calls the OS scheduler")]
fn spawn_dedicated_passes_the_priority_request_to_its_thread() {
    let executor = Executor::new();
    let plain = wait(
        executor
            .spawn_dedicated("tutti-test-plain", None, async { nice() })
            .unwrap(),
    );
    let raised = wait(
        executor
            .spawn_dedicated("tutti-test-raised", Some(PriorityRequest::Raised), async {
                nice()
            })
            .unwrap(),
    );
    // What the OS grants is the host's choice (an unprivileged run is refused); the
    // thread's own outcome decides which arm holds.
    let outcome = thread::spawn(|| crate::request_priority(PriorityRequest::Raised))
        .join()
        .unwrap();
    match outcome {
        PriorityOutcome::Granted { .. } => assert_eq!(raised, -10),
        PriorityOutcome::Refused { .. } => assert_eq!(raised, plain),
    }
}
