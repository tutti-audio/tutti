//! Sending UMP to a sequencer port.
//!
//! The output never blocks: its client is non-blocking, so a destination with no
//! room refuses the send and the batch stops there, the rest untaken. A message
//! of several packets is sent packet by packet, and only the destination
//! filling up between two of its packets cuts it; that is the one way a message
//! goes in part, and the packets it lost are counted ([`DeviceOut::losses`]).

use tutti_rt::{Counter, CounterWriter, Mutex, counter};
use tutti_units::Losses;

use crate::backend::by_message::{Refused, by_message};
use crate::endpoints::DeviceOut;
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::stamp::Paired;
use tutti_alsa_ffi::{Client, PortCaps, PortId, PortKind, QueueClock, UmpVersion};
use tutti_midi_device::EndpointId;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt, Taken};
use tutti_ump::Ump;

/// An open ALSA output, as a [`MidiOut`].
pub struct AlsaOutput {
    /// The client and its port, which are only valid together.
    ///
    /// Behind a `Mutex` because `MidiOut::queue` takes `&self` and a `Client`
    /// is used from one thread at a time. Sending happens on the control
    /// thread's output drain, never the audio thread, so a lock costs nothing
    /// that matters here.
    inner: Mutex<Sender>,
    /// The packets of messages the destination took in part, read without the
    /// send lock.
    losses: Counter<Losses>,
}

struct Sender {
    seq: Client,
    port: PortId,
    /// The client's own queue, started when the output opened, which holds an
    /// event until its time.
    queue: QueueClock,
    /// Counts the packets of a message the destination took in part, and so
    /// lost.
    lost: CounterWriter<Losses>,
}

impl Sender {
    /// Sends one UMP message now or on the queue at `at`, pairing the queue's
    /// clock with `Instant` first if none is `paired` yet: a batch pairs once,
    /// at its first scheduled packet ([`Paired`]).
    fn send(&mut self, ump: &Ump, at: SendAt, paired: &mut Option<Paired>) -> Result<()> {
        let SendAt::At(instant) = at else {
            self.seq.send_to_subscribers(self.port, ump)?;
            return Ok(());
        };
        let pairing = match *paired {
            Some(pairing) => pairing,
            None => *paired.insert(super::client::pair(&mut self.queue, &self.seq)?),
        };
        match pairing.departure(instant) {
            Some(time) => self
                .seq
                .send_to_subscribers_at(self.port, ump, &self.queue, time)?,
            // Beyond what a queue's clock holds: not schedulable, so now.
            None => self.seq.send_to_subscribers(self.port, ump)?,
        }
        Ok(())
    }
}

impl AlsaOutput {
    /// A poisoned lock guards nothing broken: `Sender` is a client handle and
    /// two ids, never left half-written, and a send that panicked inside
    /// alsa-lib leaves the client usable. A panic here would take down the
    /// output drain over a fault that is already past.
    fn sender(&self) -> std::sync::MutexGuard<'_, Sender> {
        self.inner.lock()
    }
}

#[cfg(all(test, feature = "sequencer-tests"))]
impl AlsaOutput {
    /// The free cells of the client's output pool.
    pub(super) fn output_free(&self) -> usize {
        self.sender()
            .seq
            .output_free()
            .expect("the pool to be readable")
    }
}

impl MidiOut for AlsaOutput {
    /// Each packet is held by the sequencer's queue until its time (an `At`),
    /// or sent directly (`Now`).
    ///
    /// A message goes to the sequencer a packet at a time. One that needs the
    /// output pool is refused whole when the pool lacks room for all of it. The
    /// destination's own fifo cannot be read, so a message it fills up inside
    /// is cut. A direct (`Now`) message is cut at the send: its head is out, it
    /// is taken, and its tail is counted in [`losses`](DeviceOut::losses)
    /// ([`by_message`]). A scheduled (`At`) message is cut later, in the
    /// sequencer's queue, after it was taken whole: its tail is lost uncounted.
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let mut sender = self.sender();
        let mut paired = None;
        let sent = by_message(events, "ALSA", |message| {
            // A message of several packets that need cells of this client's
            // output pool goes only if the pool has room for all of them: it
            // can be read, where the destination's fifo cannot.
            let needed = message
                .packets()
                .iter()
                .filter(|event| matches!(event.send, SendAt::At(_)))
                .count();
            if message.len() > 1 && needed > 0 {
                let free = sender.seq.output_free().map_err(|e| Refused {
                    error: e.into(),
                    sent: 0,
                })?;
                if free < needed {
                    return Err(Refused {
                        error: MidiHardwareError::OutputPoolFull { free, needed },
                        sent: 0,
                    });
                }
            }
            for (sent, event) in message.packets().iter().enumerate() {
                if let Err(error) = sender.send(&event.ump, event.send, &mut paired) {
                    return Err(Refused { error, sent });
                }
            }
            Ok(())
        });
        sender.lost.add(Losses::lost(sent.lost));
        sent.taken
    }
}

impl DeviceOut for AlsaOutput {
    fn losses(&self) -> Losses {
        self.losses.get()
    }
}

/// Opens `id` for output.
///
/// # Errors
///
/// [`MidiHardwareError::Os`] if the sequencer cannot be opened, the
/// port created, or the subscription to `id` made.
pub fn open(id: EndpointId) -> Result<Box<dyn DeviceOut>> {
    Ok(Box::new(connect(id)?))
}

/// The name of the engine's output client and of its one port.
pub(super) const CLIENT: &str = "tutti-out";

/// [`open`], as the concrete type.
pub(super) fn connect(id: EndpointId) -> Result<AlsaOutput> {
    let dest = super::address_of(id).ok_or(NotFound::Endpoint(id))?;

    // Non-blocking: a destination with no room refuses the send, which is
    // counted, rather than holding the caller until it has some.
    let seq = super::client::open(CLIENT, UmpVersion::Midi2)?;
    let port = seq.open_port(
        CLIENT,
        // Not subscribable by others, so never listed as a device (see
        // `input::open`); this client subscribes its own port to the
        // destination.
        PortCaps::READ,
        PortKind::MIDI_GENERIC | PortKind::APPLICATION,
    )?;

    seq.connect_to(port, dest)?;

    // The queue that holds scheduled events. A queue the client never created
    // is refused a scheduled send, so it is the client's own and started.
    let queue = seq.start_queue()?;

    // The destination is not stored: once `connect_to` has subscribed us, every
    // send addresses `SUBSCRIBERS` rather than the endpoint by name. Keeping the
    // address around would invite exactly the explicit-`dest` send that ALSA
    // rejects.
    let (lost, losses) = counter();
    Ok(AlsaOutput {
        inner: Mutex::new(Sender {
            seq,
            port,
            queue,
            lost,
        }),
        losses,
    })
}
