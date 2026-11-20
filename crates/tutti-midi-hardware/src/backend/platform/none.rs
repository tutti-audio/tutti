//! A platform with no native backend: Linux with an alsa-lib older than the UMP
//! sequencer API. The stub is its backend.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::virtual_endpoint::{EndpointBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::MainThreadMarker;

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}

/// Refuses: the stub has no virtual endpoints.
pub(crate) fn open_virtual_endpoint(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    main: MainThreadMarker,
) -> Result<(Box<dyn EndpointBackend>, VirtualProtocol)> {
    super::stub::open_virtual_endpoint(spec, producer, main)
}

/// No platform call exists to fail, so [`MidiHardwareError::Os`] is never
/// made.
pub(crate) type OsError = std::convert::Infallible;
