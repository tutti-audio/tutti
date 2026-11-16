use std::cell::Cell;

thread_local! {
    /// A stepped clock for this thread, if a test asked: the next read
    /// and the step each read advances it by.
    static CLOCK: Cell<Option<(std::time::Instant, std::time::Duration)>> =
        const { Cell::new(None) };
}

/// Replace this thread's clock with one that advances `step` per read,
/// so a wait's yield (or a helper's leaving) depends on how many waits
/// it made and not on how long they took.
pub(crate) fn step_clock(step: std::time::Duration) {
    CLOCK.with(|c| c.set(Some((std::time::Instant::now(), step))));
}

pub(super) fn stepped_now() -> Option<std::time::Instant> {
    CLOCK.with(|c| {
        let (t, step) = c.get()?;
        c.set(Some((t + step, step)));
        Some(t)
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod yielding;

mod helper {
    use super::super::*;
    use super::*;

    /// With the clock stepping a quarter of the budget per read, a
    /// helper spins `5 * CLOCK_EVERY - 1` waits and leaves on the next,
    /// and a task taken in between starts the budget again. The budget is
    /// the one the wait was given, not the default.
    #[test]
    fn a_helper_leaves_once_the_clock_passes_its_budget() {
        let budget = std::time::Duration::from_micros(40);
        step_clock(budget / 4);
        let at = 5 * CLOCK_EVERY;
        let mut w = SpinThenLeave::new(budget);
        for _ in 0..2 {
            for i in 0..at - 1 {
                assert!(w.wait(), "inside the budget it spins (wait {i})");
            }
            assert!(!w.wait(), "then it leaves");
            w.reset();
        }
    }

    /// A helper whose clock never passes the budget never leaves,
    /// however many waits it makes: the budget is time, not a count.
    #[test]
    fn a_helper_inside_its_budget_never_leaves() {
        step_clock(std::time::Duration::ZERO);
        let mut w = SpinThenLeave::new(BlockWaits::DEFAULT.helper_leave_after);
        for _ in 0..100 * CLOCK_EVERY {
            assert!(w.wait());
        }
    }
}
