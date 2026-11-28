//! The web platform: Web MIDI.

use crate::InputProducerHandle;
use crate::endpoints::MidiEndpoints;
use crate::error::{MidiHardwareError, Result};
use crate::start::Started;
use crate::virtual_endpoint::{AtOnce, OpeningBackend, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str = "Web MIDI has no virtual ports: the browser owns the port list";

/// Returns this platform's MIDI backend.
pub(crate) fn active(_executor: &Executor) -> Box<dyn MidiEndpoints> {
    Box::new(super::web::WebEndpoints)
}

/// Refuses on the first poll: a page cannot publish a port.
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

/// No platform call returns an error of its own type: a failed request is one of the
/// `WebMidi*` variants of [`MidiHardwareError`].
pub(crate) type OsError = std::convert::Infallible;
