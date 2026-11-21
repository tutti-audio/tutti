//! The Linux platform: ALSA's UMP sequencer.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::Result;
use crate::virtual_endpoint::{EndpointBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Returns this platform's MIDI backend.
pub(crate) fn active(executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::alsa::AlsaEndpoints::new(executor))
}

/// Publishes the virtual endpoint `spec` describes, delivering into `producer`.
pub(crate) fn open_virtual_endpoint(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
    _main: MainThreadMarker,
) -> Result<(Box<dyn EndpointBackend>, VirtualProtocol)> {
    let (endpoint, protocol) = super::alsa::virtual_endpoint::endpoint(executor, spec, producer)?;
    Ok((Box::new(endpoint), protocol))
}

/// The platform's own error, as [`MidiHardwareError::Os`](crate::MidiHardwareError::Os) carries it.
pub(crate) type OsError = tutti_alsa_ffi::Error;
