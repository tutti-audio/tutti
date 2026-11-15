//! The sleep of a target whose std sleeps on a timer: the time left until the
//! deadline, re-read until the deadline has passed.

use std::time::Duration;

use crate::monotonic_ns;

pub(super) fn sleep_until_ns(deadline_ns: i64) {
    loop {
        let left = deadline_ns.saturating_sub(monotonic_ns());
        let Ok(left) = u64::try_from(left) else {
            return;
        };
        if left == 0 {
            return;
        }
        std::thread::sleep(Duration::from_nanos(left));
    }
}
