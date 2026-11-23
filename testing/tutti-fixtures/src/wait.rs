//! A bounded wait for a condition another thread makes true.

use std::time::Duration;

use tutti_platform::Instant;

/// The deadline a test gives a wait that only guards a hang: generous, so a loaded
/// machine never trips it, and never a claim about speed.
pub const HANG_GUARD: Duration = Duration::from_secs(60);

/// How long [`wait_until`] sleeps between looks at the condition.
const POLL: Duration = Duration::from_millis(1);

/// Looks at `condition` every millisecond until it holds, for at most `deadline`.
///
/// The condition is looked at once more at the deadline, so a condition that
/// turns true as the time runs out is not lost to the sleep before it.
///
/// # Panics
///
/// If `condition` still does not hold at `deadline`, naming `what` was waited for.
pub fn wait_until(what: &str, deadline: Duration, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < deadline,
            "timed out after {deadline:?} waiting for {what}"
        );
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A condition another thread makes true ends the wait, well inside the deadline.
    #[test]
    fn a_condition_set_by_another_thread_ends_the_wait() {
        let flag = AtomicBool::new(false);
        let start = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                flag.store(true, Ordering::Release);
            });
            wait_until("the flag", Duration::from_secs(30), || {
                flag.load(Ordering::Acquire)
            });
        });
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// A condition that already holds is not slept on.
    #[test]
    fn a_true_condition_returns_without_a_sleep() {
        let mut looks = 0;
        wait_until("nothing", Duration::ZERO, || {
            looks += 1;
            true
        });
        assert_eq!(looks, 1);
    }

    /// A condition that never holds fails at the deadline and names what it waited for.
    #[test]
    fn a_condition_that_never_holds_fails_naming_it() {
        let panic = std::panic::catch_unwind(|| {
            wait_until("the moon", Duration::from_millis(20), || false);
        })
        .expect_err("an unmet condition must panic");
        let message = panic
            .downcast_ref::<String>()
            .expect("a formatted panic message");
        assert!(message.contains("the moon"), "{message}");
        assert!(message.contains("timed out"), "{message}");
    }
}
