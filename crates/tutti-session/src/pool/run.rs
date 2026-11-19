//! Running the pool's loads: the worker stages the spawner takes, the stages
//! on the main thread and on the session's own, and the landings.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_platform::MainThreadMarker;

use crate::jobs::panic_message;
use crate::pool::entry::{LoadError, Step, Value};
use crate::pool::open::OpenCx;
use crate::pool::table::{Opening, Start, Worker};
use crate::{Session, WorkerError};

/// A panic in a stage the session runs itself, as a load's failure.
fn panicked(payload: &(dyn std::any::Any + Send)) -> LoadError {
    LoadError::Job(WorkerError::Panicked {
        message: panic_message(payload),
    })
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Hands what the diff started to the stage it begins with.
    pub(crate) fn pool_start(&mut self, starts: Vec<Start<C::Float>>) {
        for Start { flight, load } in starts {
            match load.step {
                Step::Ready(value) => self.pool.entries.land(flight, Ok(value)),
                Step::Session(open) => self.pool.entries.open(Opening { flight, open }),
                Step::Worker(work) => self.pool.entries.enqueue(flight, work),
            }
        }
        self.pool_pump();
    }

    /// Starts queued worker stages while fewer than the configured number run.
    /// Each landing starts the next.
    pub(crate) fn pool_pump(&mut self) {
        while let Some(Worker {
            flight,
            work,
            asset,
        }) = self.pool.entries.next_worker()
        {
            let defer = self.plugins.main_deferral();
            // A main stage the worker made drops its output on the main thread
            // wherever the stage itself is dropped: the jobs' channel, a session
            // that ends.
            // The identity is taken before the load reads the asset, so a change
            // during the read shows at the next refresh.
            self.jobs.submit(
                move || {
                    let loaded_from = asset.and_then(|asset| asset.identity());
                    (loaded_from, work().map(|staged| staged.deferring(defer)))
                },
                move |session, done, _| {
                    let (loaded_from, done) = match done {
                        Ok((loaded_from, done)) => (loaded_from, done),
                        Err(failed) => (None, Err(LoadError::Job(failed))),
                    };
                    session.land_pool_load(flight, loaded_from, done);
                },
            );
        }
    }

    /// Runs the stages that wait for the main thread, on it. A stage whose
    /// load was cancelled or superseded since its worker ended is not run: its
    /// worker output is dropped here, on `main`.
    pub(crate) fn pool_main_stages(&mut self, main: MainThreadMarker) {
        for item in self.pool.entries.take_main(main) {
            if self.pool.entries.live(item.flight) {
                let done = catch_unwind(AssertUnwindSafe(|| item.stage.run(main)))
                    .unwrap_or_else(|payload| Err(panicked(payload.as_ref())));
                self.pool.entries.land(item.flight, done);
            } else {
                item.stage.discard(main);
                self.pool.entries.end_flight(item.flight);
            }
        }
    }

    /// Runs the stages that wait on the session's own thread.
    fn pool_openings(&mut self) {
        for Opening { flight, open } in self.pool.entries.take_openings() {
            if !self.pool.entries.live(flight) {
                self.pool.entries.end_flight(flight);
                continue;
            }
            let done: Result<Value, LoadError> =
                catch_unwind(AssertUnwindSafe(|| open(&mut OpenCx::new(self))))
                    .unwrap_or_else(|payload| Err(panicked(payload.as_ref())));
            self.pool.entries.land(flight, done);
        }
    }

    /// What `update` does for the pool: runs the stages waiting for this
    /// thread, lands what came back, and reports the entries' outcomes since
    /// the last update.
    pub(crate) fn pool_events(
        &mut self,
        main: Option<MainThreadMarker>,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        if let Some(main) = main {
            self.pool_main_stages(main);
        }
        self.pool_openings();
        self.pool.entries.sweep();
        // Before the waits settle: a swap the graph refuses fails the entry the
        // wait would otherwise report as loaded.
        #[cfg(feature = "plugin")]
        <C::Float as crate::format::Sealed>::swap_pooled_plugins(self);
        self.pool.entries.poll_waiters();
        // One at a time, each over the pool as the run before it left it. A run
        // asked during these waits for the next update; one that panics leaves
        // the rest waiting.
        self.pool_run_loaded();
        self.pool.entries.drain_events(out);
    }

    /// Runs the runs asked with `when_loaded` whose entry has loaded.
    pub(crate) fn pool_run_loaded(&mut self) {
        let through = self.pool.entries.last_when();
        while let Some(then) = self.pool.entries.next_loaded(through) {
            then(self);
        }
    }
}
