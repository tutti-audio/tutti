//! Watching the host's devices and deciding what to do when they change.
//!
//! [`Follow`] is the decision, a pure step: the time and the roster's view
//! are values. [`DeviceWatch`] is what the session runs on `Phase::Poll`: a
//! [`SnapshotSource`], a [`DeviceRoster`] and a `Follow`. Enumeration is
//! [`DevicePoller`]'s look, run as a task on the session's
//! executor (each look on its blocking pool), and the diff of two listings is the
//! [`DeviceRoster`]'s; neither runs here.
//!
//! # The policy
//!
//! - Everything that moves the stream is opt-in ([`DevicePolicy`]), and the
//!   session never replaces a device the user chose with another one: a lost
//!   explicit device waits, silent, for its own return or for the host.
//! - The session decides and the host executes. A `Device` is `!Send` and the
//!   host owns it, so a poll reports [`DeviceEvent::RestartWanted`] and the
//!   host calls `Session::restart` with the request, or ignores it to
//!   decline (a strike is counted). Every restart, wanted or the host's own,
//!   is reported by the next poll as [`DeviceEvent::Restarted`] or
//!   [`DeviceEvent::RestartFailed`]. A restart failed by the policy keeps the
//!   old spec and is tried again after the [`Backoff`]'s wait.
//! - A policy restart never makes a choice: the request goes through the same
//!   call as the host's, which drops it as stale when the host has chosen
//!   another device since.
//! - A stream is lost only by a fault ([`SessionEvent::Fault`]). Absence from a
//!   listing never ends it: ALSA opens every PCM to enumerate, so a device
//!   this process holds can drop out of the list while it plays.
//!   [`DeviceEvent::Lost`] and [`DeviceEvent::Appeared`] mean "no longer
//!   listed" and "listed again".
//! - An unknown device is not a different one. The device a stream runs on is
//!   `None` when the host cannot name it (a driver of the host's own), and
//!   `None` is never compared with the default, so a healthy stream is not
//!   restarted.
//! - On bare ALSA the default's name is constant (`default`), so
//!   [`DeviceEvent::DefaultChanged`] cannot fire there and following the
//!   default moves the stream only after a fault.
//!
//! None of this has run against real hardware, and the cost of enumeration on
//! WASAPI, CoreAudio and JACK is unmeasured, which is why the poll interval is
//! the host's to choose ([`DevicePoller::new`](tutti_cpal::DevicePoller::new)).

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tutti_platform::{Executor, Instant, Task, WeakExecutor};

use tutti_cpal::{
    DeviceChange, DeviceError, DeviceId, DeviceListing, DevicePoller, DeviceRoster, DeviceSelector,
    Snapshot, SnapshotSource,
};

use crate::{DeviceEvent, DeviceRestart, SessionEvent};

/// How the retries of one device are spaced.
///
/// Every attempt counts a strike, whatever came of it. The wait before the
/// next is `base * 2^(strikes - 1)`, at most `max`; strikes clear after
/// `quiet` once that wait is over. A device that flaps faster than
/// `quiet` is therefore retried every `max`, not on every change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first attempt.
    pub base: Duration,
    /// The longest wait.
    pub max: Duration,
    /// How long without an attempt (after the wait) clears the strikes.
    pub quiet: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            quiet: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    fn wait(&self, strikes: u32) -> Duration {
        let doublings = strikes.saturating_sub(1).min(20);
        self.base.saturating_mul(1 << doublings).min(self.max)
    }
}

/// What the session may do on its own when devices change. Everything that
/// moves the stream is off by default; the session never replaces an explicit
/// choice with another device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevicePolicy {
    /// A stream whose selector is `Default` moves when the default output
    /// changes, or when it was lost (a disconnect fault).
    ///
    /// A device missing from the list is **not** a lost stream: ALSA opens
    /// every PCM to enumerate, so a device this process holds can drop out of
    /// the list while it plays. Only a fault ends a stream. On bare ALSA the
    /// default's name is constant (`default`), so the default never
    /// "changes" and only a fault can move the stream. Opening every PCM
    /// every poll is a cost the host opts into, and a restart can meet `EBUSY` from that probe: a counted
    /// strike, then the backoff. A host that is recording should leave this
    /// off, or hold restarts ([`Session::pause_device_restarts`](crate::Session::pause_device_restarts)):
    /// a restart can change the rate mid-take.
    pub follow_default: bool,
    /// The device the host chose explicitly is reopened when it returns.
    pub reconnect: bool,
    /// A start that failed is retried when its device is listed
    /// (a [`Follow::not_started`] policy).
    pub retry_startup: bool,
    /// How retries are spaced.
    pub backoff: Backoff,
}

/// What [`Follow::step`] is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowEvent {
    /// The host selected this device on purpose.
    Chose(DeviceSelector),
    /// The stream is open on this device (`None`: its name is unknown).
    Opened(Option<DeviceId>),
    /// The stream died (a disconnect fault).
    Lost,
    /// A restart failed; `running`: the stream still plays (it was refused
    /// before it stopped).
    RestartFailed {
        /// Whether the old stream still plays.
        running: bool,
    },
    /// Time passed, or the roster changed: look again.
    Look,
}

/// The restart policy as a value: what the user chose, what runs, and when
/// the next attempt may be. [`step`](Self::step) is pure.
#[derive(Clone, Debug)]
pub struct Follow {
    policy: DevicePolicy,
    want: DeviceSelector,
    running_on: Option<DeviceId>,
    started: bool,
    broken: bool,
    held: bool,
    strikes: u32,
    not_before: Option<Instant>,
}

impl Follow {
    /// A stream that is open on `running_on` (`None`: unknown) for the
    /// selector `want`.
    #[must_use]
    pub fn running(
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) -> Self {
        Self {
            policy,
            want,
            running_on,
            started: true,
            broken: false,
            held: false,
            strikes: 0,
            not_before: None,
        }
    }

    /// No stream yet: a start that failed, to retry.
    #[must_use]
    pub fn not_started(policy: DevicePolicy, want: DeviceSelector) -> Self {
        Self {
            started: false,
            broken: true,
            ..Self::running(policy, want, None)
        }
    }

    /// While held, no restart is asked for (events still update the state).
    /// A host that must not have the stream restarted under it (a take being
    /// recorded) holds it.
    pub fn hold(&mut self, held: bool) {
        self.held = held;
    }

    /// The selector the user chose.
    #[must_use]
    pub fn want(&self) -> &DeviceSelector {
        &self.want
    }

    /// Takes `event`, looks at `view` (the devices now) at `now`, and says
    /// which restart to run, if one is due. A restart returned counts a
    /// strike and holds the next for its backoff.
    pub fn step(
        &mut self,
        event: FollowEvent,
        view: &Snapshot,
        now: Instant,
    ) -> Option<DeviceRestart> {
        self.take(event);
        self.decide(view, now)
    }

    /// Takes `event` in without deciding: for an event that arrives before
    /// any look at the devices.
    fn take(&mut self, event: FollowEvent) {
        match event {
            FollowEvent::Chose(sel) => {
                if sel != self.want {
                    self.want = sel;
                    self.strikes = 0;
                    self.not_before = None;
                }
            }
            FollowEvent::Opened(id) => {
                self.running_on = id;
                self.started = true;
                self.broken = false;
            }
            FollowEvent::Lost => self.broken = true,
            FollowEvent::RestartFailed { running } => self.broken |= !running,
            FollowEvent::Look => {}
        }
    }

    fn decide(&mut self, view: &Snapshot, now: Instant) -> Option<DeviceRestart> {
        if self
            .not_before
            .is_some_and(|t| now >= t + self.policy.backoff.quiet)
        {
            self.strikes = 0;
            self.not_before = None;
        }
        if self.held {
            return None;
        }
        let device = self.goal(view)?;
        if self.not_before.is_some_and(|t| now < t) {
            return None;
        }
        self.strikes += 1;
        self.not_before = Some(now + self.policy.backoff.wait(self.strikes));
        Some(DeviceRestart::wanted(device))
    }

    fn goal(&self, view: &Snapshot) -> Option<DeviceSelector> {
        let broken = self.broken;
        if !self.started {
            let listed = match &self.want {
                DeviceSelector::Default => view.default_output.is_some(),
                DeviceSelector::Id(id) => view.outputs.contains(id),
            };
            return (self.policy.retry_startup && listed).then(|| self.want.clone());
        }
        match &self.want {
            DeviceSelector::Default => {
                let default = view.default_output.as_ref()?;
                // An unknown device (`None`) is not a different one.
                let moved = self.running_on.as_ref().is_some_and(|d| d != default);
                (self.policy.follow_default && (broken || moved)).then_some(DeviceSelector::Default)
            }
            DeviceSelector::Id(id) => {
                (self.policy.reconnect && broken && view.outputs.contains(id))
                    .then(|| self.want.clone())
            }
        }
    }
}

/// A source, its roster and the policy: what a session runs on `Phase::Poll`
/// ([`Session::watch_devices`](crate::Session::watch_devices)), or, with no
/// session yet, what an app polls until a device appears
/// ([`without_stream`](Self::without_stream)).
pub struct DeviceWatch {
    source: Box<dyn SnapshotSource + Send>,
    roster: DeviceRoster,
    follow: Follow,
    looked: bool,
}

impl std::fmt::Debug for DeviceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceWatch")
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

impl DeviceWatch {
    /// Watches for a stream open on `running_on` for the selector `want`.
    pub fn new(
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) -> Self {
        Self {
            source: Box::new(source),
            roster: DeviceRoster::new(),
            follow: Follow::running(policy, want, running_on),
            looked: false,
        }
    }

    /// Watches for a stream that never started: [`retry_start`](Self::retry_start)
    /// starts it once `want` is listed (with `retry_startup` on).
    pub fn without_stream(
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
    ) -> Self {
        Self {
            source: Box::new(source),
            roster: DeviceRoster::new(),
            follow: Follow::not_started(policy, want),
            looked: false,
        }
    }

    /// The source, to hand to the session that takes over once the start
    /// succeeds ([`Session::watch_devices`](crate::Session::watch_devices)).
    #[must_use]
    pub fn into_source(self) -> Box<dyn SnapshotSource + Send> {
        self.source
    }

    /// For a watch with no stream: polls at the current time, and when the
    /// policy asks for a start, runs `start` with the selector. A failure
    /// is told to the policy, which waits out its backoff. `events` gets the
    /// device events the poll found (for a list). `None`: nothing was due.
    pub fn retry_start<T, E>(
        &mut self,
        events: &mut Vec<SessionEvent>,
        start: impl FnOnce(DeviceSelector) -> std::result::Result<T, E>,
    ) -> Option<std::result::Result<T, E>> {
        let now = Instant::now();
        self.poll(now, events);
        let at = events
            .iter()
            .position(|e| matches!(e, SessionEvent::Device(DeviceEvent::RestartWanted(_))))?;
        let SessionEvent::Device(DeviceEvent::RestartWanted(request)) = events.remove(at) else {
            return None;
        };
        let result = start(request.device);
        if result.is_err() {
            self.note(FollowEvent::RestartFailed { running: false }, now);
        }
        Some(result)
    }

    /// Tells the policy something happened without deciding, for a decision
    /// that waits for the next [`poll`](Self::poll)'s fresh look.
    pub fn observe(&mut self, event: FollowEvent) {
        self.follow.take(event);
    }

    /// While held, no restart is asked for ([`Follow::hold`]).
    pub fn hold(&mut self, held: bool) {
        self.follow.hold(held);
    }

    /// The devices as last seen.
    #[must_use]
    pub fn roster(&self) -> &DeviceRoster {
        &self.roster
    }

    /// The policy state.
    #[must_use]
    pub fn follow(&self) -> &Follow {
        &self.follow
    }

    /// Tells the policy something happened to the stream.
    pub fn note(&mut self, event: FollowEvent, now: Instant) -> Option<DeviceRestart> {
        if self.looked {
            self.follow.step(event, self.roster.snapshot(), now)
        } else {
            self.follow.take(event);
            None
        }
    }

    /// Takes the newest snapshot, reports what changed, and asks the policy
    /// whether to restart. Before the first snapshot nothing is decided.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<SessionEvent>) {
        if let Some(snapshot) = self.source.poll() {
            self.looked = true;
            for change in self.roster.apply(snapshot) {
                out.push(SessionEvent::Device(match change {
                    DeviceChange::Appeared(direction, id) => {
                        DeviceEvent::Appeared { direction, id }
                    }
                    DeviceChange::Vanished(direction, id) => DeviceEvent::Lost { direction, id },
                    DeviceChange::DefaultChanged(direction, id) => {
                        DeviceEvent::DefaultChanged { direction, id }
                    }
                }));
            }
        }
        if self.looked
            && let Some(request) = self.note(FollowEvent::Look, now)
        {
            out.push(SessionEvent::Device(DeviceEvent::RestartWanted(request)));
        }
    }
}

/// How long a listing of the output devices may wait before the session
/// reports it slow ([`DeviceEvent::OutputsListSlow`]): a healthy host
/// answers in milliseconds, and one that has not in this long is likely stuck.
pub(crate) const LISTING_WARNING: Duration = Duration::from_secs(5);

/// A [`DeviceListing`] being waited for: the blocking pool's task and the
/// policy around it. The time is a value, as in [`Follow::step`].
#[derive(Debug)]
pub(crate) struct OutputListing {
    state: ListingState,
    /// The `now` of the first poll.
    since: Option<Instant>,
    warned: bool,
}

/// Where a listing is.
enum ListingState {
    /// `list` runs on the blocking pool; a panic in it is the task's
    /// [`DeviceError::ListingEnded`].
    Running(Task<Result<Vec<DeviceId>, DeviceError>>),
    /// The executor refused the task: the failure waiting to be reported.
    Refused(Option<DeviceError>),
}

impl std::fmt::Debug for ListingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running(_) => "Running",
            Self::Refused(_) => "Refused",
        })
    }
}

impl OutputListing {
    /// Starts `listing`'s one blocking step on `executor`'s blocking pool.
    pub(crate) fn new(executor: &Executor, listing: DeviceListing) -> Self {
        let step = move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listing.list()))
                .unwrap_or(Err(DeviceError::ListingEnded))
        };
        let state = match executor.unblock(step) {
            Ok(task) => ListingState::Running(task),
            Err(e) => ListingState::Refused(Some(DeviceError::InvalidConfig(format!(
                "cannot start the device listing: {e}"
            )))),
        };
        Self {
            state,
            since: None,
            warned: false,
        }
    }

    /// The event this poll has, and whether the listing is over.
    pub(crate) fn poll(&mut self, now: Instant) -> (Option<SessionEvent>, bool) {
        let answer = match &mut self.state {
            ListingState::Refused(why) => why.take().map(Err),
            ListingState::Running(task) if task.is_finished() => {
                let mut cx = Context::from_waker(Waker::noop());
                match Pin::new(task).poll(&mut cx) {
                    Poll::Ready(listed) => Some(listed),
                    Poll::Pending => None,
                }
            }
            ListingState::Running(_) => None,
        };
        match answer {
            Some(Ok(devices)) => (
                Some(SessionEvent::Device(DeviceEvent::OutputsListed(devices))),
                true,
            ),
            Some(Err(why)) => (
                Some(SessionEvent::Device(DeviceEvent::OutputsListFailed(why))),
                true,
            ),
            None => {
                let since = *self.since.get_or_insert(now);
                let waited = now.saturating_duration_since(since);
                let slow = waited >= LISTING_WARNING && !self.warned;
                self.warned |= slow;
                (
                    slow.then_some(SessionEvent::Device(DeviceEvent::OutputsListSlow(waited))),
                    false,
                )
            }
        }
    }
}

/// The snapshots of a [`DevicePoller`] loop running on the session's executor.
///
/// Dropping it drops the loop's task, which cancels the loop; a look already
/// on the blocking pool finishes on its own and its answer is discarded, so
/// dropping never waits on a host that does not answer.
#[derive(Debug)]
pub(crate) struct PolledSource {
    snapshots: Receiver<Snapshot>,
    _task: Task<()>,
}

impl PolledSource {
    /// Starts the loop of `poller` on `executor`: a look on the blocking pool,
    /// the snapshot it returned sent on, then a sleep of the poller's
    /// interval.
    pub(crate) fn start(executor: &Executor, poller: DevicePoller) -> Self {
        let (tx, snapshots) = mpsc::channel();
        let task = executor.spawn(poll_loop(
            executor.downgrade(),
            executor.timer(),
            poller,
            tx,
        ));
        Self {
            snapshots,
            _task: task,
        }
    }
}

impl SnapshotSource for PolledSource {
    fn poll(&mut self) -> Option<Snapshot> {
        self.snapshots.try_iter().last()
    }
}

/// The poller's loop: ends when the session or its executor is gone, the
/// blocking pool refuses a look, or the source drops (a look that panics ends
/// the task, which the executor drops).
async fn poll_loop(
    executor: WeakExecutor,
    timer: tutti_platform::Timer,
    mut poller: DevicePoller,
    snapshots: mpsc::Sender<Snapshot>,
) {
    let interval = poller.interval();
    loop {
        let step = match executor.upgrade() {
            Some(executor) => executor.unblock(move || {
                let looked = poller.look();
                (poller, looked)
            }),
            None => return,
        };
        let Ok(step) = step else { return };
        let (back, looked) = step.await;
        poller = back;
        // A failed look is tried again at the next tick.
        if let Ok(Some(snapshot)) = looked
            && snapshots.send(snapshot).is_err()
        {
            return;
        }
        if timer.sleep(interval.get()).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
