//! The macOS platform: CoreMIDI.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::start::Started;
use crate::virtual_endpoint::{OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::coremidi::CoreMidiEndpoints::new())
}

/// Starts publishing the virtual endpoint `spec` describes, delivering into
/// `producer`. `started` is the proof the hub's client is made.
pub(crate) fn open_virtual_endpoint(
    _executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    started: Started,
    main: MainThreadMarker,
) -> Result<Box<dyn OpeningBackend>> {
    let opening = super::coremidi::UmpVirtualEndpoint::open(spec, producer, started, main)?;
    Ok(Box::new(opening))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_coremidi_ffi::Error;
