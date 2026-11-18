//! The bell where the main loop's wait has no descriptor to watch (Windows,
//! and the fallback platforms): a flag. The loop's wait naps in slices of at
//! most 1 ms and checks [`Bell::rung`] between them, so a ring ends the wait
//! within a millisecond.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The server's bell. See the module docs.
#[derive(Clone)]
pub(crate) struct Bell {
    rung: Arc<AtomicBool>,
}

impl Bell {
    /// A bell, built on the thread that owns the server.
    ///
    /// # Errors
    ///
    /// Never on this platform.
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self {
            rung: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Makes the main loop's wait end. Any thread, the audio thread included.
    pub(crate) fn ring(&self) {
        self.rung.store(true, Ordering::Release);
    }

    /// Whether it rang since the last [`take`](Self::take): what the wait
    /// checks between naps.
    pub(crate) fn rung(&self) -> bool {
        self.rung.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` (`None`: for ever) for a ring and clears it:
    /// whether one came, napping at most 1 ms between looks.
    pub(crate) fn wait(&self, timeout: Option<std::time::Duration>) -> bool {
        const NAP: std::time::Duration = std::time::Duration::from_millis(1);
        let started = std::time::Instant::now();
        while !self.rung() {
            let left = timeout.map(|t| t.saturating_sub(started.elapsed()));
            if left.is_some_and(|left| left.is_zero()) {
                return false;
            }
            std::thread::sleep(left.map_or(NAP, |left| left.min(NAP)));
        }
        self.take()
    }

    /// Clears a ring this thread has woken for.
    pub(crate) fn clear(&self) {
        self.take();
    }

    /// Whether it rang since the last call, clearing it.
    pub(crate) fn take(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }
}
