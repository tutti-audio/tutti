//! The macOS platform: CoreMIDI.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::virtual_endpoint::{DestinationBackend, SourceBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::coremidi::CoreMidiEndpoints::new())
}

/// Publishes the virtual source `spec` describes.
pub(crate) fn open_virtual_source(
    spec: &VirtualSpec,
    main: MainThreadMarker,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    let source = super::coremidi::UmpVirtualSource::open(spec, main)?;
    Ok((Box::new(source), spec.protocol))
}

/// Publishes the virtual destination `spec` describes, delivering into `producer`.
pub(crate) fn open_virtual_destination(
    _executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    main: MainThreadMarker,
) -> Result<(Box<dyn DestinationBackend>, VirtualProtocol)> {
    let destination = super::coremidi::UmpVirtualDestination::open(spec, producer, main)?;
    Ok((Box::new(destination), spec.protocol))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_coremidi_ffi::Error;
