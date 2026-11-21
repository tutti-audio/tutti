//! The Windows platform: Windows Multimedia.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{EndpointBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str = "Windows Multimedia has no virtual MIDI ports";

/// Returns this platform's MIDI backend.
pub(crate) fn active(executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::winmm::WinMmEndpoints::new(executor))
}

/// Refuses: winmm has no virtual ports.
pub(crate) fn open_virtual_endpoint(
    _executor: &Executor,
    _spec: &VirtualSpec,
    _producer: InputProducerHandle,
    _main: MainThreadMarker,
) -> Result<(Box<dyn EndpointBackend>, VirtualProtocol)> {
    Err(MidiHardwareError::Unsupported(VIRTUAL_REASON))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_winmm_ffi::Error;
