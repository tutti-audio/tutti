//! A virtual MIDI source on CoreMIDI, in the UMP protocol the spec asks for.
//!
//! A UMP endpoint is created with `MIDISourceCreateWithProtocol` and sends UMP words,
//! so a MIDI-2-only message (per-note controllers, JR Timestamps) reaches the wire;
//! the raw CoreMIDI calls for it are `tutti-coremidi-ffi`'s (`UmpSource`), and this
//! module names the endpoint and puts its errors in this crate's.
//!
//! A spec that declares Function Blocks is created as a macOS 15
//! `MIDIUMPMutableEndpoint` (`UmpEndpoint`) with its blocks registered and enabled,
//! so other apps see them; the endpoint has a destination too, which this source
//! never reads and whose messages it counts as refused.
//!
//! [`UmpVirtualSource`] owns the endpoint, disposed on drop.

use std::sync::{Arc, Weak};

use tracing::debug;
use tutti_coremidi_ffi::{UmpEndpoint, UmpSender, UmpSource};
use tutti_graph::Losses;
use tutti_platform::MainThreadMarker;
use tutti_rt::Counter;
use tutti_ump::{FunctionBlockId, Protocol, Ump};

use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{SenderBackend, SourceBackend, VirtualProtocol, VirtualSpec};

/// CoreMIDI's protocol for the spec's.
pub(super) fn protocol_of(protocol: VirtualProtocol) -> Protocol {
    match protocol {
        VirtualProtocol::Midi1 => Protocol::Midi1,
        VirtualProtocol::Midi2 => Protocol::Midi2,
    }
}

/// A CoreMIDI virtual source that speaks the UMP protocol it was created with.
///
/// It carries UMP words end to end, so JR Timestamps and other MIDI-2-only
/// messages survive to the wire. Owns its endpoint, which is disposed on drop.
/// With Function Blocks the endpoint is a main-thread object, so this type is
/// `!Send`; what sends from another thread is the [`SenderBackend`] it hands out.
#[must_use = "dropping it removes the virtual source"]
pub struct UmpVirtualSource {
    source: Source,
    name: String,
}

/// The endpoint a source sends on.
enum Source {
    /// A bare source: no Function Blocks. The senders hold it weakly.
    Plain(Arc<UmpSource>),
    /// A UMP Endpoint that declares them, and what its inert destination dropped.
    WithBlocks {
        endpoint: UmpEndpoint,
        losses: Counter<Losses>,
    },
}

impl core::fmt::Debug for UmpVirtualSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns an opaque CoreMIDI handle; report the endpoint name only.
        f.debug_struct("UmpVirtualSource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl UmpVirtualSource {
    /// Publishes the source `spec` describes: a bare source, or a UMP Endpoint
    /// with the spec's Function Blocks when it declares any. `main` is the proof
    /// the endpoint is made on the main thread.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::VirtualBlocksUnsupported`](crate::MidiHardwareError::VirtualBlocksUnsupported)
    /// when `spec` declares blocks and the OS is before macOS 15, and
    /// [`MidiHardwareError::Os`](crate::MidiHardwareError::Os) if CoreMIDI refuses
    /// the client or the endpoint, or cannot carry a block (a MIDI-CI version, a
    /// block number that is not its position).
    pub(crate) fn open(spec: &VirtualSpec, main: MainThreadMarker) -> Result<Self> {
        if spec.function_blocks().is_empty() {
            return Self::new(spec.name().as_str(), spec.protocol);
        }
        let name = spec.name().as_str();
        // The device watch's client starts before the endpoint (see `hub_first`).
        super::hub_first()?;
        let (mut writer, losses) = tutti_rt::counter::<Losses>();
        let endpoint = UmpEndpoint::new(
            main,
            name,
            protocol_of(spec.protocol),
            spec.device(),
            spec.instance(),
            &super::block_infos(spec),
            // The endpoint's destination is not this source's to read: what is
            // written to it is counted as refused.
            move |_, _| writer.add(Losses::refused(1)),
        )
        .map_err(super::open_error)?;
        debug!(name, protocol = ?spec.protocol, "Created virtual MIDI source with Function Blocks");
        Ok(Self {
            source: Source::WithBlocks { endpoint, losses },
            name: name.to_string(),
        })
    }

    /// Creates a virtual source named `name` carrying `protocol`.
    pub(crate) fn new(name: &str, protocol: VirtualProtocol) -> Result<Self> {
        // CoreMIDI sends notifications to the run loop of the process's first
        // client, so the device watch's client starts before the source's own
        // (see `hub_first`).
        let client = super::hub_first()?;
        let source = UmpSource::new(client, name, protocol_of(protocol))?;
        debug!(name, ?protocol, "Created virtual MIDI source");
        Ok(Self {
            source: Source::Plain(Arc::new(source)),
            name: name.to_string(),
        })
    }
}

/// A send on a bare source, which it does not keep alive.
struct PlainSender(Weak<UmpSource>);

impl SenderBackend for PlainSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the source is dropped, and
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

impl SourceBackend for UmpVirtualSource {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        match &self.source {
            Source::Plain(source) => Arc::new(PlainSender(Arc::downgrade(source))),
            Source::WithBlocks { endpoint, .. } => Arc::new(EndpointSender(endpoint.sender())),
        }
    }

    fn block_numbers(&self) -> Option<&[FunctionBlockId]> {
        match &self.source {
            Source::Plain(_) => None,
            Source::WithBlocks { endpoint, .. } => Some(endpoint.blocks()),
        }
    }

    fn losses(&self) -> Losses {
        match &self.source {
            Source::Plain(_) => Losses::NONE,
            Source::WithBlocks { losses, .. } => losses.get(),
        }
    }
}

#[cfg(test)]
mod tests;
