//! A virtual MIDI destination on CoreMIDI, in the UMP protocol the spec asks
//! for: the inbound counterpart to [`UmpVirtualSource`](super::UmpVirtualSource).
//!
//! Over a MIDI-1.0 transport every inbound message arrives as legacy bytes.
//! Most of MIDI 2.0 survives that (MIDI-CI is Universal SysEx by design,
//! M2-101), but **UMP Stream** (message type 0xF, M2-104 §7.1) has no MIDI-1.0
//! encoding at all: it arrives only over a UMP endpoint, which is what this
//! publishes.
//!
//! [`UmpVirtualDestination`] hands every received UMP message to a callback.
//! The endpoint and its receive block are `tutti-coremidi-ffi`'s
//! (`UmpDestination`); this module names the endpoint and puts its errors in this
//! crate's. A spec that declares Function Blocks is created as a macOS 15
//! `MIDIUMPMutableEndpoint` (`UmpEndpoint`) with its blocks registered and
//! enabled, so other apps see them; the endpoint has a source too, which this
//! destination never sends on.
//!
//! CoreMIDI delivers a packet as a *packed word stream*: several concatenated
//! UMP messages with no separators. The endpoint splits it, so the callback
//! gets one message at a time with the packet's time.

use tracing::debug;
use tutti_coremidi_ffi::{UmpDestination, UmpEndpoint};
use tutti_ump::{FunctionBlockId, Ump};

use super::virtual_source::protocol_of;
use crate::error::Result;
use crate::virtual_endpoint::{DestinationBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::MainThreadMarker;

/// A CoreMIDI virtual destination that speaks the UMP protocol it was created
/// with.
///
/// Other apps see a UMP-capable destination and can send it native UMP,
/// including the UMP-Stream messages that have no MIDI-1.0 form. Each received
/// message is passed to the callback supplied at construction, on CoreMIDI's own
/// delivery thread. Dropping it tears the endpoint down.
#[must_use = "dropping it removes the virtual destination"]
pub struct UmpVirtualDestination {
    // Owns the endpoint's lifetime; the callback installed at creation does the
    // work.
    destination: Destination,
    name: String,
}

/// The endpoint a destination receives at.
enum Destination {
    /// A bare destination: no Function Blocks.
    Plain(UmpDestination),
    /// A UMP Endpoint that declares them.
    WithBlocks(UmpEndpoint),
}

impl Destination {
    /// How many panics in the receive callback have been caught.
    fn faults(&self) -> u64 {
        match self {
            Self::Plain(destination) => destination.faults(),
            Self::WithBlocks(endpoint) => endpoint.sender().faults(),
        }
    }
}

impl core::fmt::Debug for UmpVirtualDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpVirtualDestination")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The endpoint catches a panic in its receive callback and counts it
/// ([`UmpDestination::faults`]); after one the callback delivers nothing, so
/// the destination reads as gone.
impl DestinationBackend for UmpVirtualDestination {
    fn is_alive(&self) -> bool {
        self.destination.faults() == 0
    }

    fn block_numbers(&self) -> Option<&[FunctionBlockId]> {
        match &self.destination {
            Destination::Plain(_) => None,
            Destination::WithBlocks(endpoint) => Some(endpoint.blocks()),
        }
    }
}

impl UmpVirtualDestination {
    /// Publishes the destination `spec` describes, pushing every received
    /// message into `producer` exactly as a hardware input port does: the
    /// events land in the same [`HardwareMidiInputs`](crate::HardwareMidiInputs)
    /// rings a physical endpoint feeds. A spec that declares Function Blocks
    /// publishes them on a UMP Endpoint.
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
        mut producer: crate::InputProducerHandle,
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

    /// Creates a virtual destination named `name` carrying `protocol`, invoking
    /// `on_event` for every UMP message received.
    ///
    /// `on_event` runs on CoreMIDI's delivery thread, so it must not block.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Os`] if CoreMIDI refuses either the client or the
    /// destination endpoint.
    #[cfg(test)]
    pub(crate) fn new<F>(name: &str, protocol: VirtualProtocol, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump) + Send + 'static,
    {
        Self::stamped(
            name,
            protocol,
            None,
            // A plain destination (no blocks) is no main-thread object, so the
            // fixture's stand-in marker is enough; never use it for blocks.
            tutti_fixtures::main_thread(),
            move |event, _arrived| on_event(event),
        )
    }

    /// Creates a virtual destination named `name` carrying `protocol`, invoking
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
        // The device watch's client starts before this endpoint's (`hub_first`).
        let client = super::hub_first()?;
        let on_ump = move |time, event| {
            on_event(
                event,
                super::host_time::Pairing::now().instant_of_stamp(time),
            );
        };
        // A spec that declares blocks is a UMP Endpoint; anything else a bare destination.
        let destination = match spec.filter(|spec| !spec.function_blocks().is_empty()) {
            Some(spec) => Destination::WithBlocks(
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
            None => Destination::Plain(UmpDestination::new(
                client,
                name,
                protocol_of(protocol),
                on_ump,
            )?),
        };

        debug!(name, ?protocol, "Created virtual MIDI destination");
        Ok(Self {
            destination,
            name: name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
