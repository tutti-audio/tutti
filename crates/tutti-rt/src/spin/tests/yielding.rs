use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use super::super::*;
use super::*;

thread_local! {
    /// Where this thread's caller-side yields are counted, if a test asked.
    static YIELDS: Cell<Option<&'static AtomicU32>> = const { Cell::new(None) };
}

/// Count this thread's caller-side yields into `counter` (a test's own
/// static), so another thread can see that this one yielded.
pub(crate) fn count_yields_into(counter: &'static AtomicU32) {
    YIELDS.with(|y| y.set(Some(counter)));
}

pub(in crate::spin) fn count_yield() {
    YIELDS.with(|y| {
        if let Some(c) = y.get() {
            c.fetch_add(1, Ordering::Release);
        }
    });
}

/// With the clock stepping a quarter of the budget per read, the
/// first read starts the clock and the fifth reaches the budget: the
/// wait yields on its `5 * CLOCK_EVERY`th wait without progress, and
/// not before. Progress restarts the clock. The budget is the one the
/// wait was given, not the default.
#[test]
fn yields_once_the_clock_passes_the_budget() {
    static N: AtomicU32 = AtomicU32::new(0);
    count_yields_into(&N);
    let budget = std::time::Duration::from_millis(1);
    step_clock(budget / 4);
    let at = 5 * CLOCK_EVERY;
    let mut w = SpinThenYield::new(7, budget);
    for _ in 0..at - 1 {
        w.wait(7);
    }
    assert_eq!(N.load(Ordering::Relaxed), 0, "inside the budget it spins");
    w.wait(7);
    assert_eq!(N.load(Ordering::Relaxed), 1, "then one yield");
    for _ in 0..at - 1 {
        w.wait(7);
    }
    // Progress: the clock restarts, so a full budget spins again.
    w.wait(6);
    for _ in 0..at - 2 {
        w.wait(6);
    }
    assert_eq!(N.load(Ordering::Relaxed), 1, "progress restarts the clock");
    w.wait(6);
    assert_eq!(N.load(Ordering::Relaxed), 2);
}

/// A wait whose clock never passes the budget never yields, however
/// many waits it makes: the budget is time, not a count.
#[test]
fn a_wait_inside_the_budget_never_yields() {
    static N: AtomicU32 = AtomicU32::new(0);
    count_yields_into(&N);
    step_clock(std::time::Duration::ZERO);
    let mut w = SpinThenYield::new(7, BlockWaits::DEFAULT.caller_yield_after);
    for _ in 0..100 * CLOCK_EVERY {
        w.wait(7);
    }
    assert_eq!(N.load(Ordering::Relaxed), 0);
}
