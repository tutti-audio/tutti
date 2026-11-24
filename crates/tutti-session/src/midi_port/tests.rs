use super::*;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_midi_device::{EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, persistent_endpoint};
use tutti_units::{Frame, SampleRate};

/// What `wait` resolved to on its first poll: a fake backend has nothing to start.
fn ready<T, E: std::fmt::Debug>(wait: impl Future<Output = Result<T, E>>) -> T {
    match std::pin::pin!(wait).poll(&mut Context::from_waker(std::task::Waker::noop())) {
        Poll::Ready(outcome) => outcome.expect("a fake backend starts at once"),
        Poll::Pending => panic!("a fake backend's start is ready on the first poll"),
    }
}

fn render(s: &mut Session, blocks: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
}

impl Ports {
    /// Where the first output port's stamp stream stands: the open output's, or
    /// the parked one's while the device is away.
    fn stream_origin(&self) -> Frame {
        match &self.outputs[0].link {
            OutLink::Open(open) => open.out.stream.origin(),
            OutLink::Away { stream, .. } => stream.origin(),
        }
    }

    /// The first output port's open device output with its stream.
    fn open_output(&mut self) -> &mut UmpOut<OpenOutput> {
        self.outputs[0].link.open_mut().expect("open")
    }

    /// What the first output port's open device has counted of its own.
    fn device_losses(&self) -> Option<Losses> {
        self.outputs[0].link.device().map(OpenOutput::losses)
    }

    /// What the first output port counted, cumulative.
    fn port_losses(&self) -> Losses {
        self.outputs[0].shared.losses.get()
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
    let opened = ready(s.open_midi_output(synth).expect("begins"));
    let _out = s.apply(opened).expect("opens");
    let _ = s.update();
    // The device receives JR Timestamps, so the output stamps and its stream moves.
    s.midi
        .state
        .ports
        .open_output()
        .destination_mut()
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
    render(&mut s, 8);
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

/// **An output opened with its device absent stamps from zero when the device
/// appears**: its first stamp is 0 ticks (M2-104-UM §7.2.3.1: the first message of
/// a stream has no earlier message to be spaced from), the next the 625 ticks of
/// 20 ms, as an output that was opened with its device.
#[test]
fn a_never_opened_output_stamps_from_zero() {
    let (backend, fake) =
        FakeBackend::build_with(vec![], vec![persistent_endpoint(10, "Synth")], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let synth = s.midi().devices().remove(0);
    fake.unplug(EndpointId::from_raw(10));
    let _ = s.update();
    let opened = ready(s.open_midi_output(synth).expect("begins"));
    let _out = s.apply(opened).expect("opens with no device");
    let _ = s.update();
    fake.plug(persistent_endpoint(10, "Synth"), PortDirection::Output);
    let _ = s.update();
    assert_eq!(s.midi.state.ports.stream_origin(), Frame::ZERO);
    let out = s.midi.state.ports.open_output();
    out.destination_mut().set_config(tutti_ump::StreamConfig {
        protocol: tutti_ump::Protocol::Midi2,
        rx_jr: true,
        tx_jr: false,
    });
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    out.send_frames(&[(Frame::new(0), note), (Frame::new(960), note)], None);
    let stamps: Vec<u32> = fake
        .take_sent(EndpointId::from_raw(10))
        .iter()
        .map(|ump| ump.0[0])
        .filter(|word| word >> 20 == 0x002)
        .collect();
    assert_eq!(stamps, vec![0x0020_0000, 0x0020_0271]);
}

/// **A never-opened output re-rated before its first open stamps as a fresh
/// stream at the new rate**: JR Timestamps count ticks of 1/31 250 s
/// (M2-104-UM §7.2.3.1), so at 31 250 Hz a gap of 1 000 frames is 1 000 ticks
/// (1 000 / 31 250 s x 31 250 = 1 000 = 0x3E8); at the opened rate of 48 000 Hz
/// the same gap would be 651 ticks.
#[test]
fn a_never_opened_output_re_rated_before_its_open_stamps_at_the_new_rate() {
    let (backend, fake) =
        FakeBackend::build_with(vec![], vec![persistent_endpoint(10, "Synth")], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    let _ = s.update();
    let synth = s.midi().devices().remove(0);
    fake.unplug(EndpointId::from_raw(10));
    let _ = s.update();
    let opened = ready(s.open_midi_output(synth).expect("begins"));
    let _out = s.apply(opened).expect("opens with no device");
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate::of(31_250.0)).unwrap();
    let _ = s.update();
    render(&mut s, 8);
    let _ = s.update();
    render(&mut s, 4);
    let _ = s.update();
    fake.plug(persistent_endpoint(10, "Synth"), PortDirection::Output);
    let _ = s.update();
    assert_eq!(s.midi.state.ports.stream_origin(), Frame::ZERO);
    let out = s.midi.state.ports.open_output();
    out.destination_mut().set_config(tutti_ump::StreamConfig {
        protocol: tutti_ump::Protocol::Midi2,
        rx_jr: true,
        tx_jr: false,
    });
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    out.send_frames(&[(Frame::new(0), note), (Frame::new(1_000), note)], None);
    let stamps: Vec<u32> = fake
        .take_sent(EndpointId::from_raw(10))
        .iter()
        .map(|ump| ump.0[0])
        .filter(|word| word >> 20 == 0x002)
        .collect();
    assert_eq!(stamps, vec![0x0020_0000, 0x0020_03E8]);
}

/// **A replugged device's losses count from zero, and the port's are the sum**:
/// a device that refuses everything is unplugged and plugged again; the port
/// holds what the first device refused, the new device's own count starts at
/// zero, and what it refuses after is added once to the port's.
#[test]
fn device_losses_count_from_zero_after_a_replug() {
    let (backend, fake) =
        FakeBackend::build_with(vec![], vec![persistent_endpoint(10, "Synth")], 0);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let synth = s.midi().devices().remove(0);
    let opened = ready(s.open_midi_output(synth).expect("begins"));
    let _out = s.apply(opened).expect("opens");
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    s.play().expect("room");
    render(&mut s, 8);
    let _ = s.update();
    let first = s.midi.state.ports.device_losses().expect("open");
    assert!(first.refused > 0, "the device refused what it was sent");
    assert_eq!(s.midi.state.ports.port_losses(), first);

    fake.unplug(EndpointId::from_raw(10));
    let _ = s.update();
    assert_eq!(s.midi.state.ports.port_losses(), first, "folded once");
    fake.plug(persistent_endpoint(10, "Synth"), PortDirection::Output);
    let _ = s.update();
    assert_eq!(
        s.midi.state.ports.device_losses(),
        Some(Losses::NONE),
        "the new device counts from zero"
    );
    assert_eq!(s.midi.state.ports.port_losses(), first, "nothing added");

    render(&mut s, 8);
    let _ = s.update();
    let second = s.midi.state.ports.device_losses().expect("open");
    assert!(second.refused > 0);
    assert_eq!(
        s.midi.state.ports.port_losses(),
        first + second,
        "the port's losses are the sum of its devices'"
    );
}

/// **A device's output stamps iff its configuration receives JR Timestamps**
/// (M2-104-UM §7.2.2.2), through the same `UmpOut`: two notes 20 ms apart go
/// out with a JR Timestamp of 625 ticks of 1/31 250 s (§7.2.3.1) between them
/// when it does, and bare when it does not.
#[test]
fn an_output_stamps_iff_its_configuration_receives_jr_timestamps() {
    let sent = |rx_jr: bool| {
        let (backend, fake) =
            FakeBackend::build_with(vec![], vec![persistent_endpoint(10, "Synth")], usize::MAX);
        let mut s = Session::builder()
            .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
            .build()
            .expect("builds");
        let _ = s.update();
        let synth = s.midi().devices().remove(0);
        let opened = ready(s.open_midi_output(synth).expect("begins"));
        let _out = s.apply(opened).expect("opens");
        let _ = s.update();
        let out = s.midi.state.ports.open_output();
        out.destination_mut().set_config(tutti_ump::StreamConfig {
            protocol: tutti_ump::Protocol::Midi2,
            rx_jr,
            tx_jr: false,
        });
        let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000);
        let _ = fake.take_sent(EndpointId::from_raw(10));
        out.send_frames(&[(Frame::new(0), note), (Frame::new(960), note)], None);
        let sent = fake.take_sent(EndpointId::from_raw(10));
        // The JR Timestamp messages: utility status 0x2, a 20-bit time.
        let stamps: Vec<u32> = sent
            .iter()
            .map(|ump| ump.0[0])
            .filter(|word| word >> 20 == 0x002)
            .collect();
        let notes = sent.iter().filter(|ump| **ump == note).count();
        (stamps, notes)
    };
    assert_eq!(
        sent(true),
        (vec![0x0020_0000, 0x0020_0271], 2),
        "a stamp before each note: 0 ticks, then 625 (20 ms)"
    );
    assert_eq!(sent(false), (vec![], 2), "the notes go out bare");
}

/// **A port whose reader died reads the fault it died of; one whose input was
/// not held, and so read none, reads `ReopenFailed`.**
#[test]
fn a_lost_reader_reads_its_fault_or_reopen_failed() {
    let fault = InputFault::WaitFailed(tutti_midi_hardware::Errno::new(5));
    assert_eq!(reader_loss(Some(fault)), PortCause::ReaderFailed(fault));
    assert_eq!(reader_loss(None), PortCause::ReopenFailed);
}

/// **An input node the host removed itself is not kept waiting**: the node
/// never runs its release block, and the port's retiring entry ends all the
/// same once the port's handles are dropped.
#[test]
fn a_host_removed_input_node_does_not_keep_its_retiring_entry() {
    let (backend, _fake) =
        FakeBackend::build_with(vec![persistent_endpoint(1, "Keys")], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    let opened = ready(s.open_midi_input(keys).expect("begins"));
    let port = s.apply(opened).expect("opens");
    let _ = s.update();
    s.graph().remove(port.node()).expect("the node is there");
    drop(port);
    let _ = s.update();
    let _ = s.update();
    assert!(s.midi.state.ports.retiring.is_empty());
}
