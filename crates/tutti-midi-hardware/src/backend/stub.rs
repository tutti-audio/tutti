//! The no-backend backend.
//!
//! Used where no backend exists: a Linux built against alsa-lib < 1.2.10.
//!
//! # It reports "unsupported", not "no devices"
//!
//! Enumeration returns an empty list — there genuinely are none — but *opening*
//! returns [`MidiHardwareError::Unsupported`] rather than a not-found error. The distinction
//! matters to whoever renders it: "no MIDI devices" tells a Windows user to
//! check their cables, when the truth is that this build has no MIDI backend at
//! all. One is a hardware problem, the other is a build fact.
//!
//! # Deliberately trivial
//!
//! No dependencies, no `unsafe`, no state, every method a one-liner. This is the
//! one backend that cannot be compiled on the machine most of this crate is
//! developed on, so it is written to be verifiable by reading. **If it ever
//! needs logic, that is a signal to reconsider rather than to grow it.**

use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, NoWatch, Watch};
use crate::error::{MidiHardwareError, Result};
use crate::start::Started;
use crate::virtual_endpoint::{AtOnce, OpeningBackend, VirtualSpec};
use tutti_midi_device::EndpointId;
use tutti_platform::{Executor, MainThreadMarker};

/// Why every call here fails, in the error a caller will surface.
const REASON: &str = "no MIDI backend on this platform (alsa-lib < 1.2.10)";

/// A [`MidiEndpoints`] that has none.
#[derive(Debug, Default)]
pub(super) struct StubEndpoints;

impl StubEndpoints {
    /// The stub backend. Stateless — this is `Default` under another name, kept
    /// so every backend is constructed the same way.
    pub(super) fn new() -> Self {
        Self
    }
}

impl MidiEndpoints for StubEndpoints {
    fn watch(&self, _sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(NoWatch))
    }

    fn inputs(&self) -> Result<Vec<OsEndpoint>> {
        Ok(Vec::new())
    }

    fn outputs(&self) -> Result<Vec<OsEndpoint>> {
        Ok(Vec::new())
    }

    fn open_input(
        &self,
        _started: Started,
        _id: EndpointId,
        _producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported(REASON))
    }

    fn open_output(&self, _started: Started, _id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported(REASON))
    }
}

/// Why no virtual endpoint can be published here.
const VIRTUAL_REASON: &str = "no virtual MIDI endpoints on this platform (no MIDI backend)";

/// Refuses on the first poll: there is no virtual endpoint to publish.
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

#[cfg(test)]
mod tests;
