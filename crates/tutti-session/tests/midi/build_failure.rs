//! A build that fails after the session's executor exists leaves nothing of it.

use tutti_midi_device::EndpointId;
use tutti_midi_hardware::{
    ChangeSink, DeviceOut, InputConnection, InputProducerHandle, MidiEndpoints, MidiHardwareError,
    OsEndpoint, Watch,
};
use tutti_session::{Error, Session};
use tutti_units::SampleRate;

/// A backend whose watch refuses to start, so the MIDI open fails.
pub(crate) struct Refuses;

impl MidiEndpoints for Refuses {
    fn watch(&self, _: ChangeSink) -> tutti_midi_hardware::Result<Box<dyn Watch>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
    fn inputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<OsEndpoint> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: InputProducerHandle,
    ) -> tutti_midi_hardware::Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
    fn open_output(&self, _: EndpointId) -> tutti_midi_hardware::Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
}

fn failing() -> tutti_session::SessionBuilder<tutti_session::Offline<f32>> {
    Session::builder()
        .midi_hardware_over(
            Box::new(Refuses),
            tutti_midi_hardware::MidiHardwareConfig::new(),
        )
        .sample_rate(SampleRate::SR_48K)
}

/// **A MIDI open that fails returns its error and drops every clone of the app's
/// executor**: the builder, the session's parts and the half-built MIDI layer
/// hold none afterwards.
#[test]
fn a_failed_midi_open_returns_its_error_and_drops_the_executor() {
    let executor = tutti_platform::Executor::new();
    let weak = executor.downgrade();
    let built = failing().executor(executor).build();
    assert!(matches!(
        built,
        Err(Error::MidiHardware(MidiHardwareError::Unsupported(_)))
    ));
    assert!(weak.upgrade().is_none(), "no clone of the executor is left");
}
