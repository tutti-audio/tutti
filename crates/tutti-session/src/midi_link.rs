//! The session's MIDI-CI link (feature `midi-hardware`): the OS MIDI layer shared
//! between [`update`](crate::Session::update) and one task on a thread of its own.
//!
//! The task runs on a dedicated thread of the session's executor
//! ([`Executor::spawn_dedicated`]), not on the executor's shared queue: that
//! queue can sit behind the session's other tasks (a long load or an analysis),
//! and a peer's reply or Discovery must not wait for them. It steps
//! [`MidiSession::poll`] whenever a port's reader wrote what it reads, a handle
//! queued a request, a control call changed the session, or a wait of the
//! devices ran out. It sends what the devices answer or ask again and resolves
//! a peer's answer at once, which wakes the future that asked. The rest of what
//! a poll found (Stream settles, peers and Profiles that changed) is the
//! session's own view of the devices: it is queued, and `update`'s
//! `Phase::Poll` applies it to the graph, the ports' info and the events.
//!
//! # Locks
//!
//! The layer's lock comes first, then a port's `pending` table, then its inbox
//! and record. The lock is never held across an `.await`: the task takes it for
//! one step and drops it before it waits. `update` holds it for its
//! `Phase::Poll`, and a control call holds it for that call only. The layer
//! rings the bell itself when a call leaves it work or moves its deadline.

use std::pin::Pin;
use std::sync::{Arc, MutexGuard, Weak};
use std::task::{Context, Poll};

use tutti_midi_hardware::{MidiHardwareError, MidiSession, MidiThread, Polled};
use tutti_platform::{Executor, Instant, Task, Timer};
use tutti_rt::{Bell, Listener, Mutex};

use crate::midi_port::InputShared;

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

impl MidiLink {
    /// The link over `io`, and the task that drives it on a thread of its own
    /// of `executor`, woken by `bell`'s `listener`. The task ends when the
    /// returned link drops.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Spawn`] when the OS refuses the thread.
    pub(crate) fn start(
        io: MidiSession,
        executor: &Executor,
        bell: Bell,
        listener: Listener,
    ) -> Result<(Arc<Self>, Task<()>), MidiHardwareError> {
        let link = Arc::new(Self {
            io: Mutex::new(io),
            ports: Mutex::new(Vec::new()),
            queued: Mutex::new(Vec::new()),
            bell,
        });
        let task = executor
            .spawn_dedicated(
                MidiThread::MidiCi.name(),
                None,
                drive(Arc::downgrade(&link), listener, executor.timer()),
            )
            .map_err(|source| MidiHardwareError::Spawn {
                thread: MidiThread::MidiCi,
                source,
            })?;
        Ok((link, task))
    }

    /// The layer, locked for a call of the control side.
    pub(crate) fn lock(&self) -> MutexGuard<'_, MidiSession> {
        self.io.lock()
    }

    /// The bell that wakes the task: a handle's request rings it.
    pub(crate) fn bell(&self) -> &Bell {
        &self.bell
    }

    /// Adds an input port to the ports the link routes to.
    pub(crate) fn register(&self, port: &Arc<InputShared>) {
        let mut ports = self.ports.lock();
        ports.retain(|port| port.strong_count() > 0);
        ports.push(Arc::downgrade(port));
    }

    /// What the task polled since `update` last took it.
    pub(crate) fn take_queued(&self) -> Vec<Polled> {
        std::mem::take(&mut *self.queued.lock())
    }

    /// One step at `now`, for the task and for `update`: sends what the ports'
    /// handles asked, polls the layer, resolves each answer a peer gave through
    /// its port, and queues the rest for `update`.
    pub(crate) fn step(&self, io: &mut MidiSession, now: Instant) {
        let ports: Vec<Arc<InputShared>> = {
            let mut ports = self.ports.lock();
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
        self.queued.lock().extend(found);
    }
}

/// The task: a step at the real time, then a wait for the bell or the next deadline, until the
/// link or the executor is gone.
async fn drive(link: Weak<MidiLink>, mut listener: Listener, timer: Timer) {
    loop {
        let deadline = {
            let Some(link) = link.upgrade() else { return };
            let mut io = link.lock();
            link.step(&mut io, Instant::now());
            io.deadline()
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
