//! A virtual MIDI source on CoreMIDI, in the UMP protocol the spec asks for.
//!
//! A UMP endpoint is created with `MIDISourceCreateWithProtocol` and sends UMP words,
//! so a MIDI-2-only message (per-note controllers, JR Timestamps) reaches the wire;
//! the raw CoreMIDI calls for it are `tutti-coremidi-ffi`'s (`UmpSource`), and this
//! module names the endpoint and puts its errors in this crate's.
//!
//! [`UmpVirtualSource`] owns the endpoint, disposed on drop.

use tracing::debug;
use tutti_coremidi_ffi::UmpSource;
use tutti_ump::{Protocol, Ump};

use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};

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
#[must_use = "dropping it removes the virtual source"]
pub struct UmpVirtualSource {
    source: UmpSource,
    name: String,
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
    /// Publishes the source `spec` describes. The endpoint carries a name and a
    /// protocol: CoreMIDI's endpoint API takes no Function Blocks.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::VirtualBlocksUnsupported`] when `spec` declares blocks,
    /// and [`MidiHardwareError::Os`] if CoreMIDI refuses the client or the endpoint.
    pub(crate) fn open(spec: &VirtualSpec) -> Result<Self> {
        if !spec.function_blocks().is_empty() {
            return Err(MidiHardwareError::VirtualBlocksUnsupported);
        }
        Self::new(spec.name().as_str(), spec.protocol)
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
            source,
            name: name.to_string(),
        })
    }
}

impl SourceBackend for UmpVirtualSource {
    /// Publishes one UMP message to the endpoint's subscribers.
    ///
    /// # Errors
    ///
    /// [`MidiHardwareError::Os`] when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    fn send(&self, ump: &Ump) -> Result<()> {
        Ok(self.source.send(ump)?)
    }
}

#[cfg(test)]
mod tests;
