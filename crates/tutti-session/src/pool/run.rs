//! Running the pool's loads: the worker stages the spawner takes, the stages
//! on the main thread and on the session's own, and the landings.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_rt::MainThreadMarker;

use crate::jobs::panic_message;
use crate::pool::entry::{Defer, LoadError, Step, Value};
use crate::pool::open::OpenCx;
use crate::pool::stamp::Stamp;
use crate::pool::table::{Opening, Start, Worker};
use crate::{Session, WorkerError};

/// A panic in a stage the session runs itself, as a load's failure.
fn panicked(payload: &(dyn std::any::Any + Send)) -> LoadError {
    LoadError::Job(WorkerError::Panicked {
        message: panic_message(payload),
    })
}

/// `f32` because a pooled entry is a plugin or a sampler voice opened through `OpenCx`, which is `f32`.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Hands what the diff started to the stage it begins with.
    pub(crate) fn pool_start(&mut self, starts: Vec<Start>) {
        for Start { flight, load } in starts {
            match load.step {
                Step::Ready(value) => self.pool.land(flight, Ok(value)),
                Step::Session(open) => self.pool.open(Opening { flight, open }),
                Step::Worker(work) => self.pool.enqueue(flight, work),
            }
        }
        self.pool_pump();
    }

    /// Starts queued worker stages while fewer than the configured number run.
    /// Each landing starts the next.
    pub(crate) fn pool_pump(&mut self) {
        while let Some(Worker { flight, work, file }) = self.pool.next_worker() {
            let defer = self.main_deferral();
            // A main stage the worker made drops its output on the main thread
            // wherever the stage itself is dropped: the jobs' channel, a session
            // that ends.
            // The stamp is taken before the load reads the file, so a change
            // during the read shows at the next refresh.
            self.jobs.submit(
                move || {
                    let stamp = file.and_then(|file| Stamp::of(&file).ok());
                    (stamp, work().map(|staged| staged.deferring(defer)))
                },
                move |session, done, _| {
                    let (stamp, done) = match done {
                        Ok((stamp, done)) => (stamp, done),
                        Err(failed) => (None, Err(LoadError::Job(failed))),
                    };
                    session.land_pool_load(flight, stamp, done);
                },
            );
        }
    }

    /// What a main stage dropped off the main thread hands its discard to: the
    /// session's main-work collector (feature `plugin`).
    fn main_deferral(&self) -> Option<Defer> {
        #[cfg(feature = "plugin")]
        {
            let work = self.work.clone();
            Some(std::sync::Arc::new(move |discard| work.defer(discard)))
        }
        #[cfg(not(feature = "plugin"))]
        {
            None
        }
    }

    /// Runs the stages that wait for the main thread, on it. A stage whose
    /// load was cancelled or superseded since its worker ended is not run: its
    /// worker output is dropped here, on `main`.
    pub(crate) fn pool_main_stages(&mut self, main: MainThreadMarker) {
        for item in self.pool.take_main(main) {
            if self.pool.live(item.flight) {
                let done = catch_unwind(AssertUnwindSafe(|| item.stage.run(main)))
                    .unwrap_or_else(|payload| Err(panicked(payload.as_ref())));
                self.pool.land(item.flight, done);
            } else {
                item.stage.discard(main);
                self.pool.end_flight(item.flight);
            }
        }
    }

    /// Runs the stages that wait on the session's own thread.
    fn pool_openings(&mut self) {
        for Opening { flight, open } in self.pool.take_openings() {
            if !self.pool.live(flight) {
                self.pool.end_flight(flight);
                continue;
            }
            let done: Result<Value, LoadError> =
                catch_unwind(AssertUnwindSafe(|| open(&mut OpenCx::new(self))))
                    .unwrap_or_else(|payload| Err(panicked(payload.as_ref())));
            self.pool.land(flight, done);
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
        self.pool.sweep();
        // Before the waits settle: a swap the graph refuses fails the entry the
        // wait would otherwise report as loaded.
        #[cfg(feature = "plugin")]
        self.swap_pooled_plugins();
        self.pool.poll_waiters();
        // One at a time, each over the pool as the run before it left it. A run
        // asked during these waits for the next update; one that panics leaves
        // the rest waiting.
        self.pool_run_loaded();
        self.pool.drain_events(out);
    }

    /// Runs the runs asked with `when_loaded` whose entry has loaded.
    pub(crate) fn pool_run_loaded(&mut self) {
        let through = self.pool.last_when();
        while let Some(then) = self.pool.next_loaded(through) {
            then(self);
        }
    }
}
