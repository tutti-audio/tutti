//! A platform with no native backend: Linux with an alsa-lib older than the UMP
//! sequencer API. The stub is its backend.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::virtual_endpoint::{DestinationBackend, SourceBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}

/// Refuses: the stub has no virtual endpoints.
pub(crate) fn open_virtual_source(
    spec: &VirtualSpec,
    main: MainThreadMarker,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    super::stub::open_virtual_source(spec, main)
}

/// Refuses: the stub has no virtual endpoints.
pub(crate) fn open_virtual_destination(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    main: MainThreadMarker,
) -> Result<(Box<dyn DestinationBackend>, VirtualProtocol)> {
    super::stub::open_virtual_destination(executor, spec, producer, main)
}

/// No platform call exists to fail, so [`MidiHardwareError::Os`] is never
/// made.
pub(crate) type OsError = std::convert::Infallible;
