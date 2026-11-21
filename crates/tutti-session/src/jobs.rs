//! The session's background work: a closure (a file load, a recording's pump)
//! wrapped in a future and spawned on the session's [`Executor`], and what it
//! reports lands on the control thread in [`Session::update`], in the order it
//! was sent.
//!
//! A job borrows nothing of the session. It reports through its
//! [`Landings`], which never blocks, and ends exactly once, after every
//! message it sent: with its result, or with a [`WorkerError`] when it
//! panicked. A job the executor drops unrun (the session dropped) lands nowhere:
//! its ticket drops with the session's landers, and a [`Job`] awaited elsewhere
//! resolves `Dropped`. A typed [`Job`]
//! (`job.rs`) is cancelled by its drop; a streaming job carries its own flag,
//! and [`Landings::is_closed`] tells it when the session is gone, so it
//! stops writing what no one will land.
//!
//! The executor is the session's own, driven by threads it starts, or the builder's
//! choice ([`SessionBuilder::executor`](crate::SessionBuilder::executor)), driven by
//! `update`. The threads that drive it stop and join when the session drops, so a
//! file load running on one holds the drop until its work returns. The plugin
//! loops are not jobs: they run on the executor's blocking pool, and a drop waits for
//! them separately (see `plugin.rs`).

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
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::task::{Context, Poll};
use std::time::Duration;

use tutti_platform::{Executor, Task};
use tutti_rt::{Bells, LockExt};

use crate::drive::Drive;
use crate::job::{Cancel, Job, JobKind, JobOutputQueue, Ticket};
use crate::{Clock, Session, SessionEvent, WorkerError};

/// The most tasks one [`Session::update`] runs on an executor it drives: a count, never
/// a time. A session whose own threads drive the queue ignores it; work past it runs in
/// the next update.
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

impl<M> Clone for Landings<M> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            id: self.id,
            closed: Arc::clone(&self.closed),
            _message: PhantomData,
        }
    }
}

/// A future whose panic is its output: what a job's body is polled as.
struct Contained<F>(Pin<Box<F>>);

impl<F: Future> Future for Contained<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
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
    /// Declared first: what drives the queue stops and joins before the rest drops.
    drive: Drive,
    executor: Executor,
    bells: Bells,
    tx: Sender<Arrival>,
    arrivals: Arrivals,
    landers: HashMap<Slot, Lander>,
    next: tutti_rt::Mint,
    closed: Arc<AtomicBool>,
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

/// What a job's lander may do to the session, whichever clock it is on: a
/// lander is typed to nothing of the clock, so the jobs in flight at a clock
/// switch land on the converted session.
pub(crate) trait JobSession {
    /// A pool load's worker ended with what it staged.
    fn land_pool_load(&mut self, flight: u64, done: Result<crate::pool::Staged, crate::LoadError>);
}

impl<K: crate::AppKey, C: Clock> JobSession for Session<K, C> {
    fn land_pool_load(&mut self, flight: u64, done: Result<crate::pool::Staged, crate::LoadError>) {
        self.pool.entries.worker_ended();
        match done {
            Ok(crate::pool::Staged::Done(value)) => self.pool.entries.land(flight, Ok(value)),
            Ok(crate::pool::Staged::Main(stage)) => self.pool.entries.stage_main(flight, stage),
            Err(error) => self.pool.entries.land(flight, Err(error)),
        }
        self.pool_pump();
    }
}

impl Jobs {
    pub(crate) fn new(executor: Executor, drive: Drive, bells: Bells) -> Self {
        let (tx, rx) = channel();
        Self {
            drive,
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the job table's tests submit on the pool; no session call does"
        )
    )]
    pub(crate) fn submit_job<J, R, W, L>(&mut self, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        self.submit_job_at(work, land)
    }

    fn submit_job_at<J, R, W, L>(&mut self, work: W, land: L) -> Job<J>
    where
        J: JobKind,
        R: Send + 'static,
        W: FnOnce(&Cancel) -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, Ticket<J>) + Send + 'static,
    {
        let (job, ticket) = self.new_job::<J>();
        let cancel = ticket.cancel();
        self.submit_at(
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

    /// Ticks the bells, then, unless threads drive the executor, runs up to
    /// [`POLLS_PER_UPDATE`] of its ready tasks. Before the landings, so what a future
    /// sent lands in the same update.
    pub(crate) fn poll(&self) {
        self.bells.tick();
        if self.drive.runs_itself() {
            return;
        }
        for _ in 0..POLLS_PER_UPDATE {
            if !self.executor.try_tick() {
                break;
            }
        }
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
    /// `land` (and every lander of a streaming job)
    /// runs on the thread that calls `update()`, which need not be the main
    /// thread: a landing takes no main-thread proof, and what must be done on the main
    /// thread is posted to the session's `MainWork` (`tutti_plugin_types`).
    ///
    /// A future must not block, and a closure here is one `poll` long.
    pub(crate) fn submit<R, W, L>(&mut self, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        self.submit_at(work, land);
    }

    fn submit_at<R, W, L>(&mut self, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut dyn JobSession, Result<R, WorkerError>, &mut Vec<SessionEvent>)
            + Send
            + 'static,
    {
        let mut land = Some(land);
        self.start_stream(
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

    /// Runs `work` on the executor, landing each message it sends through its
    /// [`Landings`] and then its end, exactly once and after every message, in later
    /// [`Session::update`]s on the control thread. `work` sees the landings by
    /// reference, so nothing it sends can outlive it and follow the end.
    #[cfg(test)]
    pub(crate) fn submit_stream<M, R, W, L>(&mut self, work: W, land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        self.start_stream(work, land);
    }

    fn start_stream<M, R, W, L>(&mut self, work: W, land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        self.start(move |landings| async move { work(&landings) }, land);
    }

    fn start<M, R, F, W, L>(&mut self, make: W, land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        F: Future<Output = R> + Send + 'static,
        W: FnOnce(Landings<M>) -> F + Send + 'static,
        L: FnMut(&mut dyn JobSession, Stream<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        let run = self.open_stream(land);
        let future = Box::pin(run.run(make));
        self.executor.spawn(future).detach();
    }

    /// Registers a streaming job's lander and gives back the half the job's future
    /// holds, made before that future exists: a take starts its pump first and hands
    /// it this once the file and the tap are open. Dropped unrun, the run lands
    /// [`WorkerError::Dropped`].
    pub(crate) fn open_stream<M, R, L>(&mut self, mut land: L) -> StreamRun<M, R>
    where
        M: Send + 'static,
        R: Send + 'static,
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
        StreamRun {
            landings,
            unfinished: Unfinished {
                end: Some((self.tx.clone(), id)),
            },
            _result: PhantomData,
        }
    }

    /// Spawns `future` with a thread of its own where the session's threads drive its
    /// executor: work that is not a job and runs as long as a take.
    ///
    /// # Errors
    ///
    /// The executor's refusal to start it. The future is not run.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn spawn_long(
        &self,
        name: &str,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        // An executor that `update` drives queues the future like any other, so that
        // update runs it; the session's own threads give it a thread of its own.
        if self.drive.runs_itself() {
            self.executor.spawn_dedicated(name, None, future)
        } else {
            Ok(self.executor.spawn(future))
        }
        .map(Task::detach)
    }

    /// The session's bells: a future that awaits the audio thread makes its bell here.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn bells(&self) -> &Bells {
        &self.bells
    }
}

/// A streaming job's reporting half: what the job's future runs through.
pub(crate) struct StreamRun<M, R> {
    landings: Landings<M>,
    unfinished: Unfinished,
    _result: PhantomData<fn() -> R>,
}

impl<M: Send + 'static, R: Send + 'static> StreamRun<M, R> {
    /// Runs the job's body, then lands its end exactly once, after every message:
    /// its result, or [`WorkerError::Panicked`] when it panicked. `make` builds the
    /// body from the landings.
    pub(crate) async fn run<F>(mut self, make: impl FnOnce(Landings<M>) -> F)
    where
        F: Future<Output = R>,
    {
        let end = Contained(Box::pin(make(self.landings.clone())))
            .await
            .map(|result| Box::new(result) as Box<dyn Any + Send>)
            .map_err(|payload| WorkerError::Panicked {
                message: panic_message(payload.as_ref()),
            });
        self.unfinished.done();
        // After every message: they went down the same channel first.
        let _ = self.landings.tx.send(Arrival::End(self.landings.id, end));
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
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;
    use tutti_platform::Instant;
    use tutti_units::SampleRate;

    fn session(builder: crate::SessionBuilder) -> crate::DetachedSession {
        builder.build_detached(SampleRate::SR_48K).unwrap()
    }

    #[derive(Debug, PartialEq, Eq, thiserror::Error)]
    enum EchoError {
        #[error(transparent)]
        Worker(#[from] WorkerError),
        #[error(transparent)]
        Wait(#[from] crate::WaitError),
    }

    crate::job::job_kind!(Echo => u32, EchoError);

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

    /// A job queued on a session's executor, which nothing has run, never runs once the
    /// session drops: the drop takes the executor, and the executor its queue.
    #[test]
    fn a_job_queued_in_a_dropped_session_never_runs() {
        let mut s = session(Session::builder().executor(Executor::new()));
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        s.jobs
            .submit(move || r.store(true, Ordering::SeqCst), |_, _, _| {});
        drop(s);
        assert!(!ran.load(Ordering::SeqCst));
    }

    /// The session, and so its runtime, drops with a job's work unrun: the job's ticket
    /// goes with it, and the app awaiting the job sees `Dropped`.
    #[test]
    fn an_awaited_job_in_a_dropped_session_resolves_dropped() {
        let mut s = session(Session::builder().executor(Executor::new()));
        let awaiting = Executor::new();
        let job: Job<Echo> = s.jobs.submit_job(
            |_| 7_u32,
            |_, done, ticket| ticket.land(done.map_err(EchoError::from)),
        );
        let got = Arc::new(Mutex::new(None));
        let into = Arc::clone(&got);
        awaiting
            .spawn(async move {
                *into.lock().unwrap() = Some(job.await);
            })
            .detach();
        assert_eq!(awaiting.run_until_stalled(), 1);
        assert!(got.lock().unwrap().is_none(), "the job is pending");
        drop(s);
        assert_eq!(
            awaiting.run_until_stalled(),
            1,
            "the drop woke the awaiting future"
        );
        assert_eq!(
            got.lock().unwrap().take(),
            Some(Err(EchoError::Worker(WorkerError::Dropped)))
        );
    }

    #[test]
    fn a_polled_sessions_jobs_run_and_land_in_the_order_they_were_spawned() {
        let mut s = session(Session::builder().executor(Executor::new()));
        let landed = Arc::new(Mutex::new(Vec::new()));
        for n in 0..3_u8 {
            let into = Arc::clone(&landed);
            s.jobs.submit(
                move || n,
                move |_, r, _| into.lock().unwrap().push(r.unwrap()),
            );
        }
        assert!(
            landed.lock().unwrap().is_empty(),
            "nothing runs before update"
        );
        let _ = s.update();
        assert_eq!(*landed.lock().unwrap(), [0, 1, 2]);
    }

    #[test]
    fn a_polled_sessions_job_runs_and_lands_in_the_update_and_not_before() {
        let mut s = session(Session::builder().executor(Executor::new()));
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
        let mut s = session(Session::builder().executor(Executor::new()));
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
        let executor = Executor::new();
        let mut s = session(Session::builder().executor(executor.clone()));
        let seen = Arc::new(Mutex::new(None));
        let into = Arc::clone(&seen);
        s.jobs.submit_stream(
            move |landings: &Landings<u8>| {
                *into.lock().unwrap() = Some((landings.send(7), landings.is_closed()));
            },
            |_, _, _| {},
        );
        drop(s);
        assert_eq!(executor.run_until_stalled(), 1);
        let (sent, closed) = seen.lock().unwrap().take().expect("the job ran");
        assert_eq!(sent, Err(7), "the message comes back");
        assert!(closed);
    }
}
