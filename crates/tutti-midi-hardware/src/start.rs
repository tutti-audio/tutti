//! Waiting for a backend to be ready to open endpoints.
//!
//! A backend may have to start something before it can create an endpoint or open a
//! port: CoreMIDI's one client is made on a thread of the backend's own, and the MIDI
//! server it connects to may be restarting for several seconds. That wait ends when
//! the backend's thread says so, so it is a [`Future`]: [`Start`], which another
//! thread wakes, with no executor of this crate behind it. A backend that has
//! nothing to start is ready at once.
//!
//! [`Started`] is what the future resolves to: the proof a caller holds that the
//! backend is ready, which opening a virtual endpoint takes.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tutti_platform::Instant;
use tutti_rt::LockExt;

use crate::error::{MidiHardwareError, MidiThread};

/// The wait for this platform's backend to be ready, with no session: what
/// [`MidiEndpoints::start`](crate::MidiEndpoints::start) is for the backend a
/// [`Devices`](crate::Devices) holds, and the same wait (a platform has one
/// backend, started once per process). A virtual endpoint is opened with the
/// [`Started`] it resolves to.
pub fn start() -> Start {
    crate::backend::start()
}

/// The proof that the backend is ready to open endpoints, which [`Start`] resolves
/// to. Only a finished start makes one.
#[derive(Clone, Copy, Debug)]
pub struct Started(());

/// The wait for the backend to be ready: a `Send` future that another thread
/// wakes. [`MidiEndpoints::start`](crate::MidiEndpoints::start),
/// [`Devices::start`](crate::Devices::start) and
/// [`MidiSession::start`](crate::MidiSession::start) make one.
///
/// It resolves to [`Started`], or to the error the backend failed to start with.
/// A backend with nothing to start (ALSA, Windows Multimedia, the stub) is ready on
/// the first poll. On macOS the process's one CoreMIDI client is made on a thread of
/// the backend's own, and this resolves when that thread has made it.
///
/// ```
/// fn crosses_threads<T: Send>() {}
/// crosses_threads::<tutti_midi_hardware::Start>();
/// ```
#[must_use = "a start does nothing until it is awaited"]
pub struct Start {
    wait: Wait,
}

enum Wait {
    /// Nothing to start.
    Ready,
    /// The backend failed before it could wait.
    Failed(Arc<MidiHardwareError>),
    /// The backend's thread publishes the outcome to the gate.
    Gate { gate: Arc<StartGate>, from: u64 },
}

impl core::fmt::Debug for Start {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Start").finish_non_exhaustive()
    }
}

impl Start {
    /// A start that is ready on the first poll: the default of a backend that
    /// has nothing to start.
    pub(crate) fn ready() -> Self {
        Self { wait: Wait::Ready }
    }

    /// A start that failed before it could wait.
    #[cfg_attr(
        not(any(test, midi_backend = "coremidi")),
        allow(
            dead_code,
            reason = "only the CoreMIDI backend has a start to wait for"
        )
    )]
    pub(crate) fn failed(error: MidiHardwareError) -> Self {
        Self {
            wait: Wait::Failed(Arc::new(error)),
        }
    }

    /// A start over `gate`, from the start `begun` names.
    #[cfg_attr(
        not(any(test, midi_backend = "coremidi")),
        allow(
            dead_code,
            reason = "only the CoreMIDI backend has a start to wait for"
        )
    )]
    pub(crate) fn over(gate: Arc<StartGate>, begun: Begun) -> Self {
        Self {
            wait: Wait::Gate {
                gate,
                from: begun.generation,
            },
        }
    }
}

impl Future for Start {
    type Output = Result<Started, MidiHardwareError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &self.wait {
            Wait::Ready => Poll::Ready(Ok(Started(()))),
            Wait::Failed(error) => {
                Poll::Ready(Err(MidiHardwareError::BackendFailed(Arc::clone(error))))
            }
            Wait::Gate { gate, from } => gate.poll(*from, cx.waker()).map(|outcome| {
                outcome
                    .map(|()| Started(()))
                    .map_err(MidiHardwareError::BackendFailed)
            }),
        }
    }
}

#[cfg_attr(
    not(any(test, midi_backend = "coremidi")),
    allow(
        dead_code,
        reason = "only the CoreMIDI backend has a start to wait for"
    )
)]
/// Where a backend's start stands. A start passes through it on the backend's
/// thread; every method is a transition, taking the `now` it needs.
#[derive(Clone, Debug)]
pub(crate) enum HubState {
    /// The thread is making the client. `generation` counts the starts.
    Starting {
        generation: u64,
        /// When a window of retries ended in failure, carried into this start.
        gave_up: Option<Instant>,
    },
    /// The client is made. Terminal.
    Ready,
    /// The start ended in `error`.
    Failed {
        generation: u64,
        error: Arc<MidiHardwareError>,
        /// When a window of retries ended in failure, if this one did.
        gave_up: Option<Instant>,
    },
}

#[cfg_attr(
    not(any(test, midi_backend = "coremidi")),
    allow(
        dead_code,
        reason = "only the CoreMIDI backend has a start to wait for"
    )
)]
/// What [`HubState::begin`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Begun {
    /// The start a future made now waits on.
    pub(crate) generation: u64,
    /// Whether this call started a new one, so the thread must be told.
    pub(crate) restarted: bool,
}

#[cfg_attr(
    not(any(test, midi_backend = "coremidi")),
    allow(
        dead_code,
        reason = "only the CoreMIDI backend has a start to wait for"
    )
)]
impl HubState {
    /// The first start, under way.
    pub(crate) fn new() -> Self {
        Self::Starting {
            generation: 0,
            gave_up: None,
        }
    }

    /// A future is made: it waits on the start under way, or, where the last start
    /// failed, a new one that carries the window forward.
    pub(crate) fn begin(&mut self) -> Begun {
        match self {
            Self::Starting { generation, .. } => Begun {
                generation: *generation,
                restarted: false,
            },
            Self::Ready => Begun {
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
                Begun {
                    generation: next,
                    restarted: true,
                }
            }
        }
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

    /// The start under way failed. `gave_up` is when its window ended, or else the
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

    /// The backend's thread ended before it answered: the start under way failed
    /// with [`MidiHardwareError::ThreadEnded`]. A thread that ends after the
    /// client is made, or after it failed, changes nothing.
    pub(crate) fn ended(&mut self) -> bool {
        let gave_up = match self {
            Self::Starting { gave_up, .. } => *gave_up,
            Self::Ready | Self::Failed { .. } => return false,
        };
        self.failed(
            Arc::new(MidiHardwareError::ThreadEnded {
                thread: MidiThread::DeviceWatch,
            }),
            gave_up,
        )
    }

    /// What a future that began at generation `from` has: `None` while its start
    /// is under way, the error of a start no earlier than its own.
    pub(crate) fn outcome(&self, from: u64) -> Option<Result<(), Arc<MidiHardwareError>>> {
        match self {
            Self::Ready => Some(Ok(())),
            Self::Failed {
                generation, error, ..
            } if *generation >= from => Some(Err(Arc::clone(error))),
            Self::Starting { .. } | Self::Failed { .. } => None,
        }
    }

    /// The start the backend's thread should make, when it has not made it:
    /// `done` is the generation it last ran. Its generation, and the window the
    /// start carries.
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

#[cfg_attr(
    not(any(test, midi_backend = "coremidi")),
    allow(
        dead_code,
        reason = "only the CoreMIDI backend has a start to wait for"
    )
)]
/// A [`HubState`] and the wakers of the futures waiting on it. Nothing waits while
/// holding the lock, and a waker is woken after it is released.
pub(crate) struct StartGate {
    cell: Mutex<(HubState, Vec<Waker>)>,
}

#[cfg_attr(
    not(any(test, midi_backend = "coremidi")),
    allow(
        dead_code,
        reason = "only the CoreMIDI backend has a start to wait for"
    )
)]
impl StartGate {
    /// A gate whose first start is under way.
    pub(crate) fn new() -> Self {
        Self {
            cell: Mutex::new((HubState::new(), Vec::new())),
        }
    }

    /// Runs `change` on the state; when it returns true every waiting future is woken.
    fn publish<R>(&self, change: impl FnOnce(&mut HubState) -> (bool, R)) -> R {
        let (woken, result) = {
            let mut cell = self.cell.lock_recover();
            let (changed, result) = change(&mut cell.0);
            (changed.then(|| std::mem::take(&mut cell.1)), result)
        };
        woken.into_iter().flatten().for_each(Waker::wake);
        result
    }

    /// A future is made. See [`HubState::begin`].
    pub(crate) fn begin(&self) -> Begun {
        self.publish(|state| (false, state.begin()))
    }

    /// The client is made: every waiting future is woken.
    pub(crate) fn publish_ready(&self) {
        self.publish(|state| (state.ready(), ()));
    }

    /// The start failed: every waiting future is woken.
    pub(crate) fn publish_failed(&self, error: Arc<MidiHardwareError>, gave_up: Option<Instant>) {
        self.publish(|state| (state.failed(error, gave_up), ()));
    }

    /// The backend's thread ended without answering: what it was making failed,
    /// and every waiting future is woken.
    pub(crate) fn ended(&self) {
        self.publish(|state| (state.ended(), ()));
    }

    /// See [`HubState::wanted`].
    pub(crate) fn wanted(&self, done: Option<u64>) -> Option<(u64, Option<Instant>)> {
        self.cell.lock_recover().0.wanted(done)
    }

    /// The error the last start ended in, if it has.
    pub(crate) fn failure(&self) -> Option<Arc<MidiHardwareError>> {
        match &self.cell.lock_recover().0 {
            HubState::Failed { error, .. } => Some(Arc::clone(error)),
            _ => None,
        }
    }

    /// One poll of a future that began at `from`: the outcome, or `waker` stored
    /// to be woken when the state changes.
    fn poll(&self, from: u64, waker: &Waker) -> Poll<Result<(), Arc<MidiHardwareError>>> {
        let mut cell = self.cell.lock_recover();
        if let Some(outcome) = cell.0.outcome(from) {
            return Poll::Ready(outcome);
        }
        if !cell.1.iter().any(|stored| stored.will_wake(waker)) {
            cell.1.push(waker.clone());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests;
