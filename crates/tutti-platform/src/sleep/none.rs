//! A target with no absolute sleep: every call is refused, and the caller
//! sleeps the time left.

pub(super) fn sleep_until_ns(_deadline_ns: i64) -> Result<(), ()> {
    Err(())
}
