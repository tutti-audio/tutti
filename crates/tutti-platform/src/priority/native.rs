//! Every target with an OS thread-priority call: the request is converted to
//! the `thread_priority` value the platform's own rule maps, applied to the
//! calling thread, and read back.

use super::{PriorityOutcome, PriorityRequest, Scheduling};

/// What each request asks the OS for, per platform, as the `thread_priority`
/// value it is converted from (a pure function, so the mapping is tested
/// without the privilege to apply it).
///
/// | | `Highest` | `Raised` |
/// |---|---|---|
/// | Linux | nice -20 | nice -10 |
/// | Windows | `THREAD_PRIORITY_HIGHEST` | `THREAD_PRIORITY_ABOVE_NORMAL` |
/// | macOS, iOS | the `SCHED_OTHER` maximum | a quarter of the way down from it to the minimum |
///
/// macOS's time-sharing priority is a number in a small range (about 15 to
/// 47, default 31), so a cross-platform percentage would land outside it
/// and be refused; there the point is computed from
/// `sched_get_priority_min/max`.
fn wanted(request: PriorityRequest) -> thread_priority::ThreadPriority {
    match request {
        PriorityRequest::Highest => thread_priority::ThreadPriority::Max,
        PriorityRequest::Raised => raised::raised(),
    }
}

#[cfg_attr(any(target_os = "macos", target_os = "ios"), path = "apple.rs")]
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), path = "cross.rs")]
mod raised;

#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(not(unix), path = "other.rs")]
mod scheduling_class;

pub(super) fn request(request: PriorityRequest) -> PriorityOutcome {
    use thread_priority::set_current_thread_priority;
    match set_current_thread_priority(wanted(request)) {
        Ok(()) => PriorityOutcome::Granted {
            requested: request,
            scheduling: scheduling_class::scheduling(),
        },
        Err(e) => PriorityOutcome::Refused {
            requested: request,
            error: format!("{e:?}"),
        },
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "ios")))]
#[path = "apple_tests.rs"]
mod apple_tests;
#[cfg(all(test, target_os = "linux"))]
#[path = "linux_tests.rs"]
mod linux_tests;
#[cfg(all(test, windows))]
#[path = "windows_tests.rs"]
mod windows_tests;
