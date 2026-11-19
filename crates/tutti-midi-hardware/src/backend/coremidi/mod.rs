//! The macOS backend: CoreMIDI, native UMP.
//!
//! # Almost no raw FFI
//!
//! Enumeration is `tutti-coremidi-ffi`'s (`sources`, `destinations`, `find_source`),
//! which lists endpoints with their `UniqueId`, name and protocol read from one
//! object and takes one again by id, never by list position. The safe `coremidi`
//! wrapper still covers the port path —
//! `Client::input_port_with_protocol`
//! (`MIDIInputPortCreateWithProtocol`), `InputPortWithContext::connect_source`,
//! `OutputPort::send` (`MIDISendEventList`), `EventBuffer::push(ts, &[u32])`
//! which takes UMP words directly.
//!
//! The one raw call it lacks, `MIDISourceCreateWithProtocol` (the wrapper has
//! `virtual_destination_with_protocol` but **no** `virtual_source_with_protocol`),
//! and the Mach clock are `tutti-coremidi-ffi`'s, behind safe handles
//! ([`virtual_source`], `host_time`); this crate forbids `unsafe`.
//!
//! # Protocol is asked, not assumed
//!
//! An input port opens in the protocol of its source (one port per opened
//! input): `Protocol::Midi10` for a source whose capability is MIDI 1.0,
//! so CoreMIDI hands it MIDI 1.0 UMP (type 2) and the input node's `Midi2Translator`
//! translates it, never the OS; `Protocol::Midi20` otherwise.
//! A MIDI 1.0 source is not a device speaking MIDI 2.0, so the capability records `kMIDIPropertyProtocolID` as
//! the OS reports it — and falls back to `Midi1` when the property is absent,
//! which is the conservative direction (see `OsEndpoint::capability`).
//!
//! # Function Blocks (macOS 15)
//!
//! Where the OS has a `MIDIUMPEndpoint` for an endpoint (read by
//! `tutti-coremidi-ffi`, which is where the Objective-C classes are bound), the
//! capability is that endpoint's: the Protocol in use and its Function Blocks,
//! which CoreMIDI has already discovered (Stream negotiation is the OS's for a
//! device it presents as UMP). Before macOS 15 the classes do not exist and the
//! capability is the property's alone, with no blocks.

use crate::InputFault;
use coremidi::{
    Client, Destinations, EventBuffer, InputPortWithContext, OutputPort, Protocol as CmProtocol,
    Source,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_rt::{FaultLatch, LockExt};

use crate::InputProducerHandle;
use crate::backend::by_message::{Refused, by_message};
use crate::capability::{OsEndpoint, carries_midi2_only, from_os, legacy};
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::guard::Guard;
use tutti_coremidi_ffi::{
    Endpoint as OsObject, Operation, OsStatus, SharedClient, UmpEndpointInfo, UmpEndpointsError,
    UniqueId, ump_endpoints,
};
use tutti_midi_device::{Endpoint, EndpointId, Identity, OsDevice};
use tutti_midi_runtime::{MidiOut, Outgoing, Taken};
use tutti_ump::{Protocol, read_protocol, split_stream};

mod host_time;
mod virtual_destination;
mod virtual_source;
mod watch;
pub(crate) use virtual_destination::UmpVirtualDestination;
pub(crate) use virtual_source::UmpVirtualSource;

/// The CoreMIDI client the ports of every backend share: the interim second client
/// of the process (the first is `SharedClient`, which the watch and the virtual
/// endpoints use), until the ports move to `tutti-coremidi-ffi`.
///
/// coremidi 0.9 never disposes a [`Client`] on drop (Apple: disposing the last
/// client may make the MIDI server exit and fail every later client creation),
/// so a client per endpoint set or virtual port would accumulate. One is created
/// on first use and kept for the life of the process; a failed creation is not
/// cached, so a later call retries.
pub(crate) fn shared_client() -> std::result::Result<&'static Client, OsStatus> {
    // A refused hub client is `Client::new`'s to refuse again, with its status.
    let _ = hub_first();
    static CLIENT: std::sync::Mutex<Option<&'static Client>> = std::sync::Mutex::new(None);
    let mut slot = CLIENT.lock_recover();
    if let Some(client) = *slot {
        return Ok(client);
    }
    let client: &'static Client = Box::leak(Box::new(tutti_coremidi_ffi::create_client(|| {
        Client::new("tutti-midi").map_err(OsStatus::new)
    })?));
    *slot = Some(client);
    Ok(client)
}

/// Starts the device watch's client and returns it. CoreMIDI sends notifications
/// to the run loop of the process's first client, so every place this backend
/// creates a client (`shared_client`, `UmpVirtualSource::new`,
/// `UmpVirtualDestination::stamped`) calls this first, before taking its own
/// lock; the virtual endpoints are created on the client it returns.
///
/// Rule: nothing the hub thread runs may call back into those creation sites
/// (they wait for the hub to start, and the hub thread's `snapshot` is
/// `CoreMidiEndpoints::devices`, which builds no client), or startup deadlocks.
pub(crate) fn hub_first() -> Result<SharedClient> {
    watch::client()
}

/// An endpoint's name: the display name Audio MIDI Setup shows, else its bare name,
/// else a label from its unique id (never its list position, which shifts).
fn endpoint_name(endpoint: &OsObject) -> String {
    endpoint
        .name()
        .unwrap_or_else(|| format!("MIDI Endpoint {}", endpoint.id()))
}

/// What the OS says this endpoint carries.
///
/// The UMP Endpoint the OS keeps for it (`ump`, which has the endpoint's
/// `unique_id` as its source or destination), when there is one: its Protocol in
/// use and its blocks. Otherwise the endpoint's property. An absent or unreadable `kMIDIPropertyProtocolID` means "the OS did not say",
/// which resolves to MIDI 1.0 — claiming MIDI 2.0 on no evidence would feed
/// per-note messages to an endpoint that cannot carry them.
fn endpoint_capability(endpoint: &OsObject, ump: Option<&UmpEndpointInfo>) -> Endpoint {
    if let Some(ump) = ump {
        return ump_capability(ump);
    }
    match endpoint.protocol_id() {
        // `kMIDIProtocol_1_0` is 1 and `kMIDIProtocol_2_0` is 2; any other value
        // is reserved, which reads as MIDI 1.0.
        Some(id) => from_os(read_protocol(u8::try_from(id).unwrap_or(u8::MAX)), None, []),
        None => legacy(),
    }
}

/// The protocol an input port on `source` is created with, as ALSA chooses
/// its client's version: MIDI 1.0 for a source that cannot
/// speak the MIDI 2.0 Protocol, so CoreMIDI hands it MIDI 1.0 UMP (type 2)
/// and the input node's `Midi2Translator` translates it, never the OS. A UMP Endpoint
/// that supports MIDI 2.0 opens as MIDI 2.0 even while it is in MIDI 1.0:
/// the session asks every Endpoint for MIDI 2.0, and a MIDI 1.0 port would
/// have CoreMIDI translate what it then sends natively. A source with no UMP
/// Endpoint record (a MIDI 1.0 device, or macOS before 15) follows
/// `kMIDIPropertyProtocolID`.
fn input_protocol(source: &OsObject, ump: Option<&UmpEndpointInfo>) -> CmProtocol {
    let midi2 = match ump {
        Some(ump) => ump.offered.midi2,
        None => carries_midi2_only(&endpoint_capability(source, None)),
    };
    if midi2 {
        CmProtocol::Midi20
    } else {
        CmProtocol::Midi10
    }
}

/// A UMP Endpoint as the Endpoint it describes: the Protocol it has in use
/// (having blocks is not having the MIDI 2.0 Protocol), the protocols it offers
/// and the blocks the spec can hold.
fn ump_capability(ump: &UmpEndpointInfo) -> Endpoint {
    from_os(
        ump.protocol,
        Some(&ump.offered),
        ump.blocks
            .iter()
            .map(|block| (block.info, block.name.as_deref())),
    )
}

/// The UMP Endpoint whose source (`input`) or destination has unique id `id`.
fn ump_of(all: &[UmpEndpointInfo], id: UniqueId, input: bool) -> Option<&UmpEndpointInfo> {
    all.iter().find(|e| {
        let own = if input {
            e.source_unique_id
        } else {
            e.destination_unique_id
        };
        own == Some(id)
    })
}

/// The [`EndpointId`] of an endpoint with unique id `id`.
///
/// `kMIDIPropertyUniqueID` survives a device list refresh; the enumeration index
/// does not, and an [`EndpointId`] promises the same endpoint across a refresh.
/// An endpoint without a unique id is therefore left out of the list rather than
/// handed an index that would name a different device after a hot-plug.
fn endpoint_id(id: UniqueId) -> EndpointId {
    EndpointId::from_raw(u64::from(id.get().cast_unsigned()))
}

/// The unique id an [`EndpointId`] was made from (`endpoint_id`'s inverse), `None`
/// for a handle this backend never minted.
fn unique_id_of(id: EndpointId) -> Option<UniqueId> {
    let raw = u32::try_from(id.raw()).ok()?;
    Some(UniqueId::new(raw.cast_signed()))
}

/// The macOS [`MidiEndpoints`] implementation.
pub struct CoreMidiEndpoints {
    /// The process-wide client ([`shared_client`]), or the status CoreMIDI
    /// refused it with.
    client: std::result::Result<&'static Client, OsStatus>,
}

impl core::fmt::Debug for CoreMidiEndpoints {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CoreMidiEndpoints").finish_non_exhaustive()
    }
}

impl Default for CoreMidiEndpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreMidiEndpoints {
    /// Creates the backend on the process-wide CoreMIDI client.
    ///
    /// A failed client is kept rather than panicking: enumeration then
    /// reports nothing and opening reports the status it failed with, which is
    /// what a headless machine or a sandbox without MIDI access should look
    /// like.
    ///
    /// While the MIDI server restarts (CoreMIDI refuses clients for about five
    /// seconds) the first call waits, up to 10 s once per process window; calls
    /// made within 10 s of a failed wait fail at once.
    pub fn new() -> Self {
        Self {
            client: shared_client(),
        }
    }

    fn client(&self) -> Result<&'static Client> {
        self.client.map_err(|status| {
            MidiHardwareError::Os(tutti_coremidi_ffi::Error::call(
                Operation::CreateClient,
                status,
            ))
        })
    }

    /// A `FindError` is `NotFound::Endpoint` to the caller, so a list shift and a
    /// removal look alike.
    ///
    /// The source `id` names, as the endpoint the OS lists and as the object a port
    /// connects to. The id is resolved by `kMIDIPropertyUniqueID`, and the object
    /// taken is checked to read it, so a list that shifted since the listing fails.
    fn find_source(id: EndpointId) -> Option<(OsObject, Source)> {
        let unique = unique_id_of(id)?;
        let endpoint = tutti_coremidi_ffi::find_source(unique)
            .inspect_err(|error| tracing::debug!(%error, "CoreMIDI source not taken"))
            .ok()?;
        // Interim until the ports move to `tutti-coremidi-ffi`: the port connects
        // to the safe wrapper's `Source`, taken again by the same id; a removal
        // in between is `NotFound`.
        let source = Source::from_unique_id(unique.get().cast_unsigned())?;
        Some((endpoint, source))
    }

    /// The destination `id` names, as `find_source` takes a source.
    fn find_destination(id: EndpointId) -> Option<coremidi::Destination> {
        let unique = unique_id_of(id)?;
        tutti_coremidi_ffi::find_destination(unique)
            .inspect_err(|error| tracing::debug!(%error, "CoreMIDI destination not taken"))
            .ok()?;
        // Interim until the ports move to `tutti-coremidi-ffi`: the safe wrapper
        // takes a destination by position only, so the object is the one whose
        // own id reads `unique`, read from the object taken and never from a
        // position read earlier; a removal in between is `NotFound`.
        let wanted = unique.get().cast_unsigned();
        (0..Destinations::count()).find_map(|index| {
            let destination = coremidi::Destination::from_index(index)?;
            (destination.unique_id()? == wanted).then_some(destination)
        })
    }
}

impl CoreMidiEndpoints {
    /// Every device now. Run by the hub thread: it must not create a client
    /// (`hub_first`).
    fn devices() -> Vec<OsDevice> {
        os_devices(&list_inputs(), &list_outputs())
    }
}

/// The UMP Endpoints the OS reports; none where the classes are absent (before
/// macOS 15) or CoreMIDI raised (no reachable MIDI server), which leaves each
/// endpoint's MIDI 1.0 view. The first exception is logged at `debug`.
///
/// Runs on the hub or a control thread, never the audio thread: it logs and
/// allocates.
fn os_umps() -> Vec<UmpEndpointInfo> {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    match ump_endpoints() {
        Ok(umps) => umps,
        Err(UmpEndpointsError::Unsupported) => Vec::new(),
        Err(UmpEndpointsError::Exception(exception)) => {
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::debug!(%exception, "CoreMIDI raised while listing UMP Endpoints");
            }
            Vec::new()
        }
    }
}

/// An endpoint the OS lists, as the endpoint it describes.
fn os_endpoint(endpoint: &OsObject, ump: Option<&UmpEndpointInfo>) -> OsEndpoint {
    OsEndpoint {
        id: endpoint_id(endpoint.id()),
        identity: Identity::CoreMidi(endpoint.id().get()),
        name: endpoint_name(endpoint),
        capability: endpoint_capability(endpoint, ump),
    }
}

/// The sources now, as endpoints.
fn list_inputs() -> Vec<OsEndpoint> {
    let umps = os_umps();
    tutti_coremidi_ffi::sources()
        .iter()
        .map(|source| os_endpoint(source, ump_of(&umps, source.id(), true)))
        .collect()
}

fn list_outputs() -> Vec<OsEndpoint> {
    let umps = os_umps();
    tutti_coremidi_ffi::destinations()
        .iter()
        .map(|destination| os_endpoint(destination, ump_of(&umps, destination.id(), false)))
        .collect()
}

impl MidiEndpoints for CoreMidiEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        watch::start(sink, Self::devices)
    }

    fn inputs(&self) -> Vec<OsEndpoint> {
        list_inputs()
    }

    fn outputs(&self) -> Vec<OsEndpoint> {
        list_outputs()
    }

    fn open_input(
        &self,
        id: EndpointId,
        mut producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let client = self.client()?;
        let (endpoint, source) = Self::find_source(id).ok_or(NotFound::Endpoint(id))?;
        let unique = endpoint.id();

        // The port speaks the source's protocol: a MIDI 1.0 source is delivered
        // as MIDI 1.0 UMP (type 2), which the input node's `Midi2Translator`
        // translates; CoreMIDI up-converts nothing. A MIDI 2.0 source is delivered
        // as MIDI 2.0 UMP.
        //
        // No byte parser on this path, deliberately: CoreMIDI delivers UMP
        // words either way, and a MIDI-1.0 device's SysEx arrives already
        // fragmented into UMP SysEx7 packets, which `tutti_ump::SysexAssembler`
        // (one layer up) rejoins. `tutti_ump::Midi1Parser` is for a transport
        // that delivers raw `F0 … F7` bytes.
        let endpoints = os_umps();
        let protocol = input_protocol(&endpoint, ump_of(&endpoints, unique, true));
        // CoreMIDI calls this closure from its own thread through a block, so
        // a panic must not leave it: the guard catches it and ends the input.
        let died = Arc::new(FaultLatch::<InputFault>::new());
        let guard = Guard::new(Arc::clone(&died));
        let mut port = client
            .input_port_with_protocol("tutti-in", protocol, move |event_list, _ctx| {
                guard.run(|| {
                    // The packet's own host time, as an `Instant`: when the event
                    // reached CoreMIDI, not when this callback ran.
                    let pairing = host_time::Pairing::now();
                    for packet in event_list.iter() {
                        let arrived = pairing.instant_of(packet.timestamp());
                        // One CoreMIDI packet is a packed word stream of several
                        // concatenated UMP messages.
                        for event in split_stream(packet.data()) {
                            // A full inlet counts the drop itself; a log line
                            // would run on CoreMIDI's thread.
                            let _ = producer.push(event, arrived);
                        }
                    }
                });
            })
            .map_err(|status| {
                tutti_coremidi_ffi::Error::call(Operation::CreateInputPort, OsStatus::new(status))
            })?;

        port.connect_source(&source, ()).map_err(|status| {
            tutti_coremidi_ffi::Error::call(Operation::ConnectSource, OsStatus::new(status))
        })?;

        Ok(Box::new(CoreMidiInput { _port: port, died }))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let client = self.client()?;
        let destination = Self::find_destination(id).ok_or(NotFound::Endpoint(id))?;

        let port = client.output_port("tutti-out").map_err(|status| {
            tutti_coremidi_ffi::Error::call(Operation::CreateOutputPort, OsStatus::new(status))
        })?;

        Ok(Box::new(CoreMidiOutput {
            port,
            destination,
            // `OpenOutput` says which, before anything is queued (`follow`).
            midi2: AtomicBool::new(false),
        }))
    }
}

/// An open CoreMIDI input. Dropping it closes the port.
struct CoreMidiInput {
    /// Held for its `Drop`: releasing the port is what disconnects.
    _port: InputPortWithContext<()>,
    /// Raised by the callback's [`Guard`] when it panicked.
    died: Arc<FaultLatch<InputFault>>,
}

impl InputConnection for CoreMidiInput {
    fn fault(&self) -> Option<InputFault> {
        self.died.cause()
    }
}

/// An open CoreMIDI output, as a [`MidiOut`].
///
/// No interior mutability: `OutputPort::send` takes `&self` and each call builds
/// its own [`EventBuffer`], so `MidiOut::queue(&self, ..)` needs no lock.
///
/// The per-call buffer allocates. That is acceptable *here* and would not be on
/// the audio thread: this is the control-thread send path, reached from the
/// output drain, not from `process()`.
struct CoreMidiOutput {
    port: OutputPort,
    destination: coremidi::Destination,
    /// Whether the event lists go out tagged MIDI 2.0: the Protocol the output
    /// is in now ([`DeviceOut::follow`]), which a negotiation can change after
    /// the OS reported its own at open.
    midi2: AtomicBool,
}

impl CoreMidiOutput {
    /// Sends one message as one event list, so the OS takes all its packets or
    /// none: each packet is stamped with the host time of its `SendAt`, which
    /// CoreMIDI holds it until (0 is now). `pairing` is the batch's reading of
    /// the host clock against `Instant`, taken at its first scheduled packet:
    /// read afresh for each batch, so it neither drifts nor spans a sleep
    /// (`Instant` and `mach_absolute_time` differ there).
    fn send(
        &self,
        message: &[Outgoing],
        pairing: &mut Option<host_time::Pairing>,
    ) -> std::result::Result<(), tutti_coremidi_ffi::Error> {
        let cm_protocol = if self.midi2.load(Ordering::Relaxed) {
            CmProtocol::Midi20
        } else {
            CmProtocol::Midi10
        };
        let mut buffer = EventBuffer::new(cm_protocol);
        for event in message {
            let time = host_time::stamp_of(event.send, pairing);
            buffer = buffer.with_packet(time, event.ump.words());
        }
        self.port
            .send(&self.destination, &buffer)
            .map_err(|status| {
                tutti_coremidi_ffi::Error::call(Operation::SendEventList, OsStatus::new(status))
            })
    }
}

impl DeviceOut for CoreMidiOutput {
    fn follow(&self, protocol: Protocol) {
        // A reserved protocol value is one nobody can speak: unknown means
        // MIDI 1.0, the conservative direction (see `OsEndpoint::capability`).
        self.midi2
            .store(protocol == Protocol::Midi2, Ordering::Relaxed);
    }
}

impl MidiOut for CoreMidiOutput {
    /// Each message goes to CoreMIDI as one event list, its packets stamped
    /// with the host time of their `SendAt`; CoreMIDI schedules them, and
    /// takes a message whole or not at all ([`by_message`]).
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        let mut pairing = None;
        by_message(events, "CoreMIDI", |message| {
            self.send(message.packets(), &mut pairing)
                .map_err(|error| Refused { error, sent: 0 })
        })
        .taken
    }
}

#[cfg(test)]
mod tests;
