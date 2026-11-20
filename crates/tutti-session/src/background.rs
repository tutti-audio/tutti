//! [`Background`]: what runs the session's background work.
//!
//! The session queues its background work (a file load, a plugin round trip, a streamer's
//! refills, a take's pump) on an `tutti_platform::Executor` it keeps to itself.
//! `build` hands the app the other half: a `Background` that runs the queue, on threads
//! the app starts or on the calling thread. The platform's difference (a web page has no
//! threads) is the one `os` module pair's.

use std::fmt;
use std::time::Duration;

use tutti_platform::Executor;

use crate::jobs::Jobs;

#[cfg_attr(not(target_arch = "wasm32"), path = "background/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "background/web.rs")]
mod os;

/// The most tasks one blocking wait runs between two of its updates: a count, never a
/// time.
const TICKS_PER_SLICE: usize = 256;

/// How long a blocking wait sleeps on the arrivals when it has no task to run.
const WAIT_SLICE: Duration = Duration::from_millis(1);

/// Runs a session's background work: the half of `build`'s result that does the work the
/// [`Session`](crate::Session) queues.
///
/// Background work runs only while something drives it, and the session never drives it
/// itself. [`Session::update`](crate::Session::update) lands what finished and commits
/// edits; it runs nothing. Two drivers:
///
/// - **Threads** (native only): `spawn_threads` starts threads that run the queue for as
///   long as the `Background` lives. A second call adds threads.
/// - **The caller**: [`try_tick`](Self::try_tick) runs one ready task on the calling
///   thread, and [`run_until_stalled`](Self::run_until_stalled) runs until none is ready.
///   A web page, and a game's frame loop, drive this way.
///
/// Dropping the `Background` stops and joins its threads, and the work still queued is
/// dropped: its jobs end [`BackgroundError::Dropped`](crate::BackgroundError::Dropped).
/// The work of a plugin's round trip, a take's pump and the disk streamer has a thread
/// of its own on a native target and starts at once, driven or not; every other job
/// (a file load, a pool entry's stage) waits in the queue for a driver.
///
/// A blocking wait on background work ([`Job::wait`](crate::Job::wait)) takes the
/// `&mut Background`: with threads it blocks until the landing, and without them it
/// drives the queue itself, so no wait is left without a driver.
///
/// A native app starts threads once, after the build:
///
/// ```no_run
/// # use tutti_session::Session;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let (session, mut background) = Session::builder().build()?;
/// background.spawn_threads(std::thread::available_parallelism()?)?;
/// # let _ = session;
/// # Ok(())
/// # }
/// ```
///
/// A frame loop bounds its share of a frame by a count, so a long task never freezes it:
///
/// ```no_run
/// # use tutti_session::Session;
/// # fn budget_left() -> bool { true }
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let (mut session, mut background) = Session::builder().build()?;
/// loop {
///     while budget_left() && background.try_tick() {}
///     let _events = session.update();
///     // ... render the frame ...
/// #   break;
/// }
/// # Ok(())
/// # }
/// ```
///
/// [`run_until_stalled`](Self::run_until_stalled) is for offline renders and tests: a task
/// that keeps waking itself never lets it return.
#[must_use = "background work runs only while it is driven: keep the `Background`, and start threads or call `try_tick`"]
pub struct Background {
    /// Declared first: the threads stop and join before the executor drops.
    threads: os::Threads,
    executor: Executor,
}

impl fmt::Debug for Background {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Background")
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

impl Background {
    /// A driver over a new executor, with no thread started.
    pub(crate) fn new() -> Self {
        Self {
            threads: os::Threads::new(),
            executor: Executor::new(),
        }
    }

    /// The executor the session queues its work on.
    pub(crate) fn executor(&self) -> &Executor {
        &self.executor
    }

    /// Fires the due sleeps, then runs ready tasks on this thread until none is ready,
    /// and returns how many ran. Never blocks.
    ///
    /// A task that wakes itself runs again within the same call, so a caller that must
    /// bound its work loops [`try_tick`](Self::try_tick) instead.
    pub fn run_until_stalled(&mut self) -> usize {
        self.executor.run_until_stalled()
    }

    /// Fires the due sleeps, then runs one ready task if there is one, and returns whether
    /// it did. Never blocks.
    ///
    /// A frame bounds its share of work with it: `while budget_left() && background.try_tick() {}`.
    pub fn try_tick(&mut self) -> bool {
        self.executor.try_tick()
    }

    /// One step of a blocking wait: with threads started, blocks until a job sends
    /// something or a slice passes; without, runs up to [`TICKS_PER_SLICE`] ready tasks,
    /// and blocks the slice only when there was none to run.
    pub(crate) fn step(&mut self, jobs: &mut Jobs) {
        if self.threads.is_empty() {
            let mut ran = false;
            for _ in 0..TICKS_PER_SLICE {
                if !self.try_tick() {
                    break;
                }
                ran = true;
            }
            if ran {
                return;
            }
        }
        jobs.block(WAIT_SLICE);
    }
}
