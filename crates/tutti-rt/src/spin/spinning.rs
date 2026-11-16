//! The caller's wait where it only spins: every platform but Linux and
//! Android (see the module docs, "Only on Linux").

use std::hint::spin_loop;
use std::time::Duration;

/// A wait that only spins; neither the counter nor the budget applies.
pub(crate) struct SpinThenYield;

impl SpinThenYield {
    /// A wait on a counter now at `_seen`; `_budget` is ignored, as the
    /// caller never yields here.
    pub(crate) fn new(_seen: u32, _budget: Duration) -> Self {
        Self
    }

    /// One step of the wait; the counter's value `_now` is not consulted.
    pub(crate) fn wait(&mut self, _now: u32) {
        spin_loop();
    }
}
