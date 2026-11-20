//! A platform with no native backend: Linux with an alsa-lib older than the UMP
//! sequencer API. The stub is its backend.

use crate::InputProducerHandle;
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::Result;
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::Executor;

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}

/// Refuses: the stub has no virtual endpoints.
pub(crate) fn open_virtual_source(
    spec: &VirtualSpec,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    super::stub::open_virtual_source(spec)
}

/// Refuses: the stub has no virtual endpoints.
pub(crate) fn open_virtual_destination(
    _executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(Box<dyn InputConnection>, VirtualProtocol)> {
    super::stub::open_virtual_destination(spec, producer)
}

/// No platform call exists to fail, so [`MidiHardwareError::Os`] is never
/// made.
pub(crate) type OsError = std::convert::Infallible;
