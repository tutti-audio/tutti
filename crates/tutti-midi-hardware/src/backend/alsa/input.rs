//! Reading UMP from a sequencer port.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use super::poll::{ErrorRun, read_fault, wait_fault};
use crate::InputFault;
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::{MidiHardwareError, MidiThread, Result};
use crate::guard::Guard;
use crate::stamp::Paired;
use tutti_alsa_ffi::{
    Client, ClientHandle, Event, Poller, PortCaps, PortKind, QueueClock, Read, UmpVersion, Wait,
};
use tutti_midi_device::EndpointId;
use tutti_platform::{ThreadHandle, spawn_named};
use tutti_rt::FaultLatch;

/// How long dropping an input waits for its pump to end before it lets the
/// thread go. A pump ends within a poll wake of the stop; only a read that never
/// returns outlasts this, and a drop that waited for it would hang the caller
/// (the session's control thread) with it.
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// How long a pairing of the queue's clock with the OS's is trusted: the two
/// run apart by parts per million, and a pair this old has drifted by
/// microseconds at most.
const PAIRING_AGE: Duration = Duration::from_secs(1);

/// An open ALSA input. Dropping it stops the pump and closes the port.
pub struct AlsaInput {
    /// One end of a socket pair whose other end is in the pump's `poll` set:
    /// closing this one is the shutdown signal, and wakes the pump at once.
    stop: Option<UnixStream>,
    /// Raised by the pump when it ends on an error, never on a stop.
    died: Arc<FaultLatch<InputFault>>,
    thread: Option<ThreadHandle<()>>,
    /// The pumped client, for a send on it from another thread.
    client: ClientHandle,
}

impl AlsaInput {
    /// A handle that runs a call on the pumped client, alongside the pump's reads.
    pub(super) fn client(&self) -> ClientHandle {
        self.client.clone()
    }
}

impl InputConnection for AlsaInput {
    fn fault(&self) -> Option<InputFault> {
        self.died.cause()
    }
}

impl Drop for AlsaInput {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            // Joining matters: the thread holds the `Poller` that owns the
            // `Client`, and letting it outlive this would leave the port
            // subscribed to a device the caller believes it disconnected. But
            // not for ever: a pump stuck in alsa-lib is detached, and says so.
            match thread.stop_within(JOIN_BOUND) {
                Ok(Ok(())) => {}
                // The pump's guard catches its own panics and marks the input
                // dead; one that escapes the guard is reported here.
                Ok(Err(panicked)) => tracing::error!("ALSA MIDI pump: {panicked}"),
                Err(stuck) => {
                    tracing::warn!(
                        "ALSA MIDI pump did not stop within {JOIN_BOUND:?}; left running, still subscribed"
                    );
                    stuck.detach();
                }
            }
        }
    }
}

/// The UMP version an input on a source with this UMP Endpoint record (`None`
/// for a legacy client) is opened at.
///
/// A source that can only speak MIDI 1.0 (a legacy client, or an Endpoint whose
/// record offers no MIDI 2.0 Protocol) is opened as MIDI 1.0, so it arrives as
/// type 2 and the kernel converts nothing. An Endpoint that offers MIDI 2.0 is
/// opened as MIDI 2.0 even while it is in MIDI 1.0, because the session asks it
/// for the MIDI 2.0 Protocol and the client's version is fixed for the pump's
/// life: a MIDI 1.0 client would have the kernel translate what the device then
/// sends natively.
pub(super) fn version_for(endpoint: Option<&tutti_alsa_ffi::UmpEndpoint>) -> UmpVersion {
    match endpoint {
        Some(e) if e.offered.midi2 => UmpVersion::Midi2,
        _ => UmpVersion::Midi1,
    }
}

/// Opens `id` for input and starts pumping its events into `producer`.
///
/// The pump runs on its own thread and stops when the returned connection is
/// dropped, which is also when the port unsubscribes.
///
/// # Errors
///
/// [`MidiHardwareError::Os`] if the sequencer cannot be opened, the
/// port created, or the subscription made, and
/// [`MidiHardwareError::Spawn`] if the pump thread cannot be
/// spawned. A source client that exits while the open reads its Endpoint record
/// fails the open (`ENOENT`) rather than opening as a legacy port.
pub fn open(id: EndpointId, producer: InputProducerHandle) -> Result<Box<dyn InputConnection>> {
    let (seq, clock) = prepare(id)?;
    Ok(Box::new(start(seq, clock, producer)?))
}

/// A client subscribed to `id`, and its port's clock: what [`start`] pumps.
///
/// # Errors
///
/// The name of the engine's input client and of its one port.
pub(super) const CLIENT: &str = "tutti-in";

/// As [`open`], but for the pump thread.
pub(super) fn prepare(id: EndpointId) -> Result<(Client, QueueClock)> {
    let source = super::address_of(id).ok_or(crate::error::NotFound::Endpoint(id))?;

    // Non-blocking, so the pump reads until the queue is empty and then waits in
    // `poll`, which the stop descriptor can interrupt. One client per connection
    // — see `client::open`.
    let seq = super::client::open(CLIENT, UmpVersion::Midi2)?;
    // What the source can speak decides what this client is delivered: a MIDI
    // 1.0 source arrives as MIDI 1.0 UMP (type 2) and the input node's
    // `Midi2Translator` translates it, so the kernel never converts it.
    // The record is read on the open client, and the version is set before the
    // port exists and before the subscription, so no event is delivered at the
    // other one.
    seq.set_ump_version(version_for(seq.ump_endpoint(source.client())?.as_ref()))?;
    let port = seq.open_port(
        CLIENT,
        // Not subscribable by others: the engine's pump is not a device, and a
        // port nobody else can connect to is never listed as one. This client
        // makes its own subscription below, which needs no such capability.
        PortCaps::WRITE,
        PortKind::MIDI_GENERIC | PortKind::APPLICATION,
    )?;

    // The kernel stamps each event as it delivers it, so the time an event
    // is given is when it arrived, not when this pump got round to reading it.
    let clock = seq.stamp_port(port)?;

    seq.connect_from(port, source)?;
    Ok((seq, clock))
}

/// Starts pumping what the client's ports receive into `producer`: the pump
/// owns `seq` and ends, closing it, when the returned connection drops.
///
/// # Errors
///
/// [`MidiHardwareError::Os`] if the client's descriptors cannot be
/// polled, [`MidiHardwareError::Io`] if the stop pair cannot be made, and
/// [`MidiHardwareError::Spawn`] if the pump thread cannot start.
pub(super) fn start(
    seq: Client,
    clock: QueueClock,
    producer: InputProducerHandle,
) -> Result<AlsaInput> {
    let (stop_tx, stop_rx) = UnixStream::pair().map_err(|source| MidiHardwareError::Io {
        step: "make the MIDI pump's stop pair",
        source,
    })?;
    let seq = seq.into_poller(OwnedFd::from(stop_rx))?;
    let client = seq.handle();
    let died = Arc::new(FaultLatch::<InputFault>::new());
    let flag = Arc::clone(&died);
    let thread = spawn_named(MidiThread::MidiIn.name(), None, move |_| {
        // A panic in the pump ends it with the input held, so it is caught
        // and raises the latch, as an error does.
        Guard::new(Arc::clone(&flag)).run(|| pump(seq, clock, &flag, producer));
    })
    .map_err(|source| MidiHardwareError::Spawn {
        thread: MidiThread::MidiIn,
        source,
    })?;

    Ok(AlsaInput {
        stop: Some(stop_tx),
        died,
        thread: Some(thread),
        client,
    })
}

/// Read UMP events until the stop pair closes. Reads until the queue is empty,
/// then blocks in `poll`: the descriptors are level-triggered, so an event that
/// arrives between the last read and the `poll` wakes it at once.
///
/// An error ends the pump with `died` raised: the port goes silent
/// while the connection is still held, and [`InputConnection::is_alive`] is how
/// the holder finds out.
///
/// Takes `seq` **by value** so the client is dropped here, on this thread, when
/// the loop ends — the port unsubscribes exactly when the pump stops.
pub(super) fn pump(
    mut seq: Poller,
    clock: QueueClock,
    died: &FaultLatch<InputFault>,
    mut producer: InputProducerHandle,
) {
    let mut errors = ErrorRun::default();
    let mut clock = clock;
    let mut arrivals = Arrivals::default();
    loop {
        // The client is locked for the read only, not across the wait.
        let read = seq.client().read_event();
        let received = match read {
            Ok(Read::Event(event)) => event,
            // Empty queue is the common case, not a failure.
            Ok(Read::Empty) => {
                let waited = seq.wait();
                // The wait failed: the port is silent from here.
                if let Some(fault) = wait_fault(&waited) {
                    return died.raise(fault);
                }
                if matches!(waited, Ok(Wait::Stopped)) {
                    return;
                }
                continue;
            }
            Err(e) => {
                // An overrun or a failing read: try again at once, a few times
                // in a row at most (a level-triggered descriptor would spin).
                if errors.failed() {
                    return died.raise(read_fault(e.errno()));
                }
                continue;
            }
        };
        errors.read();
        // A subscription to the port, or a change of its Endpoint, is the
        // sequencer's notice and not music.
        let Event::Ump { ump, stamp } = received else {
            continue;
        };
        let now = Instant::now();
        let now = match stamp {
            Some(t) => arrivals.at(t, now, || {
                super::client::pair(&mut clock, &seq.client()).ok()
            }),
            // A port that is not stamping: the read is the best there is.
            None => now,
        };

        // A full inlet counts the drop itself.
        let _ = producer.push(ump, now);
    }
}

/// A queue time as an OS instant: the queue's clock and the OS's read together
/// (`pairing`), then each event's age is the queue's time then, subtracted from
/// the pairing's ([`Paired::arrival`]).
#[derive(Default)]
struct Arrivals {
    /// The pairing, and when it was read.
    pairing: Option<(Paired, Instant)>,
    /// The instant given to the event before: the port delivers its events in
    /// order, so none arrives before the one ahead of it.
    latest: Option<Instant>,
}

/// Whether the held `pairing` must be read again at `now` for an event stamped
/// `stamped`: there is none, the event came after it, or it is older than
/// [`PAIRING_AGE`].
fn pairing_is_stale(pairing: Option<(Paired, Instant)>, stamped: Duration, now: Instant) -> bool {
    pairing.is_none_or(|(paired, read)| {
        stamped > paired.clock() || now.saturating_duration_since(read) > PAIRING_AGE
    })
}

impl Arrivals {
    /// The OS instant, at `now`, of an event stamped `stamped` on the queue.
    /// Takes a fresh pairing from `pair` when the held one is older than the
    /// event (the event came after it) or than [`PAIRING_AGE`]; one syscall, not
    /// one per event. With no pairing to be had the event is at `now`.
    fn at(
        &mut self,
        stamped: Duration,
        now: Instant,
        pair: impl FnOnce() -> Option<Paired>,
    ) -> Instant {
        if pairing_is_stale(self.pairing, stamped, now) {
            self.pairing = pair().map(|paired| (paired, now));
        }
        let placed = self
            .pairing
            .map_or(now, |(paired, _)| paired.arrival(stamped));
        // Two pairings place events by readings of two clocks that were not
        // taken at one instant, so an event read after a refresh can land
        // before one placed by the pairing before it. The port's order is the
        // arrival order.
        in_order(&mut self.latest, placed)
    }
}

/// `placed`, or the instant of the event before it, `latest`, when that is
/// later: an event never arrives before the one ahead of it. `latest` becomes
/// the instant returned.
fn in_order(latest: &mut Option<Instant>, placed: Instant) -> Instant {
    let at = latest.map_or(placed, |latest| latest.max(placed));
    *latest = Some(at);
    at
}

#[cfg(test)]
mod tests;
