//! A build whose MIDI layer fails to open returns that error.

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
        _: tutti_midi_hardware::Started,
        _: EndpointId,
        _: InputProducerHandle,
    ) -> tutti_midi_hardware::Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
    fn open_output(
        &self,
        _: tutti_midi_hardware::Started,
        _: EndpointId,
    ) -> tutti_midi_hardware::Result<Box<dyn DeviceOut>> {
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

/// **A MIDI open that fails returns its error**: the build fails with the backend's
/// refusal, and no session is made.
#[test]
fn a_failed_midi_open_returns_its_error() {
    let built = failing().build();
    assert!(matches!(
        built,
        Err(Error::MidiHardware(MidiHardwareError::Unsupported(_)))
    ));
}
