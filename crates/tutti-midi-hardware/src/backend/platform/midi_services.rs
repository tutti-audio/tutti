//! The Windows platform: Windows MIDI Services.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::start::Started;
use crate::virtual_endpoint::{AtOnce, OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::midi_services::MidiServicesEndpoints::new())
}

/// Publishes the virtual endpoint `spec` describes, delivering into `producer`: ready on
/// the first poll.
pub(crate) fn open_virtual_endpoint(
    _executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    _started: Started,
    _main: MainThreadMarker,
) -> Result<Box<dyn OpeningBackend>> {
    let made = super::midi_services::virtual_endpoint::endpoint(spec, producer)
        .map(|(endpoint, protocol)| (Box::new(endpoint) as _, protocol));
    Ok(Box::new(AtOnce::new(made)))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_winmidi2_ffi::Error;
