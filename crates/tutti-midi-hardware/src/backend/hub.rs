//! What a backend with a thread of its own to start shares: where its start
//! stands, the futures waiting on it, and the device watches registered with it.
//!
//! Nothing here reads the platform, so it is tested everywhere; only the CoreMIDI
//! backend uses it as a backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Poll, Waker};
use tutti_platform::Instant;
use tutti_rt::Mutex;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::{MidiHardwareError, MidiThread};
use crate::start::{Start, Waiting};
use tutti_midi_device::OsDevice;

/// Where a backend's start stands. A start passes through it on the backend's
/// thread; every method is a transition.
#[derive(Clone, Debug)]
pub(crate) enum HubState {
    /// The thread is making the client. `generation` counts the rounds.
    Starting {
        generation: u64,
        /// When a window of retries ended in failure, carried into this round.
        gave_up: Option<Instant>,
    },
    /// The client is made. Terminal.
    Ready,
    /// The round ended in `error`.
    Failed {
        generation: u64,
        error: Arc<MidiHardwareError>,
        /// When a window of retries ended in failure, if this round's did.
        gave_up: Option<Instant>,
    },
}

/// The round a future waits on, and whether making it started that round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Round {
    /// The round a future made now waits on.
    pub(crate) generation: u64,
    /// Whether this call started a new round, so the thread must be told.
    pub(crate) restarted: bool,
}

impl HubState {
    /// The first round, under way.
    pub(crate) fn new() -> Self {
        Self::Starting {
            generation: 0,
            gave_up: None,
        }
    }

    /// A future is made: it waits on the round under way, or, where the last round
    /// failed, on a new one that carries the window forward. `client_made` says the
    /// client exists, which ends every round: the state is then `Ready`. Returns
    /// whether the state changed to `Ready`, which the futures waiting must hear.
    pub(crate) fn begin(&mut self, client_made: bool) -> (Round, bool) {
        let ended = client_made && !matches!(self, Self::Ready);
        if client_made {
            *self = Self::Ready;
        }
        let round = match self {
            Self::Starting { generation, .. } => Round {
                generation: *generation,
                restarted: false,
            },
            Self::Ready => Round {
                generation: 0,
                restarted: false,
            },
            Self::Failed {
                generation,
                gave_up,
                ..
            } => {
                let next = *generation + 1;
                *self = Self::Starting {
                    generation: next,
                    gave_up: *gave_up,
                };
                Round {
                    generation: next,
                    restarted: true,
                }
            }
        };
        (round, ended)
    }

    /// Whether the client is made.
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The client is made. Returns whether the state changed: `Ready` is terminal.
    pub(crate) fn ready(&mut self) -> bool {
        match self {
            Self::Starting { .. } => {
                *self = Self::Ready;
                true
            }
            Self::Ready | Self::Failed { .. } => false,
        }
    }

    /// The round under way failed. `gave_up` is when its window ended, or else the
    /// window it was carrying. Returns whether the state changed.
    pub(crate) fn failed(
        &mut self,
        error: Arc<MidiHardwareError>,
        gave_up: Option<Instant>,
    ) -> bool {
        match self {
            Self::Starting { generation, .. } => {
                *self = Self::Failed {
                    generation: *generation,
                    error,
                    gave_up,
                };
                true
            }
            Self::Ready | Self::Failed { .. } => false,
        }
    }

    /// The backend's thread ended before it answered. Where the client exists the
    /// round is over and the state is `Ready`; otherwise the round failed with
    /// [`MidiHardwareError::ThreadEnded`]. A thread that ends after `Ready`, or
    /// after the round failed, changes nothing. Returns whether the state changed.
    pub(crate) fn ended(&mut self, client_made: bool) -> bool {
        let gave_up = match self {
            Self::Starting { gave_up, .. } => *gave_up,
            Self::Ready | Self::Failed { .. } => return false,
        };
        if client_made {
            return self.ready();
        }
        self.failed(
            Arc::new(MidiHardwareError::ThreadEnded {
                thread: MidiThread::DeviceWatch,
            }),
            gave_up,
        )
    }

    /// What a future that began at generation `from` has: `None` while its round
    /// is under way, the error of a round no earlier than its own.
    pub(crate) fn outcome(&self, from: u64) -> Option<Result<(), Arc<MidiHardwareError>>> {
        match self {
            Self::Ready => Some(Ok(())),
            Self::Failed {
                generation, error, ..
            } if *generation >= from => Some(Err(Arc::clone(error))),
            Self::Starting { .. } | Self::Failed { .. } => None,
        }
    }

    /// The round the backend's thread should run, when it has not run it: `done` is
    /// the generation it last ran. Its generation, and the window it carries.
    pub(crate) fn wanted(&self, done: Option<u64>) -> Option<(u64, Option<Instant>)> {
        match self {
            Self::Starting {
                generation,
                gave_up,
            } if done != Some(*generation) => Some((*generation, *gave_up)),
            _ => None,
        }
    }
}

/// The wakers a state change took from the gate. Waking them runs arbitrary code,
/// so a caller wakes them after it has released every lock it holds.
#[must_use = "wake them after releasing every lock, or the futures waiting are never woken"]
pub(crate) struct Wakers(Vec<Waker>);

impl Wakers {
    /// Wakes every waiting future.
    pub(crate) fn wake(self) {
        self.0.into_iter().for_each(Waker::wake);
    }
}

/// A [`HubState`] and the wakers of the futures waiting on it. It wakes nothing
/// itself: every transition returns the wakers to wake, and the state only changes
/// through [`transition`](Self::transition), which takes them whenever it changed.
pub(crate) struct StartGate {
    cell: Mutex<(HubState, Vec<Waker>)>,
}

impl StartGate {
    /// A gate whose first round is under way.
    pub(crate) fn new() -> Self {
        Self {
            cell: Mutex::new((HubState::new(), Vec::new())),
        }
    }

    /// Runs `change` on the state. When it says the state changed, the waiting
    /// futures' wakers are returned to be woken, whatever the change was.
    fn transition<R>(&self, change: impl FnOnce(&mut HubState) -> (R, bool)) -> (R, Wakers) {
        let mut cell = self.cell.lock();
        let (result, changed) = change(&mut cell.0);
        let wakers = if changed {
            std::mem::take(&mut cell.1)
        } else {
            Vec::new()
        };
        (result, Wakers(wakers))
    }

    /// A future is made. See [`HubState::begin`].
    pub(crate) fn begin(&self, client_made: bool) -> (Round, Wakers) {
        self.transition(|state| state.begin(client_made))
    }

    /// Whether the client is made.
    pub(crate) fn is_ready(&self) -> bool {
        self.cell.lock().0.is_ready()
    }

    /// The client is made.
    pub(crate) fn publish_ready(&self) -> Wakers {
        self.transition(|state| ((), state.ready())).1
    }

    /// The round failed.
    pub(crate) fn publish_failed(
        &self,
        error: Arc<MidiHardwareError>,
        gave_up: Option<Instant>,
    ) -> Wakers {
        self.transition(|state| ((), state.failed(error, gave_up)))
            .1
    }

    /// The backend's thread ended without answering: see [`HubState::ended`].
    pub(crate) fn ended(&self, client_made: bool) -> Wakers {
        self.transition(|state| ((), state.ended(client_made))).1
    }

    /// See [`HubState::wanted`].
    pub(crate) fn wanted(&self, done: Option<u64>) -> Option<(u64, Option<Instant>)> {
        self.cell.lock().0.wanted(done)
    }

    /// A start over this gate, waiting on `round`.
    pub(crate) fn start(self: &Arc<Self>, round: Round) -> Start {
        Start::over(Arc::clone(self) as Arc<dyn Waiting>, round.generation)
    }
}

impl Waiting for StartGate {
    fn poll(&self, from: u64, waker: &Waker) -> Poll<Result<(), Arc<MidiHardwareError>>> {
        let mut cell = self.cell.lock();
        if let Some(outcome) = cell.0.outcome(from) {
            return Poll::Ready(outcome);
        }
        if !cell.1.iter().any(|stored| stored.will_wake(waker)) {
            cell.1.push(waker.clone());
        }
        Poll::Pending
    }
}

/// A start that failed before it could wait.
struct Refused(Arc<MidiHardwareError>);

impl Waiting for Refused {
    fn poll(&self, _from: u64, _waker: &Waker) -> Poll<Result<(), Arc<MidiHardwareError>>> {
        Poll::Ready(Err(Arc::clone(&self.0)))
    }
}

/// A start that failed with `error` before it could wait.
pub(crate) fn failed_start(error: MidiHardwareError) -> Start {
    Start::over(Arc::new(Refused(Arc::new(error))), 0)
}

/// One registered watch: the reporting state of a `Devices`, and the sink to
/// tell if it fails.
struct Watcher {
    reporter: Reporter,
    snapshot: fn() -> Vec<OsDevice>,
    sink: ChangeSink,
    /// Whether the device list has been seeded.
    seeded: bool,
    /// False once its `Devices` has dropped.
    live: bool,
}

type Shared = Arc<Mutex<Watcher>>;

/// Looks again and reports what changed, seeding the watch first if it has not
/// been. A panic does not unwind into the OS's block: it fails the watch.
///
/// The registry's own lock is not held here. The watcher's enumeration calls into
/// the OS, and a thread holding the registry lock across such a call would block
/// the notification callback that the OS may be waiting on.
fn look(watcher: &Shared) {
    let mut watcher = watcher.lock();
    if !watcher.live {
        return;
    }
    let Watcher {
        reporter,
        snapshot,
        seeded,
        ..
    } = &mut *watcher;
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if *seeded {
            reporter.report(snapshot(), |_| false);
            Ok(())
        } else {
            let seeded_now = reporter.seed(snapshot());
            *seeded = seeded_now.is_ok();
            seeded_now
        }
    }));
    match run {
        Ok(Ok(())) => {}
        Ok(Err(seed)) => {
            watcher.live = false;
            watcher.sink.fail(MidiHardwareError::from(seed));
        }
        Err(_) => {
            watcher.live = false;
            watcher.sink.fail(crate::WatchFailure::Panicked);
        }
    }
}

/// The watches registered with a backend whose enumeration needs its client. A
/// watch is seeded when the client exists and stays registered across a failed
/// round: a later round that makes the client seeds it.
pub(crate) struct Registry {
    watches: Mutex<Vec<(u64, Shared)>>,
    next: AtomicU64,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            watches: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }

    fn all(&self) -> Vec<Shared> {
        self.watches
            .lock()
            .iter()
            .map(|(_, w)| Arc::clone(w))
            .collect()
    }

    /// Has every registered watch look again, seeding the ones not yet seeded.
    pub(crate) fn look_all(&self) {
        for watcher in &self.all() {
            look(watcher);
        }
    }

    /// What an OS notification asks of the watches: one that `changes_devices`
    /// has every watch look again, any other asks nothing.
    pub(crate) fn notified(&self, changes_devices: bool) {
        if changes_devices {
            self.look_all();
        }
    }

    /// How many watches are registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.watches.lock().len()
    }

    /// Registers `sink`, which reports `snapshot()` against what it reported before.
    /// `client_made` is asked after the registration is listed: where it says the
    /// client exists the watch is seeded now, and otherwise by the look that follows
    /// the client being made. The returned watch ends the registration when
    /// dropped, and returns once a look in progress has finished (it takes the
    /// watch's own lock, which an enumeration holds): nothing is reported after.
    pub(crate) fn register(
        self: &Arc<Self>,
        sink: ChangeSink,
        snapshot: fn() -> Vec<OsDevice>,
        client_made: impl FnOnce() -> bool,
    ) -> Box<dyn Watch> {
        let watcher: Shared = Arc::new(Mutex::new(Watcher {
            reporter: Reporter::new(sink.clone()),
            snapshot,
            sink,
            seeded: false,
            live: true,
        }));
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        self.watches.lock().push((key, Arc::clone(&watcher)));
        if client_made() {
            look(&watcher);
        }
        Box::new(Registration {
            registry: Arc::clone(self),
            key,
            watcher,
        })
    }
}

/// A registration with a [`Registry`]. Dropping it removes the registration.
struct Registration {
    registry: Arc<Registry>,
    key: u64,
    watcher: Shared,
}

impl Watch for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .watches
            .lock()
            .retain(|(key, _)| *key != self.key);
        self.watcher.lock().live = false;
    }
}

#[cfg(test)]
mod tests;
