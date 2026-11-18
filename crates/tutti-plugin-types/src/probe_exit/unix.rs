//! The signal a process died of, where the OS has signals.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

pub(super) fn signal(status: ExitStatus) -> Option<i32> {
    status.signal()
}

/// A status as the platform reads it: the number.
pub(super) fn status_text(status: i32) -> String {
    status.to_string()
}
