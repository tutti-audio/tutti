//! The session's MIDI-CI link (feature `midi-hardware`): the OS MIDI layer shared
//! between [`update`](crate::Session::update) and one task on the session's
//! executor.
//!
//! The task steps [`MidiSession::poll`] whenever a port's reader wrote what it
//! reads, a handle queued a request, a control call changed the session, or a
//! wait of the devices ran out. It sends what the devices answer or ask again
//! and resolves a peer's answer at once, which wakes the future that asked. The
//! rest of what a poll found (Stream settles, peers and Profiles that changed)
//! is the session's own view of the devices: it is queued, and `update`'s
//! `Phase::Poll` applies it to the graph, the ports' info and the events.
//!
//! # Locks
//!
//! The layer's lock comes first, then a port's `pending` table, then its inbox
//! and record. The lock is never held across an `.await`: the task takes it for
//! the steps of one pass and drops it before it waits. `update` holds it for
//! its `Phase::Poll`, and a control call holds the [`Locked`] it made for that
//! call only.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll};

use tutti_midi_hardware::{MidiSession, Polled};
use tutti_platform::{Executor, Instant, Task, Timer};
use tutti_rt::{Bell, Listener, LockExt};

use crate::midi_port::InputShared;

/// How many polls in a row a pass makes while the last one left work for the
/// next ([`MidiSession::needs_poll`]): a guard against a session that never
/// settles, not a figure a settled one reaches.
const MAX_CHAINED_POLLS: usize = 16;

/// The OS MIDI layer, and what `update` and the MIDI-CI task share of it.
pub(crate) struct MidiLink {
    io: Mutex<MidiSession>,
    /// The input ports, to route a peer's answer to its port and to drain what
    /// its handles queued.
    ports: Mutex<Vec<Weak<InputShared>>>,
    /// What the task polled that `update` applies.
    queued: Mutex<Vec<Polled>>,
    /// Rung to wake the task: by `update` and the control calls when they
    /// change the layer, and by a handle that queued a request.
    bell: Bell,
}

impl std::fmt::Debug for MidiLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiLink").finish_non_exhaustive()
    }
}

/// The layer, locked for one call: it derefs to the [`MidiSession`]. As the
/// call ends, the bell rings when the layer has work for the next poll or its
/// next deadline moved, so the task polls what the call changed.
pub(crate) struct Locked<'a> {
    guard: MutexGuard<'a, MidiSession>,
    link: &'a MidiLink,
    /// The layer's deadline when it was locked.
    deadline: Option<Instant>,
}

impl<'a> Locked<'a> {
    /// The link the layer belongs to.
    pub(crate) const fn link(&self) -> &'a MidiLink {
        self.link
    }
}

impl std::fmt::Debug for Locked<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locked").finish_non_exhaustive()
    }
}

impl std::ops::Deref for Locked<'_> {
    type Target = MidiSession;

    fn deref(&self) -> &MidiSession {
        &self.guard
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut MidiSession {
        &mut self.guard
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        if self.guard.needs_poll() || self.guard.deadline() != self.deadline {
            self.link.bell.ring();
        }
    }
}

impl MidiLink {
    /// The link over `io`, and the task that drives it on `executor`, woken by
    /// `bell`'s `listener`. The task ends when the returned link drops.
    pub(crate) fn start(
        io: MidiSession,
        executor: &Executor,
        bell: Bell,
        listener: Listener,
    ) -> (Arc<Self>, Task<()>) {
        let link = Arc::new(Self {
            io: Mutex::new(io),
            ports: Mutex::new(Vec::new()),
            queued: Mutex::new(Vec::new()),
            bell,
        });
        let task = executor.spawn(drive(Arc::downgrade(&link), listener, executor.timer()));
        (link, task)
    }

    /// The layer, locked for a call of the control side.
    pub(crate) fn lock(&self) -> Locked<'_> {
        let guard = self.io.lock_recover();
        Locked {
            deadline: guard.deadline(),
            guard,
            link: self,
        }
    }

    /// The bell that wakes the task: a handle's request rings it.
    pub(crate) fn bell(&self) -> &Bell {
        &self.bell
    }

    /// Adds an input port to the ports the link routes to.
    pub(crate) fn register(&self, port: &Arc<InputShared>) {
        let mut ports = self.ports.lock_recover();
        ports.retain(|port| port.strong_count() > 0);
        ports.push(Arc::downgrade(port));
    }

    /// What the task polled since `update` last took it.
    pub(crate) fn take_queued(&self) -> Vec<Polled> {
        std::mem::take(&mut *self.queued.lock_recover())
    }

    /// One step at `now`, for the task and for `update`: sends what the ports'
    /// handles asked, polls the layer, resolves each answer a peer gave through
    /// its port, and queues the rest for `update`.
    pub(crate) fn step(&self, io: &mut MidiSession, now: Instant) {
        let ports: Vec<Arc<InputShared>> = {
            let mut ports = self.ports.lock_recover();
            ports.retain(|port| port.strong_count() > 0);
            ports.iter().filter_map(Weak::upgrade).collect()
        };
        for port in &ports {
            port.send_requests(io);
        }
        let mut found = Vec::new();
        for polled in io.poll(now) {
            if let Polled::Answer(device, peer, answer) = &polled {
                if let Some(port) = ports.iter().find(|port| port.is_open_on(device)) {
                    port.answer(io, *peer, answer);
                }
            } else {
                found.push(polled);
            }
        }
        self.queued.lock_recover().extend(found);
    }

    /// One pass of the task: steps at the real time while the layer has work
    /// that no timer and no reply will bring. When the next wait runs out.
    fn pass(&self) -> Option<Instant> {
        let mut io = self.io.lock_recover();
        for _ in 0..MAX_CHAINED_POLLS {
            self.step(&mut io, Instant::now());
            if !io.needs_poll() {
                break;
            }
        }
        io.deadline()
    }
}

/// The task: a pass, then a wait for the bell or the next deadline, until the
/// link or the executor is gone.
async fn drive(link: Weak<MidiLink>, mut listener: Listener, timer: Timer) {
    loop {
        let deadline = {
            let Some(link) = link.upgrade() else { return };
            link.pass()
        };
        let mut rung = std::pin::pin!(listener.rung());
        let mut sleep = deadline.map(|at| timer.sleep_until(at));
        let woken = std::future::poll_fn(|cx: &mut Context<'_>| {
            if rung.as_mut().poll(cx).is_ready() {
                return Poll::Ready(true);
            }
            match sleep.as_mut().map(|sleep| Pin::new(sleep).poll(cx)) {
                Some(Poll::Ready(Ok(()))) => Poll::Ready(true),
                // The executor shut down.
                Some(Poll::Ready(Err(_))) => Poll::Ready(false),
                Some(Poll::Pending) | None => Poll::Pending,
            }
        })
        .await;
        if !woken {
            return;
        }
    }
}
