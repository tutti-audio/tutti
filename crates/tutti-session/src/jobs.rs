//! The session's spawner: work that blocks (a file load, a
//! plugin round trip, a recording's pump) runs on a thread the host's
//! spawner gives it, and what it reports lands on the control thread in
//! [`Session::update`], in the order it was sent.
//!
//! A job borrows nothing of the session. It reports through its
//! [`Landings`], which never blocks, and ends exactly once, after every
//! message it sent: with its result, or with a [`WorkerError`] when it
//! panicked or the spawner dropped it unrun. A typed [`Job`]
//! (`job.rs`) is cancelled by its drop; a streaming job carries its own flag,
//! and [`Landings::is_closed`] tells it when the session is gone, so it
//! stops writing what no one will land.
//!
//! The default spawner starts each task on a thread of
//! [`tutti_platform::spawn_named`]. When the session drops, `Jobs` stops and joins
//! those threads within [`DROP_BOUND`]; a thread still running after it (a
//! worker blocked on the main thread, say) is detached with a `tracing`
//! warning and **outlives the session** until its work returns.

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
use tutti_platform::Instant;

use tutti_platform::ThreadHandle;
use tutti_rt::LockExt;

use crate::job::{Cancel, Job, JobKind, JobOutputQueue, Ticket};
use crate::{Clock, Session, SessionEvent, WorkerError};

/// How long dropping the session waits for its job threads to stop.
const DROP_BOUND: Duration = Duration::from_millis(100);

/// One unit of blocking work the session hands to its spawner
/// ([`SessionBuilder::spawn_with`](crate::SessionBuilder::spawn_with)). The
/// spawner calls [`run`](Self::run) on a thread of its own; a task it drops
/// unrun lands as [`WorkerError::Dropped`].
#[must_use = "a task that is neither run nor kept lands as dropped"]
pub struct Task {
    work: Option<Box<dyn FnOnce() + Send>>,
    /// Where an unrun task reports its end.
    end: Option<(Sender<Arrival>, Slot)>,
}

impl Task {
    /// Runs the work on the calling thread, blocking until it ends. A panic
    /// in it is caught and lands as [`WorkerError::Panicked`].
    pub fn run(mut self) {
        self.end = None;
        if let Some(work) = self.work.take() {
            work();
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some((tx, id)) = self.end.take() {
            // The session may be gone; then nothing waits for this end.
            let _ = tx.send(Arrival::End(id, Err(WorkerError::Dropped)));
        }
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").finish_non_exhaustive()
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

/// Where the session starts its tasks.
#[derive(Clone, Default)]
pub(crate) enum Spawner {
    /// A named thread per task, through [`tutti_platform::spawn_named`]; `Jobs`
    /// keeps the handles. A thread that cannot start drops its task, which
    /// lands as [`WorkerError::Dropped`].
    #[default]
    Threads,
    /// The host's own (`SessionBuilder::spawn_with`).
    Host(Arc<dyn Fn(Task) + Send + Sync>),
}

impl Spawner {
    pub(crate) fn new(spawn: impl Fn(Task) + Send + Sync + 'static) -> Self {
        Self::Host(Arc::new(spawn))
    }
}

impl fmt::Debug for Spawner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spawner").finish_non_exhaustive()
    }
}

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

/// The session's jobs: the spawner, the channel their arrivals come back
/// on, and the lander of each job still running.
pub(crate) struct Jobs {
    spawn: Spawner,
    tx: Sender<Arrival>,
    arrivals: Arrivals,
    landers: HashMap<Slot, Lander>,
    next: tutti_rt::Mint,
    closed: Arc<AtomicBool>,
    /// The default spawner's threads, until they end.
    threads: Vec<ThreadHandle<()>>,
    /// What typed jobs landed, until the collect phase reports it.
    outputs: JobOutputQueue,
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
        asked: Instant,
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
    pub(crate) fn new(spawn: Spawner) -> Self {
        let (tx, rx) = channel();
        Self {
            spawn,
            tx,
            arrivals: Arrivals {
                rx,
                taken: VecDeque::new(),
            },
            landers: HashMap::new(),
            next: tutti_rt::Mint::new(),
            closed: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            outputs: JobOutputQueue::default(),
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

    /// Runs `work` on the spawner as a typed job. `land` runs in a later
    /// [`Session::update`] with the worker's end and the job's ticket.
    pub(crate) fn submit_job<J, R, W, L>(&mut self, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        let (job, ticket) = self.new_job::<J>();
        let cancel = ticket.cancel();
        self.submit(
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
        let outputs = std::mem::take(&mut *self.outputs.lock_recover());
        out.extend(outputs.into_iter().map(SessionEvent::JobDone));
    }

    /// Hands `task` to the spawner.
    fn dispatch(&mut self, task: Task) {
        match &self.spawn {
            Spawner::Host(spawn) => spawn(task),
            Spawner::Threads => {
                self.threads.retain(|thread| !thread.is_finished());
                // A start that fails drops its closure, and the task in it.
                if let Ok(thread) =
                    tutti_platform::spawn_named("tutti-job", None, move |_| task.run())
                {
                    self.threads.push(thread);
                }
            }
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let deadline = Instant::now() + DROP_BOUND;
        for thread in self.threads.drain(..) {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Err(stuck) = thread.stop_within(left) {
                tracing::warn!(
                    "a session job thread is still running; it outlives the session until its work returns"
                );
                stuck.detach();
            }
        }
    }
}

impl Jobs {
    /// Runs `work` on the spawner and lands its result in a later
    /// [`Session::update`], on the control thread.
    ///
    /// `land` (and every lander of [`submit_stream`](Self::submit_stream))
    /// runs on the thread that calls `update()`, which need not be the main
    /// thread: a landing takes no main-thread proof (the plugin reload's does
    /// not), and what must be done on the main
    /// thread is posted to the session's `MainWork` (`tutti_plugin_types`).
    pub(crate) fn submit<R, W, L>(&mut self, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        let mut land = Some(land);
        self.submit_stream(
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

    /// Runs `work` on the spawner, landing each message it sends through
    /// its [`Landings`] and then its end, exactly once and after every
    /// message, in later [`Session::update`]s on the control thread.
    /// `work` sees the landings by reference, so nothing it sends can
    /// outlive it and follow the end.
    pub(crate) fn submit_stream<M, R, W, L>(&mut self, work: W, mut land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        let jobs = &mut *self;
        let id = Slot(jobs.next.mint().get());
        let landings = Landings {
            tx: jobs.tx.clone(),
            id,
            closed: Arc::clone(&jobs.closed),
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
        jobs.landers.insert(id, lander);
        let end_tx = jobs.tx.clone();
        let job = Task {
            work: Some(Box::new(move || {
                let end = catch_unwind(AssertUnwindSafe(|| work(&landings)))
                    .map(|result| Box::new(result) as Box<dyn Any + Send>)
                    .map_err(|payload| WorkerError::Panicked {
                        message: panic_message(payload.as_ref()),
                    });
                // After every message: they went down the same channel first.
                let _ = landings.tx.send(Arrival::End(id, end));
            })),
            end: Some((end_tx, id)),
        };
        self.dispatch(job);
    }
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// Lands what the jobs sent since the last update, in arrival order.
    /// What arrives during a lander (a job it submits that ends at once)
    /// waits for the next update.
    pub(crate) fn land_jobs(&mut self, out: &mut Vec<SessionEvent>) {
        self.jobs.threads.retain(|thread| !thread.is_finished());
        let arrived = self.jobs.arrivals.drain();
        for arrival in arrived {
            let (id, ends) = match &arrival {
                Arrival::Message(id, _) => (*id, false),
                Arrival::End(id, _) => (*id, true),
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
        assert_ne!(worker, lander, "the work ran on the spawner's thread");
        assert_eq!(lander, std::thread::current().id(), "it landed in update");
    }

    #[test]
    fn messages_land_in_order_and_the_end_once_after_every_one() {
        let mut s = session(Session::builder());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        s.jobs.submit_stream(
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
    fn a_job_the_spawner_drops_lands_as_dropped_and_its_work_never_runs() {
        let mut s = session(Session::builder().spawn_with(drop));
        let ran = Arc::new(AtomicBool::new(false));
        let landed = Arc::new(Mutex::new(None));
        let (r, into) = (Arc::clone(&ran), Arc::clone(&landed));
        s.jobs.submit(
            move || r.store(true, Ordering::SeqCst),
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        let _ = s.update();
        assert_eq!(
            landed.lock().unwrap().take(),
            Some(Err(WorkerError::Dropped))
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn the_hosts_spawner_runs_every_job() {
        let spawned = Arc::new(Mutex::new(0_usize));
        let count = Arc::clone(&spawned);
        let mut s = session(Session::builder().spawn_with(move |job| {
            *count.lock().unwrap() += 1;
            job.run();
        }));
        let landed = Arc::new(Mutex::new(Vec::new()));
        for n in 0..3_u8 {
            let into = Arc::clone(&landed);
            s.jobs.submit(
                move || n,
                move |_, r, _| into.lock().unwrap().push(r.unwrap()),
            );
        }
        let _ = s.update();
        assert_eq!(*spawned.lock().unwrap(), 3);
        assert_eq!(*landed.lock().unwrap(), [0, 1, 2]);
    }

    #[test]
    fn a_job_outliving_its_session_sees_it_closed_and_its_sends_come_back() {
        let mut s = session(Session::builder());
        let (go, wait) = sync_channel::<()>(0);
        let (opened, open) = sync_channel::<bool>(1);
        let (report, seen) = sync_channel::<(Result<(), u8>, bool)>(1);
        s.jobs.submit_stream(
            move |landings: &Landings<u8>| {
                opened.send(landings.is_closed()).unwrap();
                wait.recv().unwrap();
                report
                    .send((landings.send(7), landings.is_closed()))
                    .unwrap();
            },
            |_, _, _| {},
        );
        let wait_long = Duration::from_secs(10);
        assert!(
            !open.recv_timeout(wait_long).unwrap(),
            "open while the session lived"
        );
        drop(s);
        go.send(()).unwrap();
        let (sent, after) = seen.recv_timeout(wait_long).unwrap();
        assert_eq!(sent, Err(7), "the message comes back");
        assert!(after);
    }
}
