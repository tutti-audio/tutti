//! Linux, Windows and the other platforms: `Raised` is a cross-platform
//! value that `thread_priority` maps by the platform's own rule.

/// The cross-platform value (`0..=99`) `Raised` asks for on Linux and
/// Windows. thread-priority maps it by the platform's own rule: on Linux's
/// time-sharing policy a linear scale onto nice 19 to -20 (measured: 72 and
/// 73 are nice -10, 75 is -11); on Windows 61 to 79 is `ABOVE_NORMAL`.
const RAISED: u8 = 73;

pub(super) fn raised() -> thread_priority::ThreadPriority {
    use thread_priority::{ThreadPriority, ThreadPriorityValue};
    // `RAISED` is in range, so the fallback is unreachable; `Max` is the
    // nearest request if it ever were not.
    ThreadPriorityValue::try_from(RAISED).map_or(ThreadPriority::Max, ThreadPriority::Crossplatform)
}
