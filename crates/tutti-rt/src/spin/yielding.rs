//! The caller's wait where it yields its core: Linux and Android, and loom
//! (whose budget is a count).

use std::time::Duration;

use crate::sync::{spin_loop, yield_now};
#[cfg(not(loom))]
use tutti_platform::Instant;

#[cfg(not(loom))]
use super::{CLOCK_EVERY, now};

/// Under loom: the caller's waits without progress per yield.
#[cfg(loom)]
const LOOM_CALLER_SPINS: u32 = 2;

/// A wait that spins, then yields its core after
/// [`BlockWaits::caller_yield_after`](super::BlockWaits::caller_yield_after)
/// without progress (on Linux). See the module docs (`src/spin.rs`).
pub(crate) struct SpinThenYield {
    /// The counter's value at the last wait.
    seen: u32,
    /// Waits since the last clock read (or, under loom, since progress or
    /// the last yield).
    spins: u32,
    /// When the counter was first seen unmoved, by the clock's reads; `None`
    /// until the first read after progress or a yield.
    #[cfg(not(loom))]
    since: Option<Instant>,
    /// How long the counter may stay unmoved.
    #[cfg(not(loom))]
    budget: Duration,
}

impl SpinThenYield {
    /// A wait on a counter now at `seen`, yielding after `budget` without
    /// progress (ignored under loom).
    pub(crate) fn new(seen: u32, budget: Duration) -> Self {
        #[cfg(loom)]
        let _ = budget;
        Self {
            seen,
            spins: 0,
            #[cfg(not(loom))]
            since: None,
            #[cfg(not(loom))]
            budget,
        }
    }

    /// One step of the wait, given the counter's current value `now`.
    pub(crate) fn wait(&mut self, now: u32) {
        if now != self.seen {
            self.seen = now;
            self.restart();
        }
        if self.due() {
            self.restart();
            #[cfg(all(test, not(loom)))]
            super::tests::yielding::count_yield();
            crate::scope::sanctioned(yield_now);
        } else {
            spin_loop();
        }
    }

    /// Start the budget again: after progress, or after a yield.
    fn restart(&mut self) {
        self.spins = 0;
        #[cfg(not(loom))]
        {
            self.since = None;
        }
    }

    /// Whether this wait spends the budget.
    #[cfg(loom)]
    fn due(&mut self) -> bool {
        self.spins += 1;
        self.spins >= LOOM_CALLER_SPINS
    }

    /// Whether this wait spends the budget: every `CLOCK_EVERY` waits, a
    /// clock read, and the first read after a restart starts the clock.
    #[cfg(not(loom))]
    fn due(&mut self) -> bool {
        self.spins += 1;
        if self.spins < CLOCK_EVERY {
            return false;
        }
        self.spins = 0;
        let t = now();
        match self.since {
            None => {
                self.since = Some(t);
                false
            }
            Some(s) => t.saturating_duration_since(s) >= self.budget,
        }
    }
}
