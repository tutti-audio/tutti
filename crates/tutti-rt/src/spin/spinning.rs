//! The caller's wait where it only spins: every platform but Linux and
//! Android (see the module docs, "Only on Linux").

use std::hint::spin_loop;
use std::time::Duration;

/// A wait that only spins; the budget does not apply.
pub(crate) struct SpinThenYield {
    /// The counter's value at the last wait.
    seen: u32,
}

impl SpinThenYield {
    /// A wait on a counter now at `seen`; `budget` is ignored, as the caller
    /// never yields here.
    pub(crate) fn new(seen: u32, _budget: Duration) -> Self {
        Self { seen }
    }

    /// One step of the wait, given the counter's current value `now`.
    pub(crate) fn wait(&mut self, now: u32) {
        self.seen = now;
        spin_loop();
    }
}
