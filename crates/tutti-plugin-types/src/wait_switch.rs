//! [`WaitSwitch`] and [`WaitArmed`]: the two ends of an out-of-process fork's
//! per-chunk wait.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The writing end of an out-of-process fork's per-chunk wait: whether the
/// fork's pipeline waits for each chunk.
///
/// The wait is how the node drives its pipeline, not a setting of the plugin:
/// the plugin's render mode is its own (what an app sets through the handle),
/// and telling it offline never makes a block wait. The switch is the fork's
/// node's alone. It is made with the reader it drives
/// ([`pair`](Self::pair)), so no second writer can exist for a reader, and it
/// is not `Clone`, so the one the node is handed is the only one. The node's
/// `prepare` arms it for a graph prepared offline and disarms it for any
/// other, on every prepare, so the audio thread of a realtime graph never
/// waits. The pipeline reads the other end ([`WaitArmed`]) on every chunk.
///
/// ```compile_fail
/// let (switch, _armed) = tutti_plugin_types::WaitSwitch::pair();
/// let _second_writer = switch.clone();
/// ```
///
/// ```compile_fail
/// // The writer cannot be made without its reader.
/// let _ = tutti_plugin_types::WaitSwitch::default();
/// ```
#[derive(Debug)]
pub struct WaitSwitch(Arc<AtomicBool>);

/// The reading end of a [`WaitSwitch`]: whether the pipeline waits for each
/// chunk. It cannot write.
#[derive(Debug)]
pub struct WaitArmed(Arc<AtomicBool>);

impl WaitSwitch {
    /// A disarmed switch and the reader it drives.
    #[must_use]
    pub fn pair() -> (WaitSwitch, WaitArmed) {
        let cell = Arc::new(AtomicBool::new(false));
        (WaitSwitch(Arc::clone(&cell)), WaitArmed(cell))
    }

    /// Arms the wait for an offline graph's prepare and disarms it for a
    /// realtime one.
    pub fn arm(&self, offline: bool) {
        self.0.store(offline, Ordering::Release);
    }
}

impl WaitArmed {
    /// Whether the pipeline waits for each chunk.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader follows the last arming of its one writer.
    #[test]
    fn the_reader_follows_its_writer() {
        let (switch, armed) = WaitSwitch::pair();
        assert!(!armed.armed(), "disarmed until a prepare arms it");
        switch.arm(true);
        assert!(armed.armed());
        switch.arm(false);
        assert!(!armed.armed());
    }

    /// Two pairs are independent: a writer drives only its own reader.
    #[test]
    fn a_writer_drives_only_its_own_reader() {
        let (first, first_armed) = WaitSwitch::pair();
        let (_second, second_armed) = WaitSwitch::pair();
        first.arm(true);
        assert!(first_armed.armed());
        assert!(!second_armed.armed());
    }
}
