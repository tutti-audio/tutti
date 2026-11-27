//! The Windows platform: Windows MIDI Services.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::{MidiHardwareError, Result};
use crate::start::Started;
use crate::virtual_endpoint::{AtOnce, OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str =
    "Windows MIDI Services publishes no virtual endpoints through this backend";

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::midi_services::MidiServicesEndpoints::new())
}

/// Refuses on the first poll: no virtual endpoint is published here yet.
pub(crate) fn open_virtual_endpoint(
    _executor: &Executor,
    _spec: &VirtualSpec,
    _producer: InputProducerHandle,
    _started: Started,
    _main: MainThreadMarker,
) -> Result<Box<dyn OpeningBackend>> {
    Ok(Box::new(AtOnce::new(Err(MidiHardwareError::Unsupported(
        VIRTUAL_REASON,
    )))))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_winmidi2_ffi::Error;
