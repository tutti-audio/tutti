//! The session's background work: the executor its futures run on, the one thread that
//! drives it, and the platforms that have none. The platform's difference is this one
//! module's.
//!
//! The executor is the session's own, driven by the one thread tutti starts for it in
//! `build()`. That thread stops and joins when the session drops. The plugin loops run on
//! the executor's blocking pool, and a drop waits for them separately (see `plugin.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tutti_platform::{Executor, Task};
use tutti_rt::{Bell, Bells};

pub(crate) use os::{Running, THREADS, start};

#[cfg_attr(not(target_arch = "wasm32"), path = "background/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "background/web.rs")]
mod os;

/// Fails with [`Error::NeedsThreads`](crate::Error::NeedsThreads) naming `work` where
/// this platform starts no thread, so a build that needs it never half-starts.
///
/// # Errors
///
/// `work`, on a target that starts no thread.
pub(crate) fn require(work: crate::BackgroundWork) -> crate::Result<()> {
    if THREADS {
        Ok(())
    } else {
        Err(crate::Error::NeedsThreads(work))
    }
}

/// The session's background work: the executor and bells its futures run on, and the long
/// pumps (a take's) it waits for when it drops.
pub(crate) struct Background {
    /// Declared first: the thread that drives the queue stops and joins before the rest
    /// drops.
    _running: Running,
    /// Declared before the executor: the bells drop first, so their tick task ends
    /// through its own shutdown and runs its last pass, not stopped by the executor.
    bells: Bells,
    executor: Executor,
    /// The long-lived pumps and the bell each one awaits: the drop rings them and joins
    /// them, so what a pump owes at its end (a take's commit) is done first.
    pumps: Vec<(Task<()>, Bell)>,
    /// Set when the session drops: a pump that sees it stops writing what no one will read.
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Background")
            .field("pumps", &self.pumps.len())
            .finish_non_exhaustive()
    }
}

impl Background {
    pub(crate) fn new(executor: Executor, running: Running, bells: Bells) -> Self {
        Self {
            _running: running,
            bells,
            executor,
            pumps: Vec::new(),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ticks the bells: what a future awaiting one sent lands in the same update.
    pub(crate) fn poll(&self) {
        self.bells.tick();
    }

    /// The session's bells: a future that awaits the audio thread makes its bell here.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn bells(&self) -> &Bells {
        &self.bells
    }

    /// The flag the drop sets: the session is gone.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn closed(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    /// Spawns `future` on a thread of its own: work that runs as long as a take. It awaits
    /// `bell`: the session's drop sets [`closed`](Self::closed), rings it and waits for the
    /// future to end.
    ///
    /// # Errors
    ///
    /// The executor's refusal to start it. The future is not run.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn spawn_pump(
        &mut self,
        name: &str,
        bell: Bell,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let pump = self.executor.spawn_dedicated(name, None, future)?;
        self.pumps.retain(|(pump, _)| !pump.is_finished());
        self.pumps.push((pump, bell));
        Ok(())
    }
}

/// The longest the drop waits for the pumps to finish: a guard against a hang, not a
/// deadline; a pump that has the session closed and its bell rung ends within a block.
const PUMP_JOIN_BOUND: Duration = Duration::from_secs(10);

impl Drop for Background {
    /// Sets the closed flag, rings each pump's bell so it sees it and finishes (a take
    /// commits its file), and waits for each before the executor can go. On a thread that
    /// drives an executor a wait would stop the tasks it runs, so the pumps are detached
    /// there, unfinished.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        for (_, bell) in &self.pumps {
            bell.ring();
        }
        // Delivers the rings to the pumps' wakers now, not at the next tick.
        self.bells.tick();
        if tutti_platform::is_driving() {
            let unfinished = self
                .pumps
                .drain(..)
                .filter(|(pump, _)| !pump.is_finished())
                .map(|(pump, _)| pump.detach())
                .count();
            if unfinished > 0 {
                tracing::warn!(
                    "the session dropped on a thread that drives an executor, which cannot wait: \
                     {unfinished} take(s) are left unfinished and not committed by this drop"
                );
            }
            return;
        }
        for (pump, _) in self.pumps.drain(..) {
            if let Err(pump) = tutti_platform::join_within(pump, PUMP_JOIN_BOUND) {
                tracing::warn!(
                    "a pump did not finish within {PUMP_JOIN_BOUND:?} of the session's drop: \
                     what it owed (a take's file) is not committed"
                );
                pump.detach();
            }
        }
    }
}
