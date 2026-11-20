//! The Windows platform: Windows Multimedia.

use crate::InputProducerHandle;
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{SourceBackend, VirtualProtocol, VirtualSpec};
use tutti_platform::Executor;

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str = "Windows Multimedia has no virtual MIDI ports";

/// Returns this platform's MIDI backend.
pub(crate) fn active(executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::winmm::WinMmEndpoints::new(executor))
}

/// Refuses: winmm has no virtual ports.
pub(crate) fn open_virtual_source(
    _spec: &VirtualSpec,
) -> Result<(Box<dyn SourceBackend>, VirtualProtocol)> {
    Err(MidiHardwareError::Unsupported(VIRTUAL_REASON))
}

/// Refuses: winmm has no virtual ports.
pub(crate) fn open_virtual_destination(
    _executor: &Executor,
    _spec: &VirtualSpec,
    _producer: InputProducerHandle,
) -> Result<(Box<dyn InputConnection>, VirtualProtocol)> {
    Err(MidiHardwareError::Unsupported(VIRTUAL_REASON))
}

/// The platform's own error, as [`MidiHardwareError::Os`] carries it.
pub(crate) type OsError = tutti_winmm_ffi::Error;
