use super::*;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_midi_device::{EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, persistent_endpoint};
use tutti_units::Frame;

fn render(s: &Session, blocks: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
}

impl Ports {
    /// Where the first output port's stamp stream stands: the open output's, or
    /// the parked one's while the device is away.
    fn stream_origin(&self) -> Frame {
        let port = &self.outputs[0];
        port.out
            .as_ref()
            .map(UmpOut::origin)
            .or_else(|| port.parked.as_ref().map(JrStream::origin))
            .expect("an output holds its stream open or parked")
    }
}

/// **The stamp stream is carried across a closed output**: an output that sent
/// the session's clock has a stream that has moved on; its device unplugged and
/// back, the port's stream stands where it stood and does not start again at
/// zero, so the stamps the device is sent stay monotonic.
#[test]
fn the_stamp_stream_is_carried_across_a_closed_output() {
    let (backend, fake) =
        FakeBackend::build_with(vec![], vec![persistent_endpoint(10, "Synth")], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let synth = s.midi().devices().remove(0);
    let _out = s.open_midi_output(synth).expect("opens");
    let _ = s.update();
    // The device receives JR Timestamps, so the output stamps and its stream moves.
    s.midi.state.ports.outputs[0]
        .out
        .as_mut()
        .and_then(UmpOut::device_mut)
        .expect("the device's output")
        .set_config(tutti_ump::StreamConfig {
            protocol: tutti_ump::Protocol::Midi2,
            rx_jr: true,
            tx_jr: false,
        });
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    s.play().expect("room");
    render(&s, 8);
    let _ = s.update();
    let before = s.midi.state.ports.stream_origin();
    assert!(before > Frame::ZERO, "the clock moved the stream on");

    fake.unplug(EndpointId::from_raw(10));
    let _ = s.update();
    assert_eq!(
        s.midi.state.ports.stream_origin(),
        before,
        "parked as it was"
    );
    fake.plug(persistent_endpoint(10, "Synth"), PortDirection::Output);
    let _ = s.update();
    assert_eq!(
        s.midi.state.ports.stream_origin(),
        before,
        "carried on by the reopened output, not started over"
    );
}

/// **A port whose reader died reads the fault it died of; one whose input was
/// not held, and so read none, reads `ReopenFailed`.**
#[test]
fn a_lost_reader_reads_its_fault_or_reopen_failed() {
    let fault = InputFault::WaitFailed(tutti_midi_hardware::Errno::new(5));
    assert_eq!(reader_loss(Some(fault)), PortCause::ReaderFailed(fault));
    assert_eq!(reader_loss(None), PortCause::ReopenFailed);
}
