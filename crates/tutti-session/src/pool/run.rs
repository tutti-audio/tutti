//! Running the pool's loads: the worker stages the spawner takes, the stages
//! on the main thread and on the session's own, and the landings.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_rt::MainThreadMarker;

use crate::jobs::panic_message;
use crate::pool::entry::{Defer, LoadError, Staged, Step, Value};
use crate::pool::open::OpenCx;
use crate::pool::table::{Opening, Start};
use crate::{JobFailed, Session};

/// A panic in a stage the session runs itself, as a load's failure.
fn panicked(payload: &(dyn std::any::Any + Send)) -> LoadError {
    LoadError::Job(JobFailed::Panicked {
        message: panic_message(payload),
    })
}

impl<C: crate::Clock> Session<C> {
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
    fn pool_pump(&mut self) {
        while let Some((flight, work)) = self.pool.next_worker() {
            let defer = self.main_deferral();
            // A main stage the worker made drops its output on the main thread
            // wherever the stage itself is dropped: the jobs' channel, a session
            // that ends.
            self.jobs.submit(
                move || work().map(|staged| staged.deferring(defer)),
                move |session, done, _| {
                    session.pool.worker_ended();
                    match done.unwrap_or_else(|failed| Err(LoadError::Job(failed))) {
                        Ok(Staged::Done(value)) => session.pool.land(flight, Ok(value)),
                        Ok(Staged::Main(stage)) => session.pool.stage_main(flight, stage),
                        Err(error) => session.pool.land(flight, Err(error)),
                    }
                    session.pool_pump();
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
    pub(crate) fn pool_events(&mut self, out: &mut Vec<crate::SessionEvent>) {
        if let Some(main) = MainThreadMarker::new() {
            self.pool_main_stages(main);
        }
        self.pool_openings();
        self.pool.sweep();
        self.pool.poll_waiters();
        self.pool.drain_events(out);
    }
}
