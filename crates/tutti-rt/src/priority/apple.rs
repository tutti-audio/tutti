//! macOS and iOS: `Raised` is a point computed from the `SCHED_OTHER` range.

pub(super) fn raised() -> thread_priority::ThreadPriority {
    use thread_priority::{
        NormalThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };
    let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
    let (Ok(min), Ok(max)) = (
        ThreadPriority::min_value_for_policy(policy),
        ThreadPriority::max_value_for_policy(policy),
    ) else {
        return ThreadPriority::Max;
    };
    let point = max - (max - min) / 4;
    u8::try_from(point)
        .ok()
        .and_then(|p| ThreadPriorityValue::try_from(p).ok())
        .map_or(ThreadPriority::Max, ThreadPriority::Crossplatform)
}
