//! [`WaitSwitch`]: the node's switch for an out-of-process fork's per-chunk
//! wait.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether an out-of-process fork's pipeline waits for each chunk.
///
/// The wait is how the node drives its pipeline, not a setting of the plugin:
/// the plugin's render mode is its own (what an app sets through the handle),
/// and telling it offline never makes a block wait. The only writer is the
/// node's `prepare`, which arms the switch for a graph prepared offline and
/// disarms it for any other, on every prepare, so the audio thread of a
/// realtime graph never waits. The pipeline reads it on every chunk.
///
/// A clone is another end of the same switch. It starts disarmed.
#[derive(Clone, Debug, Default)]
pub struct WaitSwitch(Arc<AtomicBool>);

impl WaitSwitch {
    /// A disarmed switch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms the wait for an offline graph's prepare and disarms it for a
    /// realtime one.
    pub fn arm(&self, offline: bool) {
        self.0.store(offline, Ordering::Release);
    }

    /// Whether the pipeline waits for each chunk.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch follows the last prepare, from both ends.
    #[test]
    fn the_switch_follows_the_last_arming_on_every_clone() {
        let switch = WaitSwitch::new();
        let other = switch.clone();
        assert!(!other.armed(), "disarmed until a prepare arms it");
        switch.arm(true);
        assert!(other.armed());
        switch.arm(false);
        assert!(!other.armed());
    }
}
