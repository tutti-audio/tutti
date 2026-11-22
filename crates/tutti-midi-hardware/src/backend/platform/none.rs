//! A platform with no native backend: Linux with an alsa-lib older than the UMP
//! sequencer API. The stub is its backend.

use crate::endpoints::MidiEndpoints;
use tutti_platform::Executor;

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}

pub(crate) use super::stub::open_virtual_endpoint;

/// No platform call exists to fail, so [`MidiHardwareError::Os`] is never
/// made.
pub(crate) type OsError = std::convert::Infallible;
