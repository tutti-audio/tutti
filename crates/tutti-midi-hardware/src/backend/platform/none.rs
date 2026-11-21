//! A platform with no native backend: Linux with an alsa-lib older than the UMP
//! sequencer API. The stub is its backend.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::start::Started;
use crate::virtual_endpoint::{OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}

/// Refuses on the first poll: the stub has no virtual endpoints.
pub(crate) fn open_virtual_endpoint(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    started: Started,
    main: MainThreadMarker,
) -> Result<Box<dyn OpeningBackend>> {
    super::stub::open_virtual_endpoint(executor, spec, producer, started, main)
}

/// No platform call exists to fail, so [`MidiHardwareError::Os`] is never
/// made.
pub(crate) type OsError = std::convert::Infallible;
