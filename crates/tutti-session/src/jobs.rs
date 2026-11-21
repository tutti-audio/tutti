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
//! The executor is the session's own, driven by the one thread tutti starts for it in
//! `build()`. That thread stops and joins when the session drops, so a
//! file load running on it holds the drop until its work returns. The plugin
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
use tutti_rt::{Bell, Bells, LockExt};

use crate::background::Running;
use crate::job::{Cancel, Job, JobKind, JobOutputQueue, Ticket};
use crate::loaded::panic_message;
use crate::{Clock, Session, SessionEvent, WorkerError};

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
    /// Declared first: the thread that drives the queue stops and joins before the rest
    /// drops.
    _background: Running,
    executor: Executor,
    bells: Bells,
    /// The long-lived pumps (a take's) and the bell each one awaits: the drop rings them
    /// and joins them, so what a pump owes at its end (a take's commit) is done first.
    pumps: Vec<(Task<()>, Bell)>,
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
pub(crate) trait JobSession {}

impl<K: crate::AppKey, C: Clock> JobSession for Session<K, C> {}

impl Jobs {
    pub(crate) fn new(executor: Executor, background: Running, bells: Bells) -> Self {
        let (tx, rx) = channel();
        Self {
            _background: background,
            executor,
            bells,
            pumps: Vec::new(),
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
            reason = "the job table's tests submit jobs; no session call does"
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

    /// Ticks the bells: what a future awaiting one sent lands in the same update.
    pub(crate) fn poll(&self) {
        self.bells.tick();
    }
}

/// The longest the drop waits for the long-lived pumps to finish: a guard against a hang, not
/// a deadline; a pump that has the landings closed and its bell rung ends within a block.
const PUMP_JOIN_BOUND: Duration = Duration::from_secs(10);

impl Drop for Jobs {
    /// Closes the landings, rings each pump's bell so it sees them closed and finishes
    /// (a take commits its file), and waits for each before the executor can go.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        for (_, bell) in &self.pumps {
            bell.ring();
        }
        // Delivers the rings to the pumps' wakers now, not at the next tick.
        self.bells.tick();
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
    #[cfg(test)]
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

    /// Spawns `future` with a thread of its own: work that is not a job and runs as long
    /// as a take. It awaits `bell`: the session's drop closes the landings, rings it and
    /// waits for the future to end.
    ///
    /// # Errors
    ///
    /// The executor's refusal to start it. The future is not run.
    #[cfg_attr(
        not(feature = "capture"),
        expect(dead_code, reason = "the take's pump (feature `capture`) is its user")
    )]
    pub(crate) fn spawn_long(
        &mut self,
        name: &str,
        bell: Bell,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let pump = self.executor.spawn_dedicated(name, None, future)?;
        self.pumps.retain(|(pump, _)| !pump.is_finished());
        self.pumps.push((pump, bell));
        Ok(())
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

    /// The session, and so its runtime, drops with a job's end unlanded: the job's ticket
    /// goes with it, and the app awaiting the job sees `Dropped`.
    #[test]
    fn an_awaited_job_in_a_dropped_session_resolves_dropped() {
        let mut s = session(Session::builder());
        let awaiting = Executor::new();
        let (started, started_rx) = sync_channel::<()>(0);
        let (go, go_rx) = sync_channel::<()>(0);
        let job: Job<Echo> = s.jobs.submit_job(
            move |_| {
                started.send(()).unwrap();
                go_rx.recv().unwrap();
                7_u32
            },
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
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        go.send(()).unwrap();
        // The drop joins the thread, so the work has ended; no update landed its end.
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
    fn a_job_outliving_its_session_sees_it_closed_and_its_sends_come_back() {
        let mut s = session(Session::builder());
        let (started, started_rx) = sync_channel::<()>(0);
        let seen = Arc::new(Mutex::new(None));
        let into = Arc::clone(&seen);
        s.jobs.submit_stream(
            move |landings: &Landings<u8>| {
                started.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !landings.is_closed() {
                    assert!(Instant::now() < deadline, "the session never closed");
                    std::thread::yield_now();
                }
                *into.lock().unwrap() = Some((landings.send(7), landings.is_closed()));
            },
            |_, _, _| {},
        );
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // The drop closes the jobs, then joins the thread the job is running on.
        drop(s);
        let (sent, closed) = seen.lock().unwrap().take().expect("the job ran");
        assert_eq!(sent, Err(7), "the message comes back");
        assert!(closed);
    }
}
