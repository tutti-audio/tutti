//! [`Backoff`]: how a control thread waits for a word that another thread
//! changes, without a lock and without the other side signalling.

#[cfg(not(loom))]
use std::time::Duration;

/// Steps spent spinning before the first yield.
#[cfg(not(loom))]
const SPIN_STEPS: u32 = 64;
/// Steps spent yielding before the first sleep.
#[cfg(not(loom))]
const YIELD_STEPS: u32 = 128;
/// The longest sleep: the wake-up latency of a wait that outlasts a block.
#[cfg(not(loom))]
const MAX_SLEEP: Duration = Duration::from_millis(1);

/// A wait that spins, then yields, then sleeps for twice as long each step up
/// to [`MAX_SLEEP`]. Control-side only: the sleep blocks. Under `loom` every
/// step is a yield, which schedules another model thread.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    #[cfg(not(loom))]
    step: u32,
}

impl Backoff {
    /// A wait at its first step.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Waits one step.
    #[cfg(not(loom))]
    pub(crate) fn snooze(&mut self) {
        if self.step < SPIN_STEPS {
            std::hint::spin_loop();
        } else if self.step < SPIN_STEPS + YIELD_STEPS {
            std::thread::yield_now();
        } else {
            let doublings = (self.step - SPIN_STEPS - YIELD_STEPS).min(5);
            std::thread::sleep((Duration::from_micros(32 << doublings)).min(MAX_SLEEP));
        }
        self.step = self.step.saturating_add(1);
    }

    /// Waits one step.
    #[cfg(loom)]
    pub(crate) fn snooze(&mut self) {
        loom::thread::yield_now();
    }
}
