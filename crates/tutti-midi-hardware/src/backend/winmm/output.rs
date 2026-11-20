//! Sending MIDI 1.0 to a winmm output, each event at its time.

use std::sync::Arc;
use tutti_platform::Instant;

use super::super::legacy_port::bytes::{ByteOut, Wire};
use super::super::legacy_port::sender::{Mailbox, run};
use crate::endpoints::DeviceOut;
use crate::error::{MidiHardwareError, MidiThread, Result};
use tutti_graph::Losses;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt, Taken};
use tutti_platform::{Executor, Task};
use tutti_rt::CounterWriter;
use tutti_ump::Ump;
use tutti_winmm_ffi::{MidiOut as Port, OutputPort, TimerWaiter, TimerWaker, waitable_timer};

/// How many events wait at most; a queue past it takes a prefix and says so.
const MAX_PENDING: usize = 8192;

struct Shared {
    mailbox: Mailbox<Ump>,
    /// Ends the thread's wait when an earlier event arrives or the output stops.
    wake: TimerWaker,
}

/// An open winmm output, as a [`MidiOut`]: a future on a dedicated thread of the
/// executor holds each event until its time and sends in order. Dropping it
/// stops the sender and closes the port as the sender ends, without waiting for
/// it; events not yet due are dropped.
pub struct WinMmOutput {
    shared: Arc<Shared>,
    /// The sender's future. Dropping it after the wake cancels the future once it
    /// returns from the timer's wait.
    task: Option<Task<()>>,
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
        // The wake above ends the sender's wait, and the sender returns because
        // the mailbox is stopped, not because the task is cancelled: its body is
        // synchronous inside the async block and never awaits. The task drops
        // second.
        drop(self.task.take());
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
                    Wire::Short(ump) => port.send(&ump),
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
pub(super) fn open(executor: &Executor, port: &OutputPort) -> Result<WinMmOutput> {
    let port = Port::open(port)?;
    let (wake, wait) = waitable_timer()?;
    let (mailbox, losses) = Mailbox::new();
    let shared = Arc::new(Shared { mailbox, wake });
    let task = executor
        .spawn_dedicated(MidiThread::MidiOut.name(), None, {
            let shared = Arc::clone(&shared);
            async move {
                thread(port, wait, losses, &shared);
            }
        })
        .map_err(|source| MidiHardwareError::Spawn {
            thread: MidiThread::MidiOut,
            source,
        })?;
    Ok(WinMmOutput {
        shared,
        task: Some(task),
    })
}
