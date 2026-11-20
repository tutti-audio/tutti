//! A native target's background threads.

use std::io;
use std::num::NonZeroUsize;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::Background;

/// How long a wait blocks on the arrivals when it has no task to run.
const WAIT_SLICE: Duration = Duration::from_millis(1);

/// Blocks until `rx` has something or a millisecond passes.
pub(crate) fn wait<T>(rx: &Receiver<T>) -> Option<T> {
    rx.recv_timeout(WAIT_SLICE).ok()
}

/// The threads a [`Background`] started, each group stopped and joined on drop.
#[derive(Debug)]
pub(super) struct Threads(Vec<tutti_platform::Threads>);

impl Threads {
    /// No thread yet.
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    /// Whether none was started.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Background {
    /// Starts `n` threads that run the background queue until this `Background` drops.
    /// Each fires the due sleeps, runs a ready task, and parks until one is scheduled.
    /// A second call adds threads.
    ///
    /// ```no_run
    /// # use tutti_session::Session;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let (session, mut background) = Session::builder().build()?;
    /// background.spawn_threads(std::thread::available_parallelism()?)?;
    /// # let _ = session;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// The OS's refusal to start a thread.
    pub fn spawn_threads(&mut self, n: NonZeroUsize) -> io::Result<()> {
        let started = self.executor.spawn_threads(n)?;
        self.threads.0.push(started);
        Ok(())
    }
}
