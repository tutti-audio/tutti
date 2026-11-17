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

thread_local! {
    /// Where this thread's stepped-clock reads are counted, if a test asked.
    static READS: Cell<Option<&'static std::sync::atomic::AtomicU32>> =
        const { Cell::new(None) };
}

/// Count this thread's stepped-clock reads into `counter` (a test's own
/// static), so another thread can wait for this one to have made that many
/// waits instead of sleeping.
pub(crate) fn count_clock_reads_into(counter: &'static std::sync::atomic::AtomicU32) {
    READS.with(|r| r.set(Some(counter)));
}

pub(super) fn stepped_now() -> Option<std::time::Instant> {
    CLOCK.with(|c| {
        let (t, step) = c.get()?;
        c.set(Some((t + step, step)));
        READS.with(|r| {
            if let Some(counter) = r.get() {
                counter.fetch_add(1, std::sync::atomic::Ordering::Release);
            }
        });
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

    /// The clock is read once per `CLOCK_EVERY` waits and not before, so a
    /// count of reads is a count of waits.
    #[test]
    fn the_clock_is_read_once_per_clock_every_waits() {
        static READ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        count_clock_reads_into(&READ);
        step_clock(std::time::Duration::ZERO);
        let mut w = SpinThenLeave::new(BlockWaits::DEFAULT.helper_leave_after);
        let reads = || READ.load(std::sync::atomic::Ordering::Acquire);
        for _ in 0..CLOCK_EVERY - 1 {
            assert!(w.wait());
        }
        assert_eq!(reads(), 0, "no read before `CLOCK_EVERY` waits");
        assert!(w.wait());
        assert_eq!(reads(), 1, "the first read starts the clock");
        for _ in 0..3 * CLOCK_EVERY {
            assert!(w.wait());
        }
        assert_eq!(reads(), 4, "then one read per `CLOCK_EVERY` waits");
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
