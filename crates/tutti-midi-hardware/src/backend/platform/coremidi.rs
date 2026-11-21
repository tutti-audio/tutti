//! The macOS platform: CoreMIDI.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::virtual_endpoint::{EndpointBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::coremidi::CoreMidiEndpoints::new())
}

/// Publishes the virtual endpoint `spec` describes, delivering into `producer`.
pub(crate) fn open_virtual_endpoint(
    _executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    main: MainThreadMarker,
) -> Result<(Box<dyn EndpointBackend>, VirtualProtocol)> {
    let endpoint = super::coremidi::UmpVirtualEndpoint::open(spec, producer, main)?;
    Ok((Box::new(endpoint), spec.protocol))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_coremidi_ffi::Error;
