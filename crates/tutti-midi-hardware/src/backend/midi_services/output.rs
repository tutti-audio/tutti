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
    /// and so cannot be a timestamp, goes now; a time too far ahead for the service to
    /// schedule is refused, so the packet is not taken and is offered again.
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
    /// in [`losses`](DeviceOut::losses) ([`by_message`]).
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let sent = by_message(events, "Windows MIDI Services", |message| {
            for (sent, event) in message.packets().iter().enumerate() {
                if let Err(error) = self.send(event.send, &event.ump) {
                    if error == Error::Closed {
                        self.alive.store(false, Ordering::Release);
                    }
                    return Err(Refused { error, sent });
                }
            }
            Ok(())
        });
        self.lost.lock().add(sent.losses);
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
