//! The tick's body where something parks for it: the task on a native target, the
//! loom model's stepped `Ticker`.

use super::{Ordering, Parker, Shared, State};

impl State {
    /// Whether the tick has a waker to wake: rung, and someone waiting.
    fn wakeable(&self) -> bool {
        self.rung.load(Ordering::Acquire) && self.waiting.load(Ordering::Acquire)
    }
}

impl Shared {
    /// Some bell is rung and has a waiter: the tick task's pending check.
    pub(super) fn pending(&self) -> bool {
        self.bells().iter().any(|state| state.wakeable())
    }

    /// Park until a bell has a waiter to wake or the set is shut, then run
    /// one pass; `false` once shut, after the last pass. The tick task loops on it.
    pub(super) fn tick_once(&self, parker: &mut Parker) -> bool {
        parker.park(|| self.shut.load(Ordering::Acquire) || self.pending());
        if self.shut.load(Ordering::Acquire) {
            self.last_pass();
            return false;
        }
        self.pass();
        true
    }
}
