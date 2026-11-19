//! The tick where a thread can start: one thread running
//! [`Shared::tick_once`](super::Shared::tick_once) until the set is shut.

use std::io;

use tutti_platform::{ThreadHandle, spawn_named};

use super::{Arc, Ordering, Parker, Shared, State};

/// The thread, held for its drop, which stops and joins it.
pub(super) struct Tick {
    _thread: Option<ThreadHandle<()>>,
}

impl Tick {
    pub(super) fn start(shared: &Arc<Shared>, mut parker: Parker) -> io::Result<Self> {
        let shared = Arc::clone(shared);
        let thread = spawn_named(
            "tutti-bells",
            None,
            move |_| {
                while shared.tick_once(&mut parker) {}
            },
        )?;
        Ok(Self {
            _thread: Some(thread),
        })
    }

    /// The thread ticks: nothing to do here.
    pub(super) fn run(&self, _shared: &Shared) {}

    #[cfg(any(test, loom))]
    pub(super) fn none() -> Self {
        Self { _thread: None }
    }

    #[cfg(all(test, not(loom)))]
    pub(super) fn is_running(&self) -> bool {
        self._thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl State {
    /// Whether the tick has a waker to wake: rung, and someone waiting.
    fn wakeable(&self) -> bool {
        self.rung.load(Ordering::Acquire) && self.waiting.load(Ordering::Acquire)
    }
}

impl Shared {
    /// Some bell is rung and has a waiter: the tick thread's pending check.
    pub(super) fn pending(&self) -> bool {
        self.bells().iter().any(|state| state.wakeable())
    }

    /// Park until a bell has a waiter to wake or the set is shut, then run
    /// one pass; `false` once shut. The tick thread loops on it.
    pub(super) fn tick_once(&self, parker: &mut Parker) -> bool {
        parker.park(|| self.shut.load(Ordering::Acquire) || self.pending());
        if self.shut.load(Ordering::Acquire) {
            return false;
        }
        self.pass();
        true
    }
}
