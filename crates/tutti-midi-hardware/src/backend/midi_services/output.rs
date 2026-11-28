//! Sending UMP to a Windows MIDI Services endpoint, each packet at its time.
//!
//! The output sends inside `queue_timed`: the service takes a message with a time ahead of
//! its clock and holds it until then, so there is no thread of ours and nothing waits here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::os;
use crate::backend::by_message::{Refused, by_message};
use crate::endpoints::DeviceOut;
use crate::error::Result;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt, Taken};
use tutti_rt::{Counter, CounterWriter, Mutex, counter};
use tutti_ump::Ump;
use tutti_units::Losses;
use tutti_winmidi2_ffi::{Connection, Error, Session};

/// An open output, as a [`MidiOut`].
pub(super) struct MidiServicesOutput {
    // Dropped before the session it was made on.
    connection: Connection,
    _session: Arc<Session>,
    /// Cleared when the service answers that the connection is closed.
    alive: AtomicBool,
    /// Counts the packets of a message the service took in part, and so lost.
    lost: Mutex<CounterWriter<Losses>>,
    losses: Counter<Losses>,
}

impl MidiServicesOutput {
    /// Sends `ump` at `send`: the service's timestamp of the message is that time, and the
    /// service holds it until then. A time the clock has passed, or that is before its origin
    /// and so cannot be a timestamp, goes now.
    ///
    /// # Errors
    ///
    /// The service's refusal. [`Error::TooFarAhead`] is one `queue_timed` counts as a loss
    /// instead of offering again: the service refuses a time more than 20 s ahead
    /// (`MIDI_MAX_ALLOWED_SCHEDULER_SECONDS_INTO_FUTURE`), which is past the
    /// [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD) the session's output plan schedules ahead, so the time is a clock
    /// fault, and a later offer would be refused again.
    fn send(&self, send: SendAt, ump: &Ump) -> core::result::Result<(), Error> {
        let at = match send {
            SendAt::Now => None,
            SendAt::At(at) => Some(at),
        };
        match self.connection.send(at, ump) {
            Err(Error::Timestamp) => self.connection.send(None, ump),
            sent => sent,
        }
    }
}

impl MidiOut for MidiServicesOutput {
    /// Each packet goes to the service at its time. The service takes a packet whole or
    /// not at all, so a message of several packets is cut only between two of them, by the
    /// service's buffer filling up: its head is out, it is taken, and its tail is counted
    /// in [`losses`](DeviceOut::losses) ([`by_message`]). A packet whose time is too far
    /// ahead is refused for good: its message is taken and its packets that did not go are
    /// counted as lost.
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let mut too_far = Losses::NONE;
        let sent = by_message(events, "Windows MIDI Services", |message| {
            for (sent, event) in message.packets().iter().enumerate() {
                match self.send(event.send, &event.ump) {
                    Ok(()) => {}
                    Err(Error::TooFarAhead) => {
                        too_far += Losses::lost((message.len() - sent) as u64);
                        return Ok(());
                    }
                    Err(error) => {
                        if error == Error::Closed {
                            self.alive.store(false, Ordering::Release);
                        }
                        return Err(Refused { error, sent });
                    }
                }
            }
            Ok(())
        });
        let mut lost = sent.losses;
        lost += too_far;
        self.lost.lock().add(lost);
        sent.taken
    }
}

impl DeviceOut for MidiServicesOutput {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn losses(&self) -> Losses {
        self.losses.get()
    }
}

/// Connects to the endpoint with device id `endpoint`.
///
/// # Errors
///
/// The platform's error from the connection.
pub(super) fn open(session: Arc<Session>, endpoint: &str) -> Result<MidiServicesOutput> {
    // A send has no use for what the endpoint sends.
    let connection = session.connect_send_only(endpoint).map_err(os)?;
    let (lost, losses) = counter();
    Ok(MidiServicesOutput {
        connection,
        _session: session,
        alive: AtomicBool::new(true),
        lost: Mutex::new(lost),
        losses,
    })
}
