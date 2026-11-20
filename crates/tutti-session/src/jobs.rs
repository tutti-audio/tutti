//! The session's background work: a closure (a file load, a plugin round trip,
//! a recording's pump) wrapped in a future and spawned on the session's
//! [`Executor`], and what it reports lands on the control thread in
//! [`Session::update`], in the order it was sent.
//!
//! A job borrows nothing of the session. It reports through its
//! [`Landings`], which never blocks, and ends exactly once, after every
//! message it sent: with its result, or with a [`WorkerError`] when it
//! panicked or the executor dropped it unrun. A typed [`Job`]
//! (`job.rs`) is cancelled by its drop; a streaming job carries its own flag,
//! and [`Landings::is_closed`] tells it when the session is gone, so it
//! stops writing what no one will land.
//!
//! The executor is the builder's choice
//! ([`SessionBuilder::executor`](crate::SessionBuilder::executor)). Its threads are
//! its own: when the last clone of a threaded executor drops, it joins them, so a
//! job still running then holds the session's drop until its work returns. Dropping the
//! session joins in-flight jobs; every blocking plugin job is bounded by its own deadline
//! (`tutti_plugin_types::Timeouts`: the load, `param_wait` and `state_wait`), and a server
//! that dies mid-round-trip fails the wait at once.

#![cfg_attr(
    not(any(test, feature = "capture")),
    expect(
        dead_code,
        reason = "the take's pump (feature `capture`) is the streaming half's user"
    )
)]

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use tutti_platform::Executor;
use tutti_rt::{Bells, LockExt};

use crate::job::{Cancel, Job, JobKind, JobOutputQueue, Ticket};
use crate::{Clock, Session, SessionEvent, WorkerError};

/// Where a job runs on a threaded executor.
#[derive(Clone, Copy)]
enum Place<'a> {
    /// The shared pool, for a short job.
    Pool,
    /// A thread of its own, named: a job that blocks or runs as long as a take.
    Dedicated(&'a str),
}

/// The most futures one [`Session::update`] polls on a polled executor: a count,
/// never a time. A threaded executor polls on its own threads and ignores it;
/// under a polled one, work past it runs in the next update.
const POLLS_PER_UPDATE: usize = 256;

/// What a job's future owes the session if it ends without reporting: dropped,
/// the end it sends is [`WorkerError::Dropped`]. A future the executor drops
/// unrun (a shutdown, a host that discards it) sends it through this.
struct Unfinished {
    end: Option<(Sender<Arrival>, Slot)>,
}

impl Unfinished {
    /// The job reported its own end.
    fn done(&mut self) {
        self.end = None;
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some((tx, id)) = self.end.take() {
            // The session may be gone; then nothing waits for this end.
            let _ = tx.send(Arrival::End(id, Err(WorkerError::Dropped)));
        }
    }
}

/// The sending half a streaming job reports through. Sending never blocks
/// and never fails the work: after the session is gone a message is
/// returned to the sender and dropped.
#[cfg_attr(
    not(any(test, feature = "capture")),
    expect(
        dead_code,
        reason = "the take's pump (feature `capture`) is the streaming half's user"
    )
)]
pub(crate) struct Landings<M> {
    tx: Sender<Arrival>,
    id: Slot,
    closed: Arc<AtomicBool>,
    _message: PhantomData<fn(M)>,
}

impl<M: Send + 'static> Landings<M> {
    /// Queues `message` to land in the next [`Session::update`], after
    /// every message sent before it.
    ///
    /// # Errors
    ///
    /// The message back, when the session is gone.
    #[cfg_attr(
        not(any(test, feature = "capture")),
        expect(
            dead_code,
            reason = "the take's pump (feature `capture`) is the streaming half's user"
        )
    )]
    pub(crate) fn send(&self, message: M) -> Result<(), M> {
        if self.is_closed() {
            return Err(message);
        }
        self.tx
            .send(Arrival::Message(self.id, Box::new(message)))
            .map_err(|refused| match refused.0 {
                Arrival::Message(_, boxed) => {
                    *boxed.downcast::<M>().expect("the message this call boxed")
                }
                Arrival::End(..) => unreachable!("this call sent a message"),
            })
    }

    /// The session is gone: nothing sent will land, and the job's result
    /// will not either.
    #[cfg_attr(
        not(any(test, feature = "capture")),
        expect(
            dead_code,
            reason = "the take's pump (feature `capture`) is the streaming half's user"
        )
    )]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// What a streaming job's lander receives: each message in order, then its
/// end, exactly once.
#[derive(Debug)]
pub(crate) enum Stream<M, R> {
    /// A message the work sent.
    Message(M),
    /// The work's result, after every message it sent.
    Done(R),
    /// The work did not finish, after every message it sent.
    Failed(WorkerError),
}

/// A running job's place in the lander table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Slot(u64);

/// What crosses from a worker to the control thread.
enum Arrival {
    #[cfg_attr(
        not(any(test, feature = "capture")),
        expect(
            dead_code,
            reason = "the take's pump (feature `capture`) is the streaming half's user"
        )
    )]
    Message(Slot, Box<dyn Any + Send>),
    End(Slot, Result<Box<dyn Any + Send>, WorkerError>),
}

/// A job's lander, typed away: it downcasts what its own work sent.
type Lander = Box<dyn FnMut(&mut dyn JobSession, Arrival, &mut Vec<SessionEvent>) + Send>;

/// The channel a job's messages and ends come back on, read only by the
/// control thread (`update` and `Job::wait`): off the audio thread, unbounded.
struct Arrivals {
    rx: Receiver<Arrival>,
    /// Arrivals a blocking wait took off the channel and `drain` has not
    /// returned yet.
    taken: VecDeque<Arrival>,
}

impl Arrivals {
    /// Everything that arrived, in order.
    fn drain(&mut self) -> Vec<Arrival> {
        let mut all: Vec<Arrival> = self.taken.drain(..).collect();
        all.extend(self.rx.try_iter());
        all
    }

    /// Blocks until something arrives or `slice` passes.
    fn block(&mut self, slice: Duration) {
        if !self.taken.is_empty() {
            return;
        }
        match self.rx.recv_timeout(slice) {
            Ok(arrival) => self.taken.push_back(arrival),
            // `Jobs` holds a sender: the channel never disconnects while it lives.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }
}

/// The session's jobs: the executor and bells they run on, the channel their
/// arrivals come back on, and the lander of each job still running.
pub(crate) struct Jobs {
    executor: Executor,
    bells: Bells,
    tx: Sender<Arrival>,
    arrivals: Arrivals,
    landers: HashMap<Slot, Lander>,
    next: tutti_rt::Mint,
    closed: Arc<AtomicBool>,
    /// What typed jobs landed, until the collect phase reports it.
    outputs: JobOutputQueue,
    /// Jobs the executor refused to start, and why: their `Dropped` end lands as
    /// [`WorkerError::NotStarted`].
    refused: HashMap<Slot, std::io::ErrorKind>,
    /// Events a [`Job::wait`] updated through, for the app's next update.
    held: Vec<SessionEvent>,
}

impl fmt::Debug for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs")
            .field("running", &self.landers.len())
            .finish_non_exhaustive()
    }
}

/// What a hosted plugin's job ended with: the landing a plugin-hosting (`f32`)
/// session applies and any other has no plugin to apply it to.
#[cfg(feature = "plugin")]
pub(crate) enum Landing<'a> {
    /// A reload ended with its plugin.
    Reload {
        node: tutti_graph::NodeKey,
        token: &'a Arc<()>,
        snapshot_age: Option<Duration>,
        loaded: Box<crate::plugin::PluginLoaded>,
    },
    /// A reload ended without one.
    ReloadFailed {
        node: tutti_graph::NodeKey,
        token: &'a Arc<()>,
        failed: WorkerError,
    },
    /// A parameter read ended with its values.
    Params {
        node: tutti_graph::NodeKey,
        token: &'a Arc<()>,
        values: crate::plugin::ParamValues,
    },
    /// A state snapshot ended with its state.
    Snapshot {
        node: tutti_graph::NodeKey,
        token: &'a Arc<()>,
        asked: tutti_platform::Instant,
        state: Result<Vec<u8>, tutti_plugin::error::StateError>,
    },
}

/// [`Landing`], named by the sealed format trait that routes it.
#[cfg(feature = "plugin")]
#[doc(hidden)]
pub struct PluginLanding<'a>(pub(crate) Landing<'a>);

#[cfg(feature = "plugin")]
impl fmt::Debug for PluginLanding<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginLanding").finish_non_exhaustive()
    }
}

/// What a job's lander may do to the session, whichever clock it is on: a
/// lander is typed to nothing of the clock, so the jobs in flight at a clock
/// switch land on the converted session.
pub(crate) trait JobSession {
    /// A pool load's worker ended: the identity it read under and what it staged.
    fn land_pool_load(
        &mut self,
        flight: u64,
        loaded_from: Option<tutti_platform::Identity>,
        done: Result<crate::pool::Staged, crate::LoadError>,
    );

    /// A pool refresh's check ended: the entries whose file changed reload.
    fn land_pool_refresh(&mut self, checked: Vec<crate::pool::Checked>) -> crate::RefreshReport;

    /// What a hosted plugin's job ended with.
    #[cfg(feature = "plugin")]
    fn land_plugin(&mut self, landing: Landing<'_>, out: &mut Vec<SessionEvent>);
}

impl<K: crate::AppKey, C: Clock> JobSession for Session<K, C> {
    fn land_pool_load(
        &mut self,
        flight: u64,
        loaded_from: Option<tutti_platform::Identity>,
        done: Result<crate::pool::Staged, crate::LoadError>,
    ) {
        self.pool.entries.worker_ended();
        self.pool.entries.stamped(flight, loaded_from);
        match done {
            Ok(crate::pool::Staged::Done(value)) => self.pool.entries.land(flight, Ok(value)),
            Ok(crate::pool::Staged::Main(stage)) => self.pool.entries.stage_main(flight, stage),
            Err(error) => self.pool.entries.land(flight, Err(error)),
        }
        self.pool_pump();
    }

    fn land_pool_refresh(&mut self, checked: Vec<crate::pool::Checked>) -> crate::RefreshReport {
        let (starts, refreshed) = self.pool.entries.reload_changed(checked);
        self.pool_start(starts);
        refreshed
    }

    #[cfg(feature = "plugin")]
    fn land_plugin(&mut self, landing: Landing<'_>, out: &mut Vec<SessionEvent>) {
        <C::Float as crate::format::Sealed>::land_plugin(self, PluginLanding(landing), out);
    }
}

impl Jobs {
    pub(crate) fn new(executor: Executor, bells: Bells) -> Self {
        let (tx, rx) = channel();
        Self {
            executor,
            bells,
            tx,
            arrivals: Arrivals {
                rx,
                taken: VecDeque::new(),
            },
            landers: HashMap::new(),
            next: tutti_rt::Mint::new(),
            closed: Arc::new(AtomicBool::new(false)),
            outputs: JobOutputQueue::default(),
            refused: HashMap::new(),
            held: Vec::new(),
        }
    }

    /// Where the typed jobs land: a handle that makes jobs without the session
    /// in hand makes its tickets on it.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn queue(&self) -> JobOutputQueue {
        Arc::clone(&self.outputs)
    }

    /// A job with no worker: its ticket lands when the session has the answer.
    pub(crate) fn new_job<J: JobKind>(&self) -> (Job<J>, Ticket<J>) {
        Ticket::new(&self.outputs)
    }

    /// Runs `work` on the executor as a typed job. `land` runs in a later
    /// [`Session::update`] with the worker's end and the job's ticket.
    pub(crate) fn submit_job<J, R, W, L>(&mut self, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        self.submit_job_at(Place::Pool, work, land)
    }

    /// [`submit_job`](Self::submit_job) for work that blocks: a threaded executor gives
    /// it a thread named `name`, so the pool never waits behind it.
    #[cfg_attr(
        not(feature = "plugin"),
        expect(
            dead_code,
            reason = "the plugin state job (feature `plugin`) is its user"
        )
    )]
    pub(crate) fn submit_job_blocking<J, R, W, L>(&mut self, name: &str, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        self.submit_job_at(Place::Dedicated(name), work, land)
    }

    fn submit_job_at<J, R, W, L>(&mut self, place: Place<'_>, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        let (job, ticket) = self.new_job::<J>();
        let cancel = ticket.cancel();
        self.submit_at(
            place,
            move || {
                cancel.started();
                work(&cancel)
            },
            move |session, done, _| land(session, done, ticket),
        );
        job
    }

    /// Keeps `events` for the next [`Session::update`].
    pub(crate) fn hold(&mut self, events: Vec<SessionEvent>) {
        self.held.extend(events);
    }

    /// The events held, oldest first.
    pub(crate) fn take_held(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.held)
    }

    /// Blocks until a job sends something, or `slice` passes.
    pub(crate) fn block(&mut self, slice: Duration) {
        self.arrivals.block(slice);
    }

    /// Reports what typed jobs landed, in landing order.
    pub(crate) fn report_landed(&mut self, out: &mut Vec<SessionEvent>) {
        let mut queue = self.outputs.lock_recover();
        // A job its owner awaits stays for that poll.
        let (awaited, reported): (Vec<_>, Vec<_>) = std::mem::take(&mut *queue)
            .into_iter()
            .partition(crate::JobOutput::awaited);
        *queue = awaited;
        drop(queue);
        out.extend(reported.into_iter().map(SessionEvent::JobDone));
    }

    /// Ticks the bells, then polls the executor's woken futures, up to
    /// [`POLLS_PER_UPDATE`]. Before the landings, so what a future sent lands in
    /// the same update.
    pub(crate) fn poll(&self) {
        self.bells.tick();
        self.executor.poll(POLLS_PER_UPDATE);
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl Jobs {
    /// Runs `work` on the executor and lands its result in a later
    /// [`Session::update`], on the control thread.
    ///
    /// `land` (and every lander of [`submit_dedicated`](Self::submit_dedicated))
    /// runs on the thread that calls `update()`, which need not be the main
    /// thread: a landing takes no main-thread proof (the plugin reload's does
    /// not), and what must be done on the main
    /// thread is posted to the session's `MainWork` (`tutti_plugin_types`).
    ///
    /// A future must not block, and a closure here is one `poll` long: a job whose
    /// `work` blocks (a plugin's round trip) says so with a `// blocks:` comment.
    pub(crate) fn submit<R, W, L>(&mut self, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        self.submit_at(Place::Pool, work, land);
    }

    /// [`submit`](Self::submit) for work that blocks (a plugin's round trip): a threaded
    /// executor gives it a thread named `name`, so the pool never waits behind it.
    #[cfg_attr(
        not(any(feature = "plugin", feature = "sampler")),
        expect(dead_code, reason = "the plugin and stream jobs are its users")
    )]
    pub(crate) fn submit_blocking<R, W, L>(&mut self, name: &str, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        self.submit_at(Place::Dedicated(name), work, land);
    }

    fn submit_at<R, W, L>(&mut self, place: Place<'_>, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        let mut land = Some(land);
        self.start_stream(
            place,
            move |_: &Landings<std::convert::Infallible>| work(),
            move |session, landed, events| {
                let end = match landed {
                    Stream::Message(never) => match never {},
                    Stream::Done(result) => Ok(result),
                    Stream::Failed(failed) => Err(failed),
                };
                if let Some(land) = land.take() {
                    land(session, end, events);
                }
            },
        );
    }

    /// Runs `work` on a thread of its own on a threaded executor (named `name`), landing
    /// each message it sends through its [`Landings`] and then its end, exactly once and
    /// after every message, in later [`Session::update`]s on the control thread.
    /// `work` sees the landings by reference, so nothing it sends can
    /// outlive it and follow the end. For work that runs as long as a take.
    ///
    /// An executor that refuses the future lands [`WorkerError::NotStarted`].
    #[cfg_attr(
        not(any(test, feature = "capture")),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn submit_dedicated<M, R, W, L>(&mut self, name: &str, work: W, land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        self.start_stream(Place::Dedicated(name), work, land);
    }

    fn start_stream<M, R, W, L>(&mut self, place: Place<'_>, work: W, mut land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        let id = Slot(self.next.mint().get());
        let landings = Landings {
            tx: self.tx.clone(),
            id,
            closed: Arc::clone(&self.closed),
            _message: PhantomData,
        };
        let lander: Lander = Box::new(move |session, arrival, events| {
            let landed = match arrival {
                Arrival::Message(_, message) => Stream::Message(
                    *message
                        .downcast::<M>()
                        .expect("a job's messages are its own type"),
                ),
                Arrival::End(_, Ok(result)) => Stream::Done(
                    *result
                        .downcast::<R>()
                        .expect("a job's result is its own type"),
                ),
                Arrival::End(_, Err(failed)) => Stream::Failed(failed),
            };
            land(session, landed, events);
        });
        self.landers.insert(id, lander);
        let mut unfinished = Unfinished {
            end: Some((self.tx.clone(), id)),
        };
        let future = Box::pin(async move {
            let end = catch_unwind(AssertUnwindSafe(|| work(&landings)))
                .map(|result| Box::new(result) as Box<dyn Any + Send>)
                .map_err(|payload| WorkerError::Panicked {
                    message: panic_message(payload.as_ref()),
                });
            unfinished.done();
            // After every message: they went down the same channel first.
            let _ = landings.tx.send(Arrival::End(id, end));
        });
        let started = match place {
            Place::Pool => self.executor.spawn(future),
            Place::Dedicated(name) => self.executor.spawn_dedicated(name, None, future),
        };
        // A refused start drops the future unrun, so `Unfinished` has queued
        // `Dropped`; `land_jobs` reports the refusal instead.
        if let Err(error) = started {
            self.refused.insert(id, error.kind());
        }
    }
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// Lands what the jobs sent since the last update, in arrival order.
    /// What arrives during a lander (a job it submits that ends at once)
    /// waits for the next update.
    pub(crate) fn land_jobs(&mut self, out: &mut Vec<SessionEvent>) {
        let arrived = self.jobs.arrivals.drain();
        for arrival in arrived {
            let (id, ends) = match &arrival {
                Arrival::Message(id, _) => (*id, false),
                Arrival::End(id, _) => (*id, true),
            };
            let arrival = match arrival {
                Arrival::End(id, Err(WorkerError::Dropped)) => {
                    let end = match self.jobs.refused.remove(&id) {
                        Some(kind) => WorkerError::NotStarted(kind),
                        None => WorkerError::Dropped,
                    };
                    Arrival::End(id, Err(end))
                }
                other => other,
            };
            // Out of the table while it runs: a lander may submit a job.
            let Some(mut lander) = self.jobs.landers.remove(&id) else {
                continue;
            };
            lander(self, arrival, out);
            if !ends {
                self.jobs.landers.insert(id, lander);
            }
        }
    }
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is not a string".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;
    use tutti_fixtures::HeldFutures;
    use tutti_platform::Instant;
    use tutti_units::SampleRate;

    fn session(builder: crate::SessionBuilder) -> crate::DetachedSession {
        builder.build_detached(SampleRate::SR_48K).unwrap()
    }

    /// Updates until `done` holds, or panics after a generous wait.
    fn update_until(s: &mut crate::DetachedSession, done: impl Fn() -> bool) -> Vec<SessionEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while !done() {
            assert!(Instant::now() < deadline, "the job never landed");
            events.extend(s.update());
            std::thread::yield_now();
        }
        events
    }

    #[test]
    fn a_result_lands_in_update_on_the_control_thread_not_before() {
        let mut s = session(Session::builder());
        let (go, wait) = sync_channel::<()>(0);
        let landed = Arc::new(Mutex::new(None));
        let into = Arc::clone(&landed);
        s.jobs.submit(
            move || {
                wait.recv().unwrap();
                std::thread::current().id()
            },
            move |_, result, _| {
                *into.lock().unwrap() = Some((result.unwrap(), std::thread::current().id()));
            },
        );
        let _ = s.update();
        assert!(
            landed.lock().unwrap().is_none(),
            "nothing lands before the work ends"
        );
        go.send(()).unwrap();
        let _ = update_until(&mut s, || landed.lock().unwrap().is_some());
        let (worker, lander) = landed.lock().unwrap().unwrap();
        assert_ne!(worker, lander, "the work ran on the executor's thread");
        assert_eq!(lander, std::thread::current().id(), "it landed in update");
    }

    #[test]
    fn messages_land_in_order_and_the_end_once_after_every_one() {
        let mut s = session(Session::builder());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        s.jobs.submit_dedicated(
            "test",
            |landings: &Landings<u32>| {
                for n in 0..500 {
                    landings.send(n).unwrap();
                }
                500
            },
            move |_, landed, _| {
                into.lock().unwrap().push(match landed {
                    Stream::Message(n) => n,
                    Stream::Done(end) => end + 1_000,
                    Stream::Failed(failed) => panic!("{failed}"),
                });
            },
        );
        let _ = update_until(&mut s, || seen.lock().unwrap().last() == Some(&1_500));
        let _ = s.update();
        let mut want: Vec<u32> = (0..500).collect();
        want.push(1_500);
        assert_eq!(*seen.lock().unwrap(), want);
    }

    #[test]
    fn a_panic_lands_as_failed_with_its_message() {
        let mut s = session(Session::builder());
        let landed = Arc::new(Mutex::new(None));
        let into = Arc::clone(&landed);
        s.jobs.submit(
            || -> u8 { panic!("the file was gone") },
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        let _ = update_until(&mut s, || landed.lock().unwrap().is_some());
        assert_eq!(
            landed.lock().unwrap().take().unwrap(),
            Err(WorkerError::Panicked {
                message: "the file was gone".into()
            })
        );
    }

    #[test]
    fn a_job_the_host_drops_lands_as_dropped_and_its_work_never_runs() {
        let held = HeldFutures::new();
        let mut s = session(Session::builder().executor(held.executor()));
        let ran = Arc::new(AtomicBool::new(false));
        let landed = Arc::new(Mutex::new(None));
        let (r, into) = (Arc::clone(&ran), Arc::clone(&landed));
        s.jobs.submit(
            move || r.store(true, Ordering::SeqCst),
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        held.drop_all();
        let _ = s.update();
        assert_eq!(
            landed.lock().unwrap().take(),
            Some(Err(WorkerError::Dropped))
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn every_spawned_job_reaches_the_hosts_spawn_function() {
        let held = HeldFutures::new();
        let mut s = session(Session::builder().executor(held.executor()));
        let landed = Arc::new(Mutex::new(Vec::new()));
        for n in 0..3_u8 {
            let into = Arc::clone(&landed);
            s.jobs.submit(
                move || n,
                move |_, r, _| into.lock().unwrap().push(r.unwrap()),
            );
        }
        assert_eq!(held.len(), 3, "each job is one future handed to the host");
        held.run_all();
        let _ = s.update();
        assert_eq!(*landed.lock().unwrap(), [0, 1, 2]);
    }

    #[test]
    fn a_polled_sessions_job_runs_and_lands_in_the_update_and_not_before() {
        let mut s = session(Session::builder().executor(Executor::polled()));
        let ran = Arc::new(AtomicBool::new(false));
        let landed = Arc::new(Mutex::new(None));
        let (r, into) = (Arc::clone(&ran), Arc::clone(&landed));
        s.jobs.submit(
            move || {
                r.store(true, Ordering::SeqCst);
                std::thread::current().id()
            },
            move |_, result, _| {
                *into.lock().unwrap() = Some((result.unwrap(), std::thread::current().id()));
            },
        );
        assert!(!ran.load(Ordering::SeqCst), "no work runs outside update");
        assert!(landed.lock().unwrap().is_none());
        let _ = s.update();
        let (worker, lander) = landed.lock().unwrap().take().expect("landed in the update");
        assert_eq!(worker, std::thread::current().id(), "polled by update");
        assert_eq!(lander, worker);
    }

    #[test]
    fn a_polled_sessions_panicking_job_lands_failed_and_the_update_goes_on() {
        let mut s = session(Session::builder().executor(Executor::polled()));
        let landed = Arc::new(Mutex::new(None));
        let into = Arc::clone(&landed);
        s.jobs.submit(
            || -> u8 { panic!("the decode blew up") },
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        let _ = s.update();
        assert_eq!(
            landed.lock().unwrap().take().unwrap(),
            Err(WorkerError::Panicked {
                message: "the decode blew up".into()
            })
        );
    }

    #[test]
    fn a_job_outliving_its_session_sees_it_closed_and_its_sends_come_back() {
        let held = HeldFutures::new();
        let mut s = session(Session::builder().executor(held.executor()));
        let seen = Arc::new(Mutex::new(None));
        let into = Arc::clone(&seen);
        s.jobs.submit_dedicated(
            "test",
            move |landings: &Landings<u8>| {
                *into.lock().unwrap() = Some((landings.send(7), landings.is_closed()));
            },
            |_, _, _| {},
        );
        drop(s);
        held.run_all();
        let (sent, closed) = seen.lock().unwrap().take().expect("the job ran");
        assert_eq!(sent, Err(7), "the message comes back");
        assert!(closed);
    }
}
