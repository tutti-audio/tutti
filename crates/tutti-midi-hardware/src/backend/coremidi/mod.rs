//! The macOS backend: CoreMIDI, native UMP.
//!
//! # Almost no raw FFI
//!
//! Enumeration is `tutti-coremidi-ffi`'s (`sources`, `destinations`, `find_source`),
//! which lists endpoints with their `UniqueId`, name and protocol read from one
//! object and takes one again by id, never by list position. Every CoreMIDI call is
//! `tutti-coremidi-ffi`'s, behind safe handles: the process's one client, the input
//! and output ports (UMP words in and out), the virtual endpoint ([`virtual_endpoint`]), the run loop of the device watch ([`watch`]) and the Mach
//! clock (`host_time`); this crate forbids `unsafe`.
//!
//! # Protocol is asked, not assumed
//!
//! An input port opens in the protocol of its source (one port per opened
//! input): `Protocol::Midi1` for a source whose capability is MIDI 1.0,
//! so CoreMIDI hands it MIDI 1.0 UMP (type 2) and the input node's `Midi2Translator`
//! translates it, never the OS; `Protocol::Midi2` otherwise.
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
use std::sync::atomic::{AtomicBool, Ordering};

use crate::InputProducerHandle;
use crate::backend::by_message::{Refused, by_message};
use crate::capability::{OsEndpoint, carries_midi2_only, from_os, legacy};
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::virtual_endpoint::VirtualSpec;
use tutti_coremidi_ffi::{
    Endpoint as OsObject, InputPort, OutputPort, SharedClient, UmpEndpointInfo, UmpEndpointsError,
    UniqueId, ump_endpoints,
};
use tutti_midi_device::{Endpoint, EndpointId, Identity, OsDevice};
use tutti_midi_runtime::{MidiOut, Outgoing, Taken};
use tutti_ump::{FunctionBlockInfo, Protocol, read_protocol};

mod host_time;
mod virtual_endpoint;
mod watch;
pub(crate) use virtual_endpoint::UmpVirtualEndpoint;

/// The process's one CoreMIDI client, started with the device watch's hub on first
/// use. CoreMIDI sends notifications to the run loop of the thread that created the
/// process's first client, and the hub thread creates it, so the endpoints and ports
/// of this backend are all created on the client this returns.
///
/// Rule: nothing the hub thread runs may call back into this (it waits for the hub
/// to start, and the hub thread's `snapshot` is `CoreMidiEndpoints::devices`, which
/// creates no endpoint or port), or startup deadlocks.
pub(crate) fn hub_first() -> Result<SharedClient> {
    watch::client()
}

/// The Function Blocks `spec` declares, as the Function Block Info and name
/// `tutti-coremidi-ffi` creates them from.
fn block_infos(spec: &VirtualSpec) -> Vec<(FunctionBlockInfo, Option<&str>)> {
    spec.function_blocks()
        .iter()
        .map(|block| (block.info(), block.name().map(|name| name.as_str())))
        .collect()
}

/// A failure to create an endpoint with Function Blocks as this crate reports it:
/// the OS having no such endpoint (before macOS 15) is
/// [`MidiHardwareError::VirtualBlocksUnsupported`], anything else CoreMIDI's own.
fn open_error(error: tutti_coremidi_ffi::Error) -> MidiHardwareError {
    match error {
        tutti_coremidi_ffi::Error::UmpEndpointUnsupported => {
            MidiHardwareError::VirtualBlocksUnsupported
        }
        other => MidiHardwareError::Os(other),
    }
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
fn input_protocol(source: &OsObject, ump: Option<&UmpEndpointInfo>) -> Protocol {
    let midi2 = match ump {
        Some(ump) => ump.offered.midi2,
        None => carries_midi2_only(&endpoint_capability(source, None)),
    };
    if midi2 {
        Protocol::Midi2
    } else {
        Protocol::Midi1
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
#[derive(Debug)]
pub struct CoreMidiEndpoints;

impl Default for CoreMidiEndpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreMidiEndpoints {
    /// Creates the backend on the process's one CoreMIDI client.
    ///
    /// A failed client is not kept: enumeration then reports nothing and opening
    /// reports the error it failed with, which is what a headless machine or a
    /// sandbox without MIDI access should look like.
    ///
    /// While the MIDI server restarts (CoreMIDI refuses clients for about five
    /// seconds) the first call waits, up to 10 s once per process window; calls
    /// made within 10 s of a failed wait fail at once.
    pub fn new() -> Self {
        let _ = hub_first();
        Self
    }

    /// The source `id` names, as the endpoint the OS lists and as the object a port
    /// connects to. The id is resolved by `kMIDIPropertyUniqueID`, and the object
    /// taken is checked to read it, so a list that shifted since the listing fails.
    /// A `FindError` is `NotFound::Endpoint` to the caller, so a list shift and a
    /// removal look alike.
    fn find_source(id: EndpointId) -> Option<OsObject> {
        tutti_coremidi_ffi::find_source(unique_id_of(id)?)
            .inspect_err(|error| tracing::debug!(%error, "CoreMIDI source not taken"))
            .ok()
    }

    /// The destination `id` names, as `find_source` takes a source.
    fn find_destination(id: EndpointId) -> Option<OsObject> {
        tutti_coremidi_ffi::find_destination(unique_id_of(id)?)
            .inspect_err(|error| tracing::debug!(%error, "CoreMIDI destination not taken"))
            .ok()
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
        let client = hub_first()?;
        let endpoint = Self::find_source(id).ok_or(NotFound::Endpoint(id))?;

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
        let protocol = input_protocol(&endpoint, ump_of(&endpoints, endpoint.id(), true));
        // CoreMIDI calls this closure from its own thread through a block; the
        // port catches a panic in it and ends the input (`fault`).
        let port = InputPort::connect(
            client,
            "tutti-in",
            protocol,
            &endpoint,
            // The clocks are read once per delivered list, so messages with equal
            // stamps get equal instants.
            host_time::Pairing::now,
            move |pairing, time, event| {
                // The packet's own host time, as an `Instant`: when the event
                // reached CoreMIDI, not when this callback ran. A full inlet counts
                // the drop itself; a log line would run on CoreMIDI's thread.
                let _ = producer.push(event, pairing.instant_of_stamp(time));
            },
        )?;
        Ok(Box::new(CoreMidiInput { port }))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let client = hub_first()?;
        let destination = Self::find_destination(id).ok_or(NotFound::Endpoint(id))?;
        let port = OutputPort::new(client, "tutti-out")?;
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
    /// Held for its `Drop`, which disconnects it; it counts a panic in the
    /// callback.
    port: InputPort,
}

impl InputConnection for CoreMidiInput {
    fn fault(&self) -> Option<InputFault> {
        (self.port.faults() > 0).then_some(InputFault::Panicked)
    }
}

/// An open CoreMIDI output, as a [`MidiOut`].
///
/// No interior mutability: `OutputPort::send` takes `&self` and each call builds
/// its own event list, so `MidiOut::queue(&self, ..)` needs no lock.
///
/// The per-call buffers allocate. That is acceptable *here* and would not be on
/// the audio thread: this is the control-thread send path, reached from the
/// output drain, not from `process()`.
struct CoreMidiOutput {
    port: OutputPort,
    destination: OsObject,
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
        let protocol = if self.midi2.load(Ordering::Relaxed) {
            Protocol::Midi2
        } else {
            Protocol::Midi1
        };
        let packets: Vec<_> = message
            .iter()
            .map(|event| (host_time::stamp_of(event.send, pairing), event.ump))
            .collect();
        self.port.send(&self.destination, protocol, &packets)
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
