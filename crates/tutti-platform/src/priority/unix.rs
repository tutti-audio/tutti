//! Unix: the scheduling class is read back from the thread's policy.

use super::Scheduling;

pub(super) fn scheduling() -> Scheduling {
    match thread_priority::thread_schedule_policy() {
        Ok(thread_priority::ThreadSchedulePolicy::Realtime(_)) => Scheduling::Realtime,
        _ => Scheduling::TimeSharing,
    }
}
