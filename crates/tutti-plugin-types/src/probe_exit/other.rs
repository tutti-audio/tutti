//! A platform without signals: a process ends with a status code only.

use std::process::ExitStatus;

pub(super) fn signal(_: ExitStatus) -> Option<i32> {
    None
}

/// A status as the platform reads it: Windows' large codes (an access violation
/// is `0xC0000005`) in hex, where a negative decimal says nothing.
pub(super) fn status_text(status: i32) -> String {
    format!("{status:#010X}")
}
