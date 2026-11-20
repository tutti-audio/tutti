//! The web: a page cannot block, and std's sleep panics there, so the call returns at
//! once.

pub(super) fn sleep_until_ns(_deadline_ns: i64) -> Result<(), ()> {
    Ok(())
}
