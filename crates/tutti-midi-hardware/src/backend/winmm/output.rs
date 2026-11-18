//! Sending MIDI 1.0 to a winmm output, each event at its time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::legacy_port::bytes::{ByteOut, Wire};
use super::super::legacy_port::sender::{Mailbox, run};
use crate::endpoints::DeviceOut;
use crate::error::{MidiHardwareError, Result, Thread};
use tutti_graph::Losses;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt, Taken};
use tutti_rt::{CounterWriter, ThreadHandle, spawn_named};
use tutti_ump::Ump;
use tutti_winmm_ffi::{MidiOut as Port, OutputPort, TimerWaiter, TimerWaker, waitable_timer};

/// How many events wait at most; a queue past it takes a prefix and says so.
const MAX_PENDING: usize = 8192;

/// How long dropping an output waits for its thread. The thread looks at the
/// stop before each event, so it ends within the send it is in; a SysEx long
/// enough to take longer is left to finish on its own, and the thread is
/// detached with a warning.
const JOIN_BOUND: Duration = Duration::from_secs(2);

struct Shared {
    mailbox: Mailbox<Ump>,
    /// Ends the thread's wait when an earlier event arrives or the output stops.
    wake: TimerWaker,
}

/// An open winmm output, as a [`MidiOut`]: a thread holds each event until its
/// time and sends in order. Dropping it stops the thread and closes the port;
/// events not yet due are dropped.
pub struct WinMmOutput {
    shared: Arc<Shared>,
    thread: Option<ThreadHandle<()>>,
}

impl MidiOut for WinMmOutput {
    /// Returns what was taken: whole messages, short only when [`MAX_PENDING`]
    /// events already wait or the batch's next message would pass it, and none
    /// once the output is dead. A send that then fails is counted in
    /// [`losses`](DeviceOut::losses).
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let now = Instant::now();
        let mut state = self.shared.mailbox.state();
        let taken = Taken::within(events, state.room(MAX_PENDING));
        state.take(
            MAX_PENDING,
            events[..taken.packets()].iter().map(|event| {
                let due = match event.send {
                    SendAt::Now => now,
                    SendAt::At(at) => at,
                };
                (due, event.ump)
            }),
        );
        drop(state);
        if !taken.is_none()
            && let Err(e) = self.shared.wake.wake()
        {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        taken
    }
}

impl DeviceOut for WinMmOutput {
    fn is_alive(&self) -> bool {
        self.shared.mailbox.state().alive()
    }

    fn losses(&self) -> Losses {
        self.shared.mailbox.losses()
    }

    fn pending(&self) -> usize {
        self.shared.mailbox.state().pending()
    }
}

impl Drop for WinMmOutput {
    fn drop(&mut self) {
        self.shared.mailbox.state().stop();
        if let Err(e) = self.shared.wake.wake() {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        if let Some(thread) = self.thread.take() {
            match thread.stop_within(JOIN_BOUND) {
                Ok(Ok(())) => {}
                // A panic in the sender is caught by its loop, which kills the
                // mailbox; one that escapes is reported here.
                Ok(Err(panicked)) => tracing::error!("winmm MIDI output: {panicked}"),
                Err(stuck) => {
                    tracing::warn!(
                        "winmm MIDI output did not stop within {JOIN_BOUND:?}; left running"
                    );
                    stuck.detach();
                }
            }
        }
    }
}

/// The sender thread: the loop of [`run`], with winmm for the send and the
/// timer for the wait.
fn thread(mut port: Port, mut wait: TimerWaiter, losses: CounterWriter<Losses>, shared: &Shared) {
    run(&shared.mailbox, losses, || {
        let mut bytes = ByteOut::new();
        (
            move |ump| {
                bytes.encode(ump, |wire| match wire {
                    Wire::Short(message) => port.short(message),
                    Wire::Long(message) => port.long(message),
                })
            },
            move |time| match time {
                Some(after) => wait.wait_for(after).map(|_| ()),
                None => wait.wait(),
            },
        )
    });
}

/// Opens `port`.
///
/// # Errors
///
/// [`MidiHardwareError::Os`] from the port's or the timer's creation, and
/// [`MidiHardwareError::Spawn`] if the thread cannot be spawned.
pub(super) fn open(port: &OutputPort) -> Result<WinMmOutput> {
    let port = Port::open(port)?;
    let (wake, wait) = waitable_timer()?;
    let (mailbox, losses) = Mailbox::new();
    let shared = Arc::new(Shared { mailbox, wake });
    let thread = spawn_named(Thread::MidiOut.name(), None, {
        let shared = Arc::clone(&shared);
        move |_| {
            thread(port, wait, losses, &shared);
        }
    })
    .map_err(|source| MidiHardwareError::Spawn {
        thread: Thread::MidiOut,
        source,
    })?;
    Ok(WinMmOutput {
        shared,
        thread: Some(thread),
    })
}
