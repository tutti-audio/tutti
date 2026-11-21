//! The Linux platform: ALSA's UMP sequencer.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::start::{Start, Started};
use crate::virtual_endpoint::{Immediate, OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::alsa::AlsaEndpoints::new(executor))
}

/// The wait for the backend to be ready: ALSA has nothing to start.
pub(crate) fn start() -> Start {
    Start::ready()
}

/// Publishes the virtual endpoint `spec` describes, delivering into `producer`:
/// ready on the first poll.
pub(crate) fn open_virtual_endpoint(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    _started: Started,
    _main: MainThreadMarker,
) -> Result<Box<dyn OpeningBackend>> {
    let made = super::alsa::virtual_endpoint::endpoint(executor, spec, producer)
        .map(|(endpoint, protocol)| (Box::new(endpoint) as _, protocol));
    Ok(Box::new(Immediate::new(made)))
}

/// The platform's own error, as [`MidiHardwareError::Os`](crate::MidiHardwareError::Os) carries it.
pub(crate) type OsError = tutti_alsa_ffi::Error;
