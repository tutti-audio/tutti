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

/// A session with output ports on `Synth` (device 10) and `Pad` (device 11), their
/// handles, and the device ids.
fn two_outputs() -> (
    Session,
    tutti_midi_hardware::test_support::FakeCounters,
    [MidiDeviceId; 2],
    Vec<crate::MidiOutput>,
) {
    let (backend, fake) = FakeBackend::build_with(
        vec![],
        vec![
            persistent_endpoint(10, "Synth"),
            persistent_endpoint(11, "Pad"),
        ],
        usize::MAX,
    );
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .build()
        .expect("builds");
    let _ = s.update();
    let devices = s.midi().devices();
    let (mut ids, mut handles) = (Vec::new(), Vec::new());
    for name in ["Synth", "Pad"] {
        let info = devices
            .iter()
            .find(|d| d.name == name)
            .expect("listed")
            .clone();
        ids.push(info.id.clone());
        let opened = ready(s.open_midi_output(info).expect("begins"));
        handles.push(s.apply(opened).expect("opens"));
    }
    let _ = s.update();
    let ids: [MidiDeviceId; 2] = ids.try_into().expect("two");
    (s, fake, ids, handles)
}

fn inquiry(control: tutti_midi_ci::DataControl) -> ReportInquiry {
    tutti_midi_hardware::test_support::report_inquiry(
        MidiGroup::of(0),
        tutti_ump::GroupSpan::of(1),
        tutti_midi_ci::Muid::of(0x0102_0304),
        tutti_midi_ci::Muid::of(0x0506_0708),
        control,
        ReportScope::Group {
            controllers: tutti_midi_ci::ChannelControllers::all(),
            notes: tutti_midi_ci::NoteData::all(),
        },
    )
}

/// What a sent packet is, for an order: the first packet of a MIDI-CI Reply (`reply`) or
/// End (`end`), another SysEx7 packet (`""`), and a message of the report (`data`).
fn kind(ump: &tutti_ump::Ump) -> &'static str {
    match ump.decode() {
        Ok(tutti_ump::Message::Sysex7(_, packet))
            if packet.status() == tutti_ump::SysexStatus::Start =>
        {
            match packet.data()[3] {
                0x43 => "reply",
                0x44 => "end",
                _ => "sysex",
            }
        }
        Ok(tutti_ump::Message::Sysex7(..)) => "",
        _ => "data",
    }
}

/// The report's shape: each message kind, with the SysEx7 continuations left out.
fn shape(sent: &[tutti_ump::Ump]) -> Vec<&'static str> {
    sent.iter().map(kind).filter(|k| !k.is_empty()).collect()
}

/// M2-101 section 9.6 to 9.8: the report of an output is its Reply, the messages of what the
/// port said (a Note On and the Note Off that matches it) and the End, in that order, on the
/// port's own device, and a port of another device sends nothing.
/// Spec: m2-101/9.6#1, m2-101/9.8#1
#[test]
fn an_output_answers_a_report_as_a_reply_its_messages_and_an_end_in_order() {
    let (mut s, fake, _ids, _handles) = two_outputs();
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000);
    s.midi.state.ports.outputs[0].record.observe(note);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let _ = fake.take_sent(EndpointId::from_raw(11));
    let ask = inquiry(tutti_midi_ci::DataControl::Full);
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), None);
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert_eq!(shape(&sent), ["reply", "data", "data", "end"]);
    assert!(fake.take_sent(EndpointId::from_raw(11)).is_empty());
}

/// A report asked of an output whose device is away is handed back unanswered and sends
/// nothing, so the caller answers it as a device that declares nothing.
#[test]
fn an_output_whose_device_is_away_hands_the_report_back() {
    let (mut s, fake, _ids, _handles) = two_outputs();
    fake.unplug(EndpointId::from_raw(10));
    let _ = s.update();
    let ask = inquiry(tutti_midi_ci::DataControl::Full);
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), Some(ask));
    assert!(fake.take_sent(EndpointId::from_raw(10)).is_empty());
}

/// A report inquiry on a device is answered by the held port of that device, found by its
/// device id and not by its place: the second port answers its own device's.
#[test]
fn a_report_is_answered_by_the_held_port_of_its_device() {
    let (mut s, fake, [_synth, pad], _handles) = two_outputs();
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let _ = fake.take_sent(EndpointId::from_raw(11));
    let ask = inquiry(tutti_midi_ci::DataControl::Capabilities);
    let mut io = s.midi.hardware.locked().expect("hardware");
    s.midi.state.ports.answer_report(&mut io, &pad, &ask);
    drop(io);
    assert!(fake.take_sent(EndpointId::from_raw(10)).is_empty());
    assert_eq!(
        shape(&fake.take_sent(EndpointId::from_raw(11))),
        ["reply", "end"]
    );
}

/// M2-101 section 9.5 and 9.7.1: a report of the Function Block covers the Groups of the
/// block, not the Group its MIDI-CI Device speaks on alone, Groups ascending.
/// Spec: m2-101/9.5#5, m2-101/9.6#6
#[test]
fn a_report_of_the_function_block_covers_every_group_of_the_block() {
    let (mut s, fake, _ids, _handles) = two_outputs();
    for group in [1, 0] {
        let at = tutti_ump::Address::new(MidiGroup::of(group), tutti_ump::MidiChannel::FIRST);
        let voice =
            tutti_ump::Voice1::control_change(tutti_ump::CCNumber::VOLUME, tutti_ump::U7::of(90));
        let ump = tutti_ump::Ump::from(tutti_ump::Message::midi1(at, voice));
        s.midi.state.ports.outputs[0].record.observe(ump);
    }
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let ask = tutti_midi_hardware::test_support::report_inquiry(
        MidiGroup::of(0),
        tutti_ump::GroupSpan::of(2),
        tutti_midi_ci::Muid::of(1),
        tutti_midi_ci::Muid::of(2),
        tutti_midi_ci::DataControl::Full,
        ReportScope::FunctionBlock {
            system: tutti_midi_ci::SystemMessages::empty(),
            controllers: tutti_midi_ci::ChannelControllers::CONTROL_CHANGE,
            notes: tutti_midi_ci::NoteData::empty(),
        },
    );
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), None);
    let sent = fake.take_sent(EndpointId::from_raw(10));
    let groups: Vec<u8> = sent
        .iter()
        .filter(|ump| kind(ump) == "data")
        .filter_map(|ump| ump.group().map(MidiGroup::get))
        .collect();
    assert_eq!(groups, [0, 1]);
    // One Reply and one End for the whole block, not for each Group (§9.6).
    assert_eq!(shape(&sent), ["reply", "data", "data", "end"]);
}

fn use_protocol(s: &mut Session, protocol: tutti_ump::Protocol, rx_jr: bool) {
    s.midi.state.ports.outputs[0]
        .link
        .open_mut()
        .expect("open")
        .destination_mut()
        .set_config(tutti_ump::StreamConfig {
            protocol,
            rx_jr,
            tx_jr: false,
        });
}

/// What the device was sent of a report of a Note On sent in MIDI 1.0 with velocity 80.
fn reported_note(protocol: tutti_ump::Protocol) -> Vec<tutti_ump::Ump> {
    let (mut s, fake, _ids, _handles) = two_outputs();
    use_protocol(&mut s, protocol, false);
    let at = tutti_ump::Address::first(tutti_ump::MidiChannel::FIRST);
    let on = tutti_ump::Voice1::note_on(tutti_ump::NoteNumber::of(60), tutti_ump::U7::of(80));
    let ump = tutti_ump::Ump::from(tutti_ump::Message::midi1(at, on));
    s.midi.state.ports.outputs[0].record.observe(ump);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let ask = inquiry(tutti_midi_ci::DataControl::Full);
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), None);
    fake.take_sent(EndpointId::from_raw(10))
        .into_iter()
        .filter(|u| kind(u) == "data")
        .collect()
}

/// M2-101 section 9.7.2: under the MIDI 1.0 Protocol each active note is a MIDI 1.0 Note On
/// with its original velocity, and the matching Note Off is MIDI 1.0 too.
/// Spec: m2-101/9.7.2#14
#[test]
fn a_midi_1_device_is_reported_a_midi_1_note_on_with_its_velocity_and_a_note_off() {
    let sent = reported_note(tutti_ump::Protocol::Midi1);
    let [on, off] = sent[..] else {
        panic!("a Note On and a Note Off: {sent:?}");
    };
    assert!(matches!(
        on.decode(),
        Ok(tutti_ump::Message::Midi1(_, tutti_ump::Voice1::NoteOn { note, velocity }))
            if note.get() == 60 && velocity.get() == 80
    ));
    assert!(matches!(
        off.note_key_event(),
        Some(tutti_ump::NoteKeyEvent::Off(k)) if k.note().get() == 60
    ));
    assert!(matches!(off.decode(), Ok(tutti_ump::Message::Midi1(..))));
}

/// Under the MIDI 2.0 Protocol the same note is a MIDI 2.0 Note On and Note Off.
/// Spec: m2-101/9.7.2#15
#[test]
fn a_midi_2_device_is_reported_a_midi_2_note_on_and_note_off() {
    let sent = reported_note(tutti_ump::Protocol::Midi2);
    let [on, off] = sent[..] else {
        panic!("a Note On and a Note Off: {sent:?}");
    };
    assert!(matches!(
        on.decode(),
        Ok(tutti_ump::Message::Midi2(_, tutti_ump::Voice2::NoteOn { note, .. })) if note.get() == 60
    ));
    assert!(matches!(
        off.decode(),
        Ok(tutti_ump::Message::Midi2(
            _,
            tutti_ump::Voice2::NoteOff { .. }
        ))
    ));
}

/// The report is one batch: the device's sink is handed one `queue_timed`, so nothing a
/// drain sends lands between its Reply and its End (M2-101 section 9.8.2).
#[test]
fn a_report_is_one_send_to_the_device() {
    let (mut s, fake, _ids, _handles) = two_outputs();
    use_protocol(&mut s, tutti_ump::Protocol::Midi2, true);
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0x8000);
    s.midi.state.ports.outputs[0].record.observe(note);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let ask = inquiry(tutti_midi_ci::DataControl::Full);
    let before = fake.batches.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), None);
    let batches = fake.batches.load(std::sync::atomic::Ordering::SeqCst) - before;
    assert_eq!(batches, 1);
    let sent = fake.take_sent(EndpointId::from_raw(10));
    let report: Vec<_> = sent.iter().filter(|u| u.0[0] >> 28 != 0).copied().collect();
    assert_eq!(shape(&report), ["reply", "data", "data", "end"]);
}

/// A report asked of no held port, or of one whose device is away, is handed back to
/// the caller's one decline; one a held port answers is not.
#[test]
fn a_report_with_no_port_or_an_away_port_is_returned_for_the_decline() {
    let (mut s, fake, [synth, pad], _handles) = two_outputs();
    let ask = inquiry(tutti_midi_ci::DataControl::Full);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    assert_eq!(s.midi.state.ports.route_report(&synth, &ask), None);
    assert!(!fake.take_sent(EndpointId::from_raw(10)).is_empty());
    fake.unplug(EndpointId::from_raw(11));
    let _ = s.update();
    assert_eq!(s.midi.state.ports.route_report(&pad, &ask), Some(ask));
    let other = MidiDeviceId::clone(&synth);
    drop(_handles);
    let _ = s.update();
    assert_eq!(s.midi.state.ports.route_report(&other, &ask), Some(ask));
}

/// M2-101 section 9.5: a report of one Channel names that Channel alone.
#[test]
fn a_channel_scope_report_has_that_channel_only() {
    let (mut s, fake, _ids, _handles) = two_outputs();
    for channel in [2, 5] {
        let at = tutti_ump::Address::first(tutti_ump::MidiChannel::of(channel));
        let voice =
            tutti_ump::Voice1::control_change(tutti_ump::CCNumber::VOLUME, tutti_ump::U7::of(90));
        s.midi.state.ports.outputs[0]
            .record
            .observe(tutti_ump::Ump::from(tutti_ump::Message::midi1(at, voice)));
    }
    let _ = fake.take_sent(EndpointId::from_raw(10));
    let ask = tutti_midi_hardware::test_support::report_inquiry(
        MidiGroup::of(0),
        tutti_ump::GroupSpan::of(1),
        tutti_midi_ci::Muid::of(1),
        tutti_midi_ci::Muid::of(2),
        tutti_midi_ci::DataControl::Full,
        ReportScope::Channel {
            channel: tutti_ump::MidiChannel::of(5),
            controllers: tutti_midi_ci::ChannelControllers::CONTROL_CHANGE,
            notes: tutti_midi_ci::NoteData::empty(),
        },
    );
    assert_eq!(s.midi.state.ports.outputs[0].answer_report(&ask), None);
    let sent = fake.take_sent(EndpointId::from_raw(10));
    let channels: Vec<u8> = sent
        .iter()
        .filter(|u| kind(u) == "data")
        .filter_map(|u| u.channel().map(tutti_ump::MidiChannel::get))
        .collect();
    assert_eq!(channels, [5]);
}
