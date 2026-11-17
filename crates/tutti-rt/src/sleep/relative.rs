//! The sleep of a target whose std sleeps on a timer: the time left until the
//! deadline, re-read until the deadline has passed.

use std::time::Duration;

use crate::monotonic_ns;

/// The time from `now_ns` to `deadline_ns`; `None` once the deadline has
/// been reached.
pub(super) fn left(deadline_ns: i64, now_ns: i64) -> Option<Duration> {
    match u64::try_from(deadline_ns.saturating_sub(now_ns)) {
        Ok(0) | Err(_) => None,
        Ok(ns) => Some(Duration::from_nanos(ns)),
    }
}

pub(super) fn sleep_until_ns(deadline_ns: i64) {
    while let Some(left) = left(deadline_ns, monotonic_ns()) {
        std::thread::sleep(left);
    }
}
