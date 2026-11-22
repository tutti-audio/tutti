//! The backend seam: enumerate and open this OS's MIDI endpoints.
//!
//! One implementation per platform — CoreMIDI, ALSA seq-UMP, winmm, and a stub where
//! none exists. [`MidiSession::open`](crate::MidiSession::open) picks one, and that is the
//! **only** place in the crate a `#[cfg(target_os)]` decides anything.
//!
//! # Why this trait, and why it stops here
//!
//! Applying the repo's own test — *a trait earns its name if you can state its
//! boundary in one sentence without "and"* — this one is "enumerate and open
//! this OS's MIDI endpoints". The "and" is doing no work: enumeration exists to
//! produce the [`EndpointId`] that opening consumes, and no caller wants one
//! without the other. Splitting them would put an id's minter and its only
//! consumer in different traits.
//!
//! What is deliberately **not** here: a `UmpIn`/`UmpOut` pair mirroring
//! `Source`/`Sink`. [`Ump`](tutti_ump::Ump) already
//! *is* a UMP message — `words()` hands back the `&[u32]` a driver wants —
//! so a `UmpOut::send_ump` would restate `MidiOut::queue`'s boundary exactly.
//! Two names for one behaviour is not a type.
//!
//! Hence [`open_output`](MidiEndpoints::open_output) returns a
//! **`Box<dyn MidiOut>`**: a backend yields a value speaking the engine's
//! existing vocabulary, so nothing downstream needs to know which OS produced
//! it. That is what lets the output router be one field and a `match` instead of
//! a `#[cfg]` ladder.
//!
//! # Input does not return a trait object
//!
//! Asymmetric on purpose. Output is a sink the caller pushes to, so an erased
//! `MidiOut` is exactly right. Input arrives the other way — a driver callback
//! pushes it on its own thread — and the engine already has a destination: the
//! [`InputProducerHandle`] of a [`HardwareMidiInputs`] port, which stamps each
//! arrival with its frame and pushes it into the device's event inlet. A backend therefore takes the handle and
//! wires its callback to it, rather than handing back a source nobody would
//! poll.
//!
//! [`HardwareMidiInputs`]: crate::HardwareMidiInputs

use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::error::Result;
use crate::start::{Start, Started};
use crate::{InputFault, InputProducerHandle};
use tutti_midi_device::EndpointId;
use tutti_midi_runtime::MidiOut;
use tutti_ump::Protocol;
use tutti_units::Losses;

/// An open input connection: a live subscription to one endpoint.
///
/// **No method moves events.** Events flow through the [`InputProducerHandle`]
/// the backend was handed, never through this value; what it carries is a
/// *lifetime*. Dropping it closes the port, which is why
/// [`MidiEndpoints::open_input`] returns it rather than `()` and why a caller
/// must hold it.
///
/// Do not give it an `endpoint()` accessor: the [`EndpointId`] is already the
/// key of the map these are stored in, so a connection is never asked for its
/// own address. The trait is named rather than erased to `Box<dyn Send>`
/// precisely because a bare `Send` says nothing about what dropping the value
/// does — this is where a backend author reads that contract, which is a
/// distinct thing to be even with no behaviour attached.
pub trait InputConnection: Send {
    /// Whether the backend is still delivering: `false` from the moment
    /// [`fault`](Self::fault) reads a cause, when the port is silent while the
    /// connection is still held.
    fn is_alive(&self) -> bool {
        self.fault().is_none()
    }

    /// Why the backend stopped delivering, `None` while it is. The first cause
    /// stays. A backend that can end on its own overrides this (and
    /// [`is_alive`](Self::is_alive) follows); the default is `None`, for a
    /// backend that cannot tell.
    fn fault(&self) -> Option<InputFault> {
        None
    }
}

/// A running watch over the OS's device list. Method-less like
/// [`InputConnection`]: it is a lifetime. Dropping it stops the backend's
/// work without waiting for it: a backend whose work is a future on an
/// `Executor` returns at once, and the future ends just after, on its own thread.
/// A report already under way reaches the [`ChangeSink`] as it ends, even after
/// the last [`Devices`](crate::Devices) has dropped: the sink holds the list's
/// state alive, takes the report into a list nobody reads and wakes no
/// subscriber that has gone.
pub trait Watch: Send + Sync {}

/// A watch that watches nothing: where no OS reports changes.
#[derive(Debug)]
pub(crate) struct NoWatch;

impl Watch for NoWatch {}

/// This OS's MIDI endpoints.
pub trait MidiEndpoints: Send + Sync {
    /// The wait for the backend to be ready to open endpoints and ports.
    ///
    /// A backend that has something to start (CoreMIDI's client, made on a thread
    /// of its own) returns a [`Start`] that resolves when it is made, or with the
    /// error it failed with. The default is for a backend with nothing to start: a
    /// start that is ready on its first poll. The [`Started`] it resolves to is what
    /// [`open_input`](Self::open_input) and [`open_output`](Self::open_output) take.
    fn start(&self) -> Start {
        Start::ready()
    }

    /// Starts reporting the device list to `sink`: every device present, once the
    /// backend can list them, then each change from a thread of the backend's own.
    /// A backend with nothing to start (ALSA, winmm, the stub) lists them before this
    /// returns; one that has a start to wait for (CoreMIDI) lists them when its
    /// [`start`](Self::start) has resolved, and the watch stays registered if a start
    /// fails and seeds when a later one succeeds. A reader of
    /// [`DeviceEvents::initial`](crate::DeviceEvents::initial) after the start has
    /// resolved sees the seeded list on every backend.
    ///
    /// Where the OS cannot be asked (no sequencer to open) the watch reports
    /// nothing, and [`inputs`](Self::inputs) fails there.
    ///
    /// # Errors
    ///
    /// The platform error from starting the thread.
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>>;

    /// Endpoints that can send MIDI to this engine.
    ///
    /// A fresh snapshot per call — device lists go stale on hot-plug, and a
    /// cached one is a second owner of state the OS already holds.
    ///
    /// # Errors
    ///
    /// The platform error when the OS cannot be asked (a sequencer that will
    /// not open): that is not an empty list.
    fn inputs(&self) -> Result<Vec<OsEndpoint>>;

    /// Endpoints this engine can send MIDI to. A fresh snapshot per call, as
    /// [`inputs`](Self::inputs) is, and failing as it does.
    ///
    /// # Errors
    ///
    /// As [`inputs`](Self::inputs).
    fn outputs(&self) -> Result<Vec<OsEndpoint>>;

    /// Open `id` for input, delivering events into `producer`.
    ///
    /// The returned connection must be **held**: dropping it closes the port.
    /// An implementation takes `producer` onto whatever thread the OS delivers
    /// on and pushes from there alone — one handle, one pusher.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::NotFound`](crate::MidiHardwareError::NotFound) when `id` names no current
    /// endpoint of this platform, or the platform error from opening the port.
    /// (The caller, `Devices::open_input`, has already refused an id that is not
    /// a present input.)
    fn open_input(
        &self,
        started: Started,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>>;

    /// Open `id` for output.
    ///
    /// The result speaks [`MidiOut`] — the same trait a per-unit mailbox
    /// implements — so callers route to a hardware wire and to a synth inbox
    /// through one vocabulary.
    ///
    /// # Errors
    ///
    /// As [`open_input`](Self::open_input), plus
    /// [`MidiHardwareError::Unsupported`](crate::MidiHardwareError::Unsupported) from the stub backend
    /// on a platform with no backend.
    fn open_output(&self, started: Started, id: EndpointId) -> Result<Box<dyn DeviceOut>>;
}

/// A device's output as a backend opens it: a [`MidiOut`] that is also told the
/// Protocol its packets are in.
///
/// [`OpenOutput`](crate::OpenOutput) converts a batch to the device's Protocol
/// before it queues it (`wire`), so the batch is in one Protocol. A transport that
/// tags what it sends with a Protocol (CoreMIDI's event list) must tag it with
/// that one, not the one the OS reported when the output opened: a negotiation
/// can settle on the other.
pub trait DeviceOut: MidiOut {
    /// Whether the output can still send.
    ///
    /// An output with a sender thread of its own answers under that thread's
    /// lock, held briefly, and returns `false` from then on once the thread has
    /// ended other than by a stop; it takes no more events. An output that
    /// sends inside [`queue`](MidiOut::queue) cannot end on its own, so it is
    /// alive for as long as it is open, which is the default.
    fn is_alive(&self) -> bool {
        true
    }

    /// What the output took and then could not send: a send the device refused
    /// is [`lost`](Losses::lost); an event waiting when the sender ended, and
    /// one with no form its wire carries (a message type a MIDI 1.0 byte port
    /// cannot express), is [`refused`](Losses::refused). An output that sends
    /// inside `queue` takes nothing it does not send (a failure there is the
    /// shorter count `queue` returns), so it loses nothing after taking, which
    /// is the default.
    fn losses(&self) -> Losses {
        Losses::NONE
    }

    /// How many events this crate's own sender thread has taken and not yet
    /// sent: waiting for their time or for the thread, read under the thread's
    /// lock, held briefly. An output that sends
    /// inside `queue` has no such thread, so the default is 0. For ALSA and
    /// CoreMIDI, which hand timed events to the OS's scheduler, 0 means this
    /// crate holds none, not that the OS has sent them.
    fn pending(&self) -> usize {
        0
    }

    /// The batches from now on are in `protocol`. Called when the output opens
    /// and whenever the device's configuration changes. It may run while
    /// another thread is in [`queue`](MidiOut::queue) (the Active Sensing thread
    /// queues beside the control thread), so an implementation keeps the
    /// protocol in an atomic. The default is for a transport that does not
    /// tag.
    fn follow(&self, _protocol: Protocol) {}
}
