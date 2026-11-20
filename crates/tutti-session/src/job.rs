//! Work that takes time: one typed [`Job`] per piece of work.
//!
//! A [`Session`] call that cannot answer at once (a plugin's state, a pool
//! check) returns a [`Job<J>`](Job). Its kind `J` names the answer: `J::Output`
//! on success, `J::Error` on failure. The answer lands once, in an
//! [`update`](Session::update), as a [`SessionEvent::JobDone`] the app reads
//! with [`JobOutput::take`]; or [`Job::wait`] runs updates until it finishes and
//! returns it, the one blocking door. A `Job` is also a [`Future`]: awaited, it
//! resolves once an `update` has landed it. Dropping the job cancels it: its
//! result, if the work still finishes, is discarded.
//!
//! # Adding a kind (inside this crate)
//!
//! [`JobKind`] is sealed: a kind is added here, in four steps.
//!
//! 1. Name the kind and its answer with `job_kind!`: the unit struct, its
//!    `Output` and its `Error`. The error converts from [`WorkerError`] and
//!    [`WaitError`] (`#[from]` variants).
//! 2. In the session call, start the work with `Jobs::submit_job::<Kind, _, _, _>(work,
//!    land)` and return the [`Job`] it gives. `work` runs on the session's
//!    executor and may read its [`Cancel`]; `land` runs in the next
//!    [`update`](Session::update) on the control thread and ends with
//!    `Ticket::land(result)`.
//! 3. A stage that must run on the main thread is posted to the session's main
//!    work (`Session::work`); it sets `Ticket::waiting_for_main_thread` first
//!    and lands its result with `Ticket::land`, checking
//!    `Ticket::cancelled` before it changes anything.
//! 4. Work that needs no worker (a result the session already waits for) takes
//!    `Jobs::new_job::<Kind>()` and lands through its [`Ticket`] when the
//!    session has the answer.
//!
//! A [`Ticket`] that is dropped without landing ends the job: [`Job::wait`]
//! answers [`WorkerError::Dropped`].

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tutti_rt::LockExt;
use tutti_rt::Mint;

use crate::{Clock, Session, SessionEvent};

pub(crate) mod sealed {
    /// Keeps [`JobKind`](super::JobKind) the crate's to implement.
    pub trait Sealed {}
}

/// Names the answer of a [`Job`]. Sealed: only `tutti-session` implements it.
pub trait JobKind: sealed::Sealed + 'static {
    /// What the job lands on success.
    type Output: Send + 'static;
    /// Why it ended without: the kind's own typed error. A worker that
    /// panicked or was dropped ([`WorkerError`]) and a [`Job::wait`] that could
    /// not wait ([`WaitError`]) convert in by `From`.
    type Error: std::error::Error + From<WorkerError> + From<WaitError> + Send + 'static;
}

/// Declares a job kind: the unit struct and its sealed [`JobKind`] impl.
macro_rules! job_kind {
    (
        $(#[$meta:meta])*
        $vis:vis $kind:ident $(<$param:ident>)? => $output:ty, $error:ty
    ) => {
        $(#[$meta])*
        #[derive(Debug)]
        $vis struct $kind $(<$param>)? $((::std::marker::PhantomData<fn() -> $param>))?;

        impl $(<$param: Send + 'static>)? $crate::job::sealed::Sealed for $kind $(<$param>)? {}

        impl $(<$param: Send + 'static>)? $crate::JobKind for $kind $(<$param>)? {
            type Output = $output;
            type Error = $error;
        }
    };
}
pub(crate) use job_kind;

/// A piece of work running off the caller's thread. Dropping it cancels the
/// work: a result that still arrives is discarded.
#[must_use = "dropping a job cancels it"]
pub struct Job<J: JobKind> {
    id: JobId,
    cell: Arc<Cell>,
    queue: JobOutputQueue,
    _kind: PhantomData<fn() -> J>,
}

/// One job's identity, unique in the process: a [`JobOutput`] is keyed by it, and
/// its result is type-erased, so two sessions never share an id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobId(u64);

/// The next [`JobId`]'s number, process-wide.
static NEXT_ID: Mint = Mint::new();

/// Where a [`Job`] stands. A reading, not a promise: it moves on while the
/// caller looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JobStatus {
    /// Started, its work not begun.
    Pending,
    /// The work is running off the caller's thread.
    Running,
    /// The work waits for a stage that runs on the main thread, in an
    /// [`update`](Session::update) called there.
    WaitingForMainThread,
    /// The result is in the session's hands: it lands in the next update.
    Done,
}

/// [`Job::wait`] could not wait: the job is still the caller's, and still
/// running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum WaitError {
    /// The job waits for the main thread and the caller is not on it: call
    /// `wait`, or [`update`](Session::update), there.
    #[error("the job waits for the main thread and the caller is not on it")]
    NeedsMainThread,
    /// The job's result already landed and was taken: by the app from its
    /// [`SessionEvent::JobDone`], or by an earlier `wait`.
    #[error("the job's result was already taken")]
    AlreadyLanded,
}

/// Why a job's worker ended without a result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum WorkerError {
    /// The work panicked; `message` is the panic's, when it was a string.
    #[error("a session job panicked: {message}")]
    Panicked {
        /// The panic's message, or a placeholder for a payload of another
        /// type.
        message: String,
    },
    /// The executor dropped the job unrun or part-way (a shutdown, a host that
    /// discards the future), or the session ended the job unanswered.
    #[error("the executor dropped a session job without finishing it")]
    Dropped,
    /// The executor refused to start the job: the OS refused a thread. The work never
    /// ran.
    #[error("the executor could not start a session job: {0}")]
    NotStarted(std::io::ErrorKind),
}

const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const WAITING: u8 = 2;
const DONE: u8 = 3;

/// What a job and its ticket share.
pub(crate) struct Cell {
    status: AtomicU8,
    cancelled: AtomicBool,
    /// The ticket is gone: nothing more will land.
    ended: AtomicBool,
    /// The waker of the future awaiting the job: woken by the landing or the
    /// ticket's end. Control-side threads only.
    waker: Mutex<Option<Waker>>,
    /// The job was polled as a future: its landing waits in the queue for that
    /// poll, and the collect phase does not report it.
    awaited: AtomicBool,
}

impl Cell {
    /// Wakes the awaiting future, if there is one.
    fn wake(&self) {
        // Woken outside the lock: a `Waker` may do anything, including poll the job.
        let waker = self.waker.lock_recover().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The worker's view of its job's cancellation.
#[derive(Clone)]
pub(crate) struct Cancel(Arc<Cell>);

impl Cancel {
    /// The worker began: the job is [`Running`](JobStatus::Running).
    pub(crate) fn started(&self) {
        self.0.status.store(RUNNING, Ordering::Release);
    }

    /// The job was dropped: its result will not be read.
    #[cfg_attr(
        not(any(test, feature = "plugin", feature = "analyze")),
        expect(
            dead_code,
            reason = "the plugin state jobs (feature `plugin`) and analyze read it"
        )
    )]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
}

impl<J: JobKind> Job<J> {
    /// This job's identity: [`JobOutput::id`] of its landing.
    #[must_use]
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Where the job stands now.
    #[must_use]
    pub fn status(&self) -> JobStatus {
        match self.cell.status.load(Ordering::Acquire) {
            PENDING => JobStatus::Pending,
            RUNNING => JobStatus::Running,
            WAITING => JobStatus::WaitingForMainThread,
            _ => JobStatus::Done,
        }
    }

    /// Calls `session.update()` until this job finishes, and returns its result.
    /// The events of those updates stay queued for the app's next `update()`,
    /// in order: a wait delays them, it loses none.
    ///
    /// Waiting takes `&self`: an error does not cancel the job. Call it on the
    /// session that made the job.
    ///
    /// The main-thread stage of a job runs inside an `update`, so `wait` runs
    /// it only when called on the thread that holds the process claim
    /// ([`tutti_platform::MainThreadMarker::new`]). Called elsewhere on a job that
    /// is [`WaitingForMainThread`](JobStatus::WaitingForMainThread), it
    /// answers [`WaitError::NeedsMainThread`] and the job runs on.
    ///
    /// The result is taken: a later [`JobOutput::take`] of the job answers `None`,
    /// and a second `wait`, or one after the app took the job's
    /// [`SessionEvent::JobDone`], answers [`WaitError::AlreadyLanded`]. A worker
    /// that panicked lands as [`WorkerError::Panicked`], so a wait never hangs
    /// on it, nor on a job the executor dropped or whose session side ended
    /// unanswered ([`WorkerError::Dropped`]).
    ///
    /// # Errors
    ///
    /// The kind's own error, or a [`WorkerError`] or [`WaitError`] converted
    /// into it.
    pub fn wait<K: crate::AppKey, C: Clock>(
        &self,
        session: &mut Session<K, C>,
    ) -> Result<J::Output, J::Error> {
        loop {
            // Read before the update: an ended ticket has already queued its
            // landing, which that update then finds.
            let ended = self.cell.ended.load(Ordering::Acquire);
            if self.status() == JobStatus::WaitingForMainThread
                && tutti_platform::MainThreadMarker::new().is_none()
            {
                return Err(WaitError::NeedsMainThread.into());
            }
            let mut events = session.update();
            let landed = events.iter_mut().find_map(|event| match event {
                SessionEvent::JobDone(landed) => landed.take(self),
                _ => None,
            });
            // The landing is consumed: the app's next update does not see it.
            events.retain(|event| !matches!(event, SessionEvent::JobDone(l) if l.id == self.id));
            if let Some(result) = landed {
                session.jobs.hold(events);
                return result;
            }
            session.jobs.hold(events);
            if ended {
                return Err(if self.status() == JobStatus::Done {
                    WaitError::AlreadyLanded.into()
                } else {
                    WorkerError::Dropped.into()
                });
            }
            session.jobs.block(WAIT_SLICE);
        }
    }
}

impl<J: JobKind> Future for Job<J> {
    type Output = Result<J::Output, J::Error>;

    /// Resolves once an [`update`](Session::update) has landed the job, with what
    /// [`Job::wait`] returns. The landing wakes the task.
    ///
    /// Await a job before the update that lands it, or any update before the app
    /// takes its [`SessionEvent::JobDone`]: a landing the collect phase already
    /// reported, or that the app or a [`wait`](Job::wait) took, is gone, and the
    /// future answers [`WaitError::AlreadyLanded`].
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Registered before the checks: a landing or an end that follows finds the
        // waker, and one that came first is found by the checks.
        *this.cell.waker.lock_recover() = Some(cx.waker().clone());
        this.cell.awaited.store(true, Ordering::Release);
        let landed = {
            let mut queue = this.queue.lock_recover();
            let at = queue.iter().position(|landed| landed.id == this.id);
            at.map(|at| queue.remove(at))
        };
        if let Some(mut landed) = landed {
            return Poll::Ready(
                landed
                    .take(this)
                    .unwrap_or_else(|| Err(WaitError::AlreadyLanded.into())),
            );
        }
        if this.status() == JobStatus::Done {
            return Poll::Ready(Err(WaitError::AlreadyLanded.into()));
        }
        if this.cell.ended.load(Ordering::Acquire) {
            return Poll::Ready(Err(WorkerError::Dropped.into()));
        }
        Poll::Pending
    }
}

/// How long [`Job::wait`] blocks on the arrivals between two updates.
const WAIT_SLICE: Duration = Duration::from_millis(1);

impl<J: JobKind> Drop for Job<J> {
    fn drop(&mut self) {
        self.cell.cancelled.store(true, Ordering::Release);
        // A landing queued before the drop is discarded with it; one after it
        // sees `cancelled` under the same lock (`Ticket::land`). What is
        // discarded drops after the lock is released: a result may own a handle
        // (a Subscription) whose drop asks its port for something, which can end
        // a job and take this lock.
        let discarded = {
            let mut queue = self.queue.lock_recover();
            let (gone, kept) = std::mem::take(&mut *queue)
                .into_iter()
                .partition(|landed| landed.id == self.id);
            *queue = kept;
            gone
        };
        drop(discarded);
    }
}

impl<J: JobKind> fmt::Debug for Job<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("status", &self.status())
            .finish()
    }
}

/// The result of a job, as [`SessionEvent::JobDone`] reports it, once. The
/// result is type-erased and keyed by [`JobId`]: only [`take`](Self::take) with
/// the job it belongs to returns it.
pub struct JobOutput {
    id: JobId,
    result: Option<Box<dyn Any + Send>>,
    cell: Arc<Cell>,
}

impl JobOutput {
    /// A [`Job`] is polled as a future: it takes this landing itself.
    pub(crate) fn awaited(&self) -> bool {
        self.cell.awaited.load(Ordering::Acquire)
    }

    /// The job that landed.
    #[must_use]
    pub fn id(&self) -> JobId {
        self.id
    }

    /// The result of `job`, when this is its landing and it was not taken: a
    /// second call, and a call with another job, answer `None`.
    pub fn take<J: JobKind>(&mut self, job: &Job<J>) -> Option<Result<J::Output, J::Error>> {
        if self.id != job.id {
            return None;
        }
        // Checked before it moves out, so a mismatch never destroys the result.
        if !self.result.as_ref()?.is::<Result<J::Output, J::Error>>() {
            return None;
        }
        self.result
            .take()?
            .downcast::<Result<J::Output, J::Error>>()
            .ok()
            .map(|result| *result)
    }
}

impl fmt::Debug for JobOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobOutput")
            .field("id", &self.id)
            .field("taken", &self.result.is_none())
            .finish()
    }
}

/// What landed since the last update: the session's end of every job's
/// result, drained into [`SessionEvent::JobDone`] by the collect phase.
pub(crate) type JobOutputQueue = Arc<Mutex<Vec<JobOutput>>>;

/// The session's end of a [`Job`]: lands its result once.
pub(crate) struct Ticket<J: JobKind> {
    id: JobId,
    cell: Arc<Cell>,
    queue: JobOutputQueue,
    _kind: PhantomData<fn() -> J>,
}

impl<J: JobKind> Ticket<J> {
    pub(crate) fn new(queue: &JobOutputQueue) -> (Job<J>, Self) {
        let id = JobId(NEXT_ID.mint().get());
        let cell = Arc::new(Cell {
            status: AtomicU8::new(PENDING),
            cancelled: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            waker: Mutex::new(None),
            awaited: AtomicBool::new(false),
        });
        (
            Job {
                id,
                cell: Arc::clone(&cell),
                queue: Arc::clone(queue),
                _kind: PhantomData,
            },
            Self {
                id,
                cell,
                queue: Arc::clone(queue),
                _kind: PhantomData,
            },
        )
    }

    pub(crate) fn cancel(&self) -> Cancel {
        Cancel(Arc::clone(&self.cell))
    }

    /// The job was dropped: nothing will read what lands.
    pub(crate) fn cancelled(&self) -> bool {
        self.cell.cancelled.load(Ordering::Acquire)
    }

    #[cfg_attr(
        not(any(test, feature = "plugin")),
        expect(
            dead_code,
            reason = "the plugin restore (feature `plugin`) is its user"
        )
    )]
    pub(crate) fn waiting_for_main_thread(&self) {
        self.cell.status.store(WAITING, Ordering::Release);
    }

    /// Marks the job as waiting for the main thread (`true`) or running
    /// (`false`), while it is not done.
    pub(crate) fn set_main_wait(&self, waiting: bool) {
        self.cell
            .status
            .store(if waiting { WAITING } else { RUNNING }, Ordering::Release);
    }

    /// Runs `effect` and lands its result, unless the job was dropped first:
    /// then `effect` does not run. A drop after the check lets `effect` finish
    /// (it is atomic with respect to the drop) and its result is discarded.
    pub(crate) fn complete(self, effect: impl FnOnce() -> Result<J::Output, J::Error>) {
        if !self.cancelled() {
            let result = effect();
            self.land(result);
        }
    }

    /// Queues the result to land in the next update, unless the job was
    /// dropped.
    pub(crate) fn land(self, result: Result<J::Output, J::Error>) {
        {
            let mut queue = self.queue.lock_recover();
            // Under the lock the drop's purge takes: a landing is queued before
            // the purge (and purged) or sees the drop here.
            if self.cancelled() {
                return;
            }
            self.cell.status.store(DONE, Ordering::Release);
            queue.push(JobOutput {
                id: self.id,
                result: Some(Box::new(result)),
                cell: Arc::clone(&self.cell),
            });
        }
        self.cell.wake();
    }
}

impl<J: JobKind> Drop for Ticket<J> {
    fn drop(&mut self) {
        self.cell.ended.store(true, Ordering::Release);
        self.cell.wake();
    }
}

#[cfg(test)]
mod tests;
