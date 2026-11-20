//! A virtual MIDI endpoint on CoreMIDI, in the UMP protocol the spec asks for:
//! one name that other apps send to and read from.
//!
//! A UMP endpoint is created with `MIDISourceCreateWithProtocol` and
//! `MIDIDestinationCreateWithProtocol` and carries UMP words, so a MIDI-2-only
//! message (per-note controllers, JR Timestamps) reaches the wire; the raw CoreMIDI
//! calls for it are `tutti-coremidi-ffi`'s (`UmpSource`, `UmpDestination`), and this
//! module names the endpoint and puts its errors in this crate's.
//!
//! Over a MIDI-1.0 transport every inbound message arrives as legacy bytes. Most of
//! MIDI 2.0 survives that (MIDI-CI is Universal SysEx by design, M2-101), but
//! **UMP Stream** (message type 0xF, M2-104 §7.1) has no MIDI-1.0 encoding at all:
//! it arrives only over a UMP endpoint, which is what this publishes. CoreMIDI
//! delivers a packet as a *packed word stream*: several concatenated UMP messages
//! with no separators. The destination splits it, so the callback gets one message
//! at a time with the packet's time.
//!
//! A spec that declares Function Blocks is created as a macOS 15
//! `MIDIUMPMutableEndpoint` (`UmpEndpoint`), one object with a source and a
//! destination, with its blocks registered and enabled so other apps see them. A
//! spec with none is a source and a destination under the spec's name, which is
//! what CoreMIDI offers for an endpoint with no blocks to declare.
//!
//! [`UmpVirtualEndpoint`] owns the endpoint, disposed on drop.

use std::sync::{Arc, Weak};

use tracing::debug;
use tutti_coremidi_ffi::{UmpDestination, UmpEndpoint, UmpSender, UmpSource};
use tutti_platform::MainThreadMarker;
use tutti_ump::{FunctionBlockId, Protocol, Ump};

use crate::InputProducerHandle;
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{EndpointBackend, SenderBackend, VirtualProtocol, VirtualSpec};

/// CoreMIDI's protocol for the spec's.
fn protocol_of(protocol: VirtualProtocol) -> Protocol {
    match protocol {
        VirtualProtocol::Midi1 => Protocol::Midi1,
        VirtualProtocol::Midi2 => Protocol::Midi2,
    }
}

/// A CoreMIDI virtual endpoint that speaks the UMP protocol it was created with.
///
/// It carries UMP words end to end, so JR Timestamps and other MIDI-2-only
/// messages survive to the wire in both directions. Each received message is
/// passed to the callback supplied at construction, on CoreMIDI's own delivery
/// thread. Owns its endpoint, which is disposed on drop. With Function Blocks the
/// endpoint is a main-thread object, so this type is `!Send`; what sends from
/// another thread is the [`SenderBackend`] it hands out.
#[must_use = "dropping it removes the virtual endpoint"]
pub struct UmpVirtualEndpoint {
    endpoint: Endpoint,
    name: String,
}

/// The CoreMIDI objects an endpoint is.
enum Endpoint {
    /// A source and a destination of one name: no Function Blocks. The senders
    /// hold the source weakly.
    Pair {
        source: Arc<UmpSource>,
        destination: UmpDestination,
    },
    /// A UMP Endpoint that declares blocks.
    WithBlocks(UmpEndpoint),
}

impl core::fmt::Debug for UmpVirtualEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns opaque CoreMIDI handles; report the endpoint name only.
        f.debug_struct("UmpVirtualEndpoint")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl UmpVirtualEndpoint {
    /// Publishes the endpoint `spec` describes, pushing every received message
    /// into `producer` exactly as a hardware input port does: the events land in
    /// the same [`HardwareMidiInputs`](crate::HardwareMidiInputs) rings a physical
    /// endpoint feeds. A spec that declares Function Blocks publishes them on a UMP
    /// Endpoint. `main` is the proof the endpoint is made on the main thread.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::VirtualBlocksUnsupported`](crate::MidiHardwareError::VirtualBlocksUnsupported)
    /// when `spec` declares blocks and the OS is before macOS 15, and
    /// [`MidiHardwareError::Os`](crate::MidiHardwareError::Os) if CoreMIDI refuses
    /// the client or the endpoint, or cannot carry a block (a MIDI-CI version, a
    /// block number that is not its position).
    pub(crate) fn open(
        spec: &VirtualSpec,
        mut producer: InputProducerHandle,
        main: MainThreadMarker,
    ) -> Result<Self> {
        Self::stamped(
            spec.name().as_str(),
            spec.protocol,
            Some(spec),
            main,
            move |event, arrived| {
                // A full inlet counts the drop itself; a log line would run on
                // CoreMIDI's thread.
                let _ = producer.push(event, arrived);
            },
        )
    }

    /// Creates a virtual endpoint named `name` carrying `protocol`, invoking
    /// `on_event` for every UMP message received.
    ///
    /// `on_event` runs on CoreMIDI's delivery thread, so it must not block.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Os`] if CoreMIDI refuses either the client or an
    /// endpoint.
    #[cfg(test)]
    pub(crate) fn new<F>(name: &str, protocol: VirtualProtocol, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump) + Send + 'static,
    {
        Self::stamped(
            name,
            protocol,
            None,
            // An endpoint with no blocks is no main-thread object, so the
            // fixture's stand-in marker is enough; never use it for blocks.
            tutti_fixtures::main_thread(),
            move |event, _arrived| on_event(event),
        )
    }

    /// Creates a virtual endpoint named `name` carrying `protocol`, invoking
    /// `on_event` for every UMP message received with the `Instant` its packet
    /// reached CoreMIDI (the packet's host time).
    fn stamped<F>(
        name: &str,
        protocol: VirtualProtocol,
        spec: Option<&VirtualSpec>,
        main: MainThreadMarker,
        mut on_event: F,
    ) -> Result<Self>
    where
        F: FnMut(Ump, tutti_platform::Instant) + Send + 'static,
    {
        // The device watch's client starts before this endpoint's (`hub_first`):
        // CoreMIDI sends notifications to the run loop of the process's first client.
        let client = super::hub_first()?;
        let on_ump = move |time, event| {
            on_event(
                event,
                super::host_time::Pairing::now().instant_of_stamp(time),
            );
        };
        // A spec that declares blocks is a UMP Endpoint; anything else a pair.
        let endpoint = match spec.filter(|spec| !spec.function_blocks().is_empty()) {
            Some(spec) => Endpoint::WithBlocks(
                UmpEndpoint::new(
                    main,
                    name,
                    protocol_of(protocol),
                    spec.device(),
                    spec.instance(),
                    &super::block_infos(spec),
                    on_ump,
                )
                .map_err(super::open_error)?,
            ),
            None => Endpoint::Pair {
                source: Arc::new(UmpSource::new(client, name, protocol_of(protocol))?),
                destination: UmpDestination::new(client, name, protocol_of(protocol), on_ump)?,
            },
        };
        debug!(name, ?protocol, "Created virtual MIDI endpoint");
        Ok(Self {
            endpoint,
            name: name.to_string(),
        })
    }
}

/// A send on a source, which it does not keep alive.
struct PairSender(Weak<UmpSource>);

impl SenderBackend for PairSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the endpoint is dropped, and
    /// [`MidiHardwareError::Os`] when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    fn send(&self, ump: &Ump) -> Result<()> {
        let source = self.0.upgrade().ok_or(MidiHardwareError::EndpointGone)?;
        Ok(source.send(ump)?)
    }
}

/// A send on a UMP Endpoint with Function Blocks.
struct EndpointSender(UmpSender);

impl SenderBackend for EndpointSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the endpoint is dropped, and
    /// [`MidiHardwareError::Os`] when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    fn send(&self, ump: &Ump) -> Result<()> {
        self.0.send(ump).map_err(|error| match error {
            tutti_coremidi_ffi::Error::EndpointGone => MidiHardwareError::EndpointGone,
            other => MidiHardwareError::Os(other),
        })
    }
}

/// The destination catches a panic in its receive callback and counts it
/// ([`UmpDestination::faults`]); after one the callback delivers nothing, so the
/// endpoint reads as gone.
impl EndpointBackend for UmpVirtualEndpoint {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        match &self.endpoint {
            Endpoint::Pair { source, .. } => Arc::new(PairSender(Arc::downgrade(source))),
            Endpoint::WithBlocks(endpoint) => Arc::new(EndpointSender(endpoint.sender())),
        }
    }

    fn is_alive(&self) -> bool {
        let faults = match &self.endpoint {
            Endpoint::Pair { destination, .. } => destination.faults(),
            Endpoint::WithBlocks(endpoint) => endpoint.faults(),
        };
        faults == 0
    }

    fn block_numbers(&self) -> Option<&[FunctionBlockId]> {
        match &self.endpoint {
            Endpoint::Pair { .. } => None,
            Endpoint::WithBlocks(endpoint) => Some(endpoint.blocks()),
        }
    }
}

#[cfg(test)]
mod tests;
