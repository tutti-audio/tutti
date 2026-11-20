//! A native target's background threads: one to four by the core count.

use std::io;
use std::num::NonZeroUsize;

use tutti_platform::{Executor, Threads};

/// The most threads a session starts for its background work.
const MAX_THREADS: usize = 4;

/// What drives the background queue between updates.
pub(crate) struct Drive {
    threads: Option<Threads>,
}

impl Drive {
    /// Threads that drive `executor`, one to four by `available_parallelism`.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start a thread.
    pub(crate) fn background(executor: &Executor) -> io::Result<Self> {
        let n = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, MAX_THREADS);
        let n = NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            threads: Some(executor.spawn_threads(n)?),
        })
    }

    /// Nothing but `update` drives it.
    pub(crate) fn update() -> Self {
        Self { threads: None }
    }

    /// Whether threads run the queue, so `update` leaves it alone.
    pub(crate) fn runs_itself(&self) -> bool {
        self.threads.is_some()
    }
}
