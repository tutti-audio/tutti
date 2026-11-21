//! MIDI ports: `open_midi_input` and `open_midi_output` open a saved device,
//! and the handle reads whether it is there. A fake OS plugs and unplugs the
//! devices.

use std::collections::BTreeSet;
use tutti_platform::Instant;

use crate::common::{MF, Seen, blocks, note, play, tap};
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_midi_device::{Capabilities, DeviceInfo, Endpoint, EndpointId, PortDirection, Supports};
use tutti_midi_hardware::InputFault;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint, persistent_endpoint};
use tutti_midi_runtime::MidiClipNode;
use tutti_session::{
    EventInPort, MidiInput, MidiOutput, MidiPortError, MidiRouteRule, Peer, PortCause, Presence,
    Resource, ResourceFact, Session, SessionEvent, Stamp,
};
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteKeyEvent, NoteNumber, U7, Voice2,
};
use tutti_units::{Beat, BeatDuration, SampleRate};

const RATE: f64 = 48_000.0;

/// A session over a fake OS with no devices, and the handle that plugs some.
fn session() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    (s, fake)
}

/// Plugs a device the OS tells for certain across a replug, as CoreMIDI does:
/// what a returning device must be to be opened again unasked.
fn plug(fake: &FakeCounters, raw: u32, name: &str, direction: PortDirection) {
    fake.plug(persistent_endpoint(raw, name), direction);
}

fn unplug(fake: &FakeCounters, raw: u32) {
    fake.unplug(EndpointId::from_raw(u64::from(raw)));
}

/// The listing of the device `name`, as the session read it.
fn device(s: &mut Session, name: &str) -> DeviceInfo {
    s.midi()
        .devices()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

/// Opens the input port of `info`, and updates: the port's nodes are committed.
fn input(s: &mut Session, info: &DeviceInfo) -> MidiInput {
    let port = s.open_midi_input(info.clone()).expect("opens");
    let _ = s.update();
    port
}

/// Opens the output port of `info`, and updates.
fn output(s: &mut Session, info: &DeviceInfo) -> MidiOutput {
    let port = s.open_midi_output(info.clone()).run().expect("opens");
    let _ = s.update();
    port
}

/// What an update said of the resource `id`.
fn changed(events: &[SessionEvent], id: tutti_session::ResourceId) -> Vec<ResourceFact> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ResourceChanged { id: of, fact } if *of == id => Some(*fact),
            _ => None,
        })
        .collect()
}

fn roll(s: &Session, frames: usize) {
    play(s);
    blocks(s, frames.div_ceil(512));
}

fn key(channel: u8, n: u8) -> tutti_ump::Ump {
    midi::note_on(MidiChannel::of(channel), n, MF)
}

fn heard(seen: &Seen, wanted: fn(NoteKeyEvent) -> Option<u8>) -> Vec<u8> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|&(_, e)| e.note_key_event().and_then(wanted))
        .collect()
}

fn note_ons(seen: &Seen) -> Vec<u8> {
    heard(seen, |e| match e {
        NoteKeyEvent::On(k) => Some(k.note().get()),
        _ => None,
    })
}

fn note_offs(seen: &Seen) -> Vec<u8> {
    heard(seen, |e| match e {
        NoteKeyEvent::Off(k) => Some(k.note().get()),
        _ => None,
    })
}

/// **A saved device that is absent opens, and reads `Lost(Unplugged)`**: a port
/// needs no device to exist.
#[test]
fn an_absent_device_opens_lost_unplugged() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    unplug(&fake, 1);
    let _ = s.update();

    let port = input(&mut s, &keys);
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert_eq!(port.info().device(), &keys, "it reads the saved device");
}

/// **A device that is unplugged and plugged again goes `Lost` and `Live` behind
/// the same handle**: each change is one `ResourceChanged` of presence, and the
/// same input node serves the port.
#[test]
fn a_replug_goes_live_on_the_same_handle_and_node() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    let node = port.node();
    assert_eq!(port.presence(), Presence::Live);

    unplug(&fake, 1);
    let events = s.update();
    assert_eq!(changed(&events, port.id()), [ResourceFact::Presence]);
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert!(
        fake.connected_inputs().is_empty(),
        "the device's connection is closed"
    );

    plug(&fake, 1, "Keys", PortDirection::Input);
    let events = s.update();
    assert!(
        changed(&events, port.id()).contains(&ResourceFact::Presence),
        "{events:?}"
    );
    assert_eq!(
        port.presence(),
        Presence::Live,
        "the handle read before follows"
    );
    assert_eq!(port.node(), node, "wired to the same input node");
    assert!(s.graph_ref().contains(node));
    assert!(!fake.connected_inputs().is_empty());
}

/// **A returning device known only by its name is opened when it is the only
/// one**: an ALSA identity cannot tell two units of a model apart, so a replug
/// binds what a load would bind, the only present device of the saved name.
#[test]
fn a_returning_device_known_only_by_name_is_opened_when_it_is_the_only_one() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    assert_eq!(
        port.presence(),
        Presence::Live,
        "the only one of its name is bound at open"
    );

    unplug(&fake, 1);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
}

/// **Two devices of the saved name returning stay Lost**: either could be the
/// saved one, which is the open's ambiguity too, so nothing is bound unasked.
#[test]
fn two_returning_devices_of_the_saved_name_leave_the_port_lost() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    unplug(&fake, 1);
    let _ = s.update();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(2, "Keys"), PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert!(fake.connected_inputs().is_empty());
}

/// **Another device's arrival does not open a lost port**, and the saved
/// device's own does.
#[test]
fn another_devices_arrival_does_not_restore_a_lost_port() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    unplug(&fake, 1);
    let _ = s.update();
    plug(&fake, 2, "Pads", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert!(fake.connected_inputs().is_empty());
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
}

/// **A port whose device is held by another port fails its open `InUse`**: the
/// second port on one device cannot open it, and names it.
#[test]
fn a_second_port_on_one_device_fails_in_use() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let first = input(&mut s, &keys);
    let error = s
        .open_midi_input(keys.clone())
        .expect_err("the device is held");
    assert_eq!(first.presence(), Presence::Live);
    assert!(
        matches!(&error, MidiPortError::InUse(id) if *id == keys.id),
        "{error:?}"
    );
    assert_eq!(
        fake.connected_inputs().len(),
        1,
        "the failed open left no port"
    );
}

/// **An open the OS refuses reads `ReopenFailed`, and is tried again when the
/// device next appears**: an output and an input each.
#[test]
fn a_refused_reopen_reads_reopen_failed_and_is_retried_when_the_device_returns() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let (keys, synth) = (device(&mut s, "Keys"), device(&mut s, "Synth"));
    let (input, output) = (input(&mut s, &keys), output(&mut s, &synth));
    unplug(&fake, 1);
    unplug(&fake, 10);
    let _ = s.update();

    fake.refuse_input_opens(true);
    fake.refuse_output_opens(true);
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(input.presence(), Presence::Lost(PortCause::ReopenFailed));
    assert_eq!(output.presence(), Presence::Lost(PortCause::ReopenFailed));

    // The OS lets go, and the devices appear in the list again.
    fake.refuse_input_opens(false);
    fake.refuse_output_opens(false);
    unplug(&fake, 1);
    unplug(&fake, 10);
    let _ = s.update();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(input.presence(), Presence::Live);
    assert_eq!(output.presence(), Presence::Live);
}

/// **An input whose reader dies while its device stays listed reads
/// `ReaderFailed` when it cannot be reopened, and is retried when the device
/// next appears.** When the reopen works, the port stays live.
#[test]
fn a_dead_reader_is_reopened_or_reads_reader_failed() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);

    fake.kill_input_reader(EndpointId::from_raw(1), InputFault::PortBroken);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live, "reopened at once");
    assert!(!fake.connected_inputs().is_empty());

    fake.refuse_input_opens(true);
    fake.kill_input_reader(EndpointId::from_raw(1), InputFault::PortBroken);
    let events = s.update();
    assert_eq!(
        port.presence(),
        Presence::Lost(PortCause::ReaderFailed(InputFault::PortBroken)),
        "the cause the backend's reader died of"
    );
    assert_eq!(changed(&events, port.id()), [ResourceFact::Presence]);

    fake.refuse_input_opens(false);
    unplug(&fake, 1);
    let _ = s.update();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
}

/// **An unrouted port is processed, settles and leaves once dropped**: no synth is routed to
/// it, nothing hears it, and its node is still taken out once its release has
/// gone, so a port that comes and goes leaves nothing in the graph.
#[test]
fn an_unrouted_port_that_is_removed_leaves_the_graph() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let before = s.graph_ref().testing().sent_plan().map(|p| p.order().len());
    let mut nodes = Vec::new();
    for _ in 0..3 {
        let port = input(&mut s, &keys);
        let _ = s.update();
        blocks(&s, 1);
        nodes.push(port.node());
        drop(port);
        let _ = s.update();
        assert!(s.graph_ref().contains(*nodes.last().unwrap()));
        blocks(&s, 2);
        let _ = s.update();
    }
    for node in nodes {
        assert!(!s.graph_ref().contains(node), "{node:?} left");
    }
    let after = s.graph_ref().testing().sent_plan().map(|p| p.order().len());
    assert_eq!(before, after, "nothing was left in the plan");
}

/// **An ambiguous saved device fails the open with its candidates, and opens
/// once one leaves**: two present devices of one name could be the saved one;
/// once one leaves, the next open binds the other.
#[test]
fn an_ambiguous_match_fails_with_candidates_and_opens_once_one_leaves() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(2, "Keys"), PortDirection::Input);
    let _ = s.update();
    let saved = s.midi().devices().remove(0);

    let error = s
        .open_midi_input(saved.clone())
        .expect_err("two devices could be the saved one");
    let MidiPortError::Ambiguous(candidates) = &error else {
        panic!("ambiguous, got {error:?}");
    };
    assert_eq!(candidates.len(), 2, "both devices are candidates");
    assert!(
        fake.connected_inputs().is_empty(),
        "the failed open left no port"
    );

    unplug(&fake, 2);
    let _ = s.update();
    let port = s.open_midi_input(saved).expect("opens once one left");
    assert_eq!(port.presence(), Presence::Live);
}

/// **A session without the OS MIDI layer fails the open with a named error.**
#[test]
fn a_session_without_the_hardware_layer_fails_the_open_by_name() {
    let (mut with, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = with.update();
    let keys = device(&mut with, "Keys");

    let mut s = Session::builder().build().expect("builds");
    let error = s.open_midi_input(keys).expect_err("no OS MIDI layer");
    assert!(matches!(error, MidiPortError::NoHardware(_)), "{error:?}");
}

/// **A dropped output sends what its node held, before it closes**: the clock's
/// ticks of the blocks rendered since the last update reach the device when the
/// last handle is dropped in the update that closes the port.
#[test]
fn a_dropped_output_sends_what_its_node_held() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    roll(&s, 4_096);
    assert!(
        fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "nothing is drained before an update"
    );
    drop(out);
    let _ = s.update();
    assert!(
        !fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "the node's events were sent before the port closed"
    );
}

/// **A note off sent just before the last handle drops reaches the device**:
/// the port stays open, its tap drained into the device, until the graph has
/// rendered past the drop, so the note does not hang.
#[test]
fn a_note_off_sent_just_before_the_last_handle_drops_reaches_the_device() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    let node = out.node();
    let mut keys = s.midi().input(node).expect("the node takes events");
    let _ = s.update();
    let _ = fake.take_sent(EndpointId::from_raw(10));

    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .expect("queued");
    drop(out);
    let _ = s.update();
    assert!(s.graph_ref().contains(node), "no block has rendered it yet");
    blocks(&s, 3);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the output retired");
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(
        sent.iter().any(
            |u| matches!(u.note_key_event(), Some(NoteKeyEvent::Off(k)) if k.note().get() == 60)
        ),
        "the device received the note off: {sent:?}"
    );
}

/// **A device an output still retires on opens again at once**: the open ends
/// the retiring output, so the device has one connection of the session, and
/// the new port is live on it.
#[test]
fn a_device_opens_again_while_its_dropped_output_is_retiring() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let first = output(&mut s, &synth);
    let node = first.node();
    drop(first);
    let _ = s.update();
    assert!(s.graph_ref().contains(node), "it is retiring");
    let again = s.open_midi_output(synth).run().expect("opens at once");
    assert!(!s.graph_ref().contains(node), "the retiring output ended");
    assert_eq!(again.presence(), Presence::Live);
}

/// **A device is free to open again as soon as its port's last handle is
/// dropped**: the open sweeps the dropped port first, with no update between.
#[test]
fn a_dropped_input_can_be_opened_again_at_once() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let first = input(&mut s, &keys);
    drop(first);
    let again = s.open_midi_input(keys).expect("the dropped port freed it");
    assert_eq!(again.presence(), Presence::Live);
    assert_eq!(fake.connected_inputs().len(), 1, "one connection");
}

/// **An output opened again sweeps the dropped one**: its node is out of the
/// graph before the new port's is added, with no update between.
#[test]
fn a_dropped_output_is_closed_by_the_next_open() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let first = output(&mut s, &synth);
    let node = first.node();
    drop(first);
    let _second = s.open_midi_output(synth).run().expect("opens");
    assert!(!s.graph_ref().contains(node), "the dropped output closed");
}

/// **A port closes when its last handle is dropped, and not before**: a clone
/// keeps the device's connection and the input node, and the last drop ends the
/// connection the OS peer sees and takes the node out of the graph.
#[test]
fn a_port_closes_when_its_last_handle_is_dropped() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    let clone = port.clone();
    let node = port.node();
    assert!(s.graph_ref().contains(node));
    assert_eq!(fake.connected_inputs().len(), 1);

    drop(port);
    let _ = s.update();
    assert_eq!(clone.presence(), Presence::Live, "a clone holds the port");
    assert_eq!(fake.connected_inputs().len(), 1);
    assert!(s.graph_ref().contains(node));

    drop(clone);
    let _ = s.update();
    assert!(
        fake.connected_inputs().is_empty(),
        "the OS peer no longer sees the subscription"
    );
    // The node leaves once it has released what its device held: after a block.
    blocks(&s, 2);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the input node went");
}

/// **An output port closes with its last handle**: the node leaves the graph
/// and a device that returns finds no port to open.
#[test]
fn an_output_port_closes_when_its_last_handle_is_dropped() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    let node = out.node();
    assert!(s.graph_ref().contains(node));

    drop(out);
    let _ = s.update();
    assert!(
        s.graph_ref().contains(node),
        "the node stays until the graph has rendered past the drop"
    );
    blocks(&s, 3);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the output node went");
    unplug(&fake, 10);
    let _ = s.update();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    roll(&s, 4_096);
    let _ = s.update();
    assert!(
        fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "the dropped port did not reopen with its device"
    );
}

/// **A port that was dropped is not reopened when its device returns**: the
/// session tracks a port by its handles, so a dropped one is no longer
/// watched for its device.
#[test]
fn a_dropped_input_is_not_reopened_when_its_device_returns() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    unplug(&fake, 1);
    let _ = s.update();
    drop(port);
    let _ = s.update();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert!(fake.connected_inputs().is_empty());
}

/// **A device that returns is heard by the handle opened before it left**: the
/// port is tracked by its own state, not by a name the app declared, so the
/// handle that read `Lost` hears a note again once the device is back.
#[test]
fn the_same_handle_hears_a_device_that_returns() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&s, 4_096);
    assert_eq!(note_ons(&seen), [60]);

    unplug(&fake, 1);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 62), Instant::now()));
    roll(&s, 4_096);
    assert_eq!(note_ons(&seen), [60, 62], "the returned device is heard");
}

/// How many times the sustain pedal went up (a controller 64 of value 0) in what
/// the synth heard.
fn pedal_ups(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| {
            matches!(
                e.decode(),
                Ok(Message::Midi2(_, Voice2::ControlChange { controller, value }))
                    if controller == CCNumber::SUSTAIN && value.get() == 0
            )
        })
        .count()
}

/// Holds a key, the sustain pedal and a bend off centre on channel `channel` of
/// the fake device `raw`.
fn hold(fake: &FakeCounters, raw: u64, channel: u8, note: u8) {
    let id = EndpointId::from_raw(raw);
    let pedal = midi::m1_cc(MidiChannel::of(channel), CCNumber::SUSTAIN, U7::of(127));
    let bend = midi::pitch_bend(
        Address::new(MidiGroup::FIRST, MidiChannel::of(channel)),
        0xC000_0000,
    );
    for ump in [key(channel, note), pedal, bend] {
        assert!(fake.send_input(id, ump, Instant::now()));
    }
}

/// How many times a pitch bend returned to centre in what the synth heard.
fn centred_bends(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| {
            matches!(
                e.decode(),
                Ok(Message::Midi2(_, Voice2::PitchBend(v))) if v.get() == 0x8000_0000
            )
        })
        .count()
}

/// **What a device holds is released when its port goes**: a key, the sustain
/// pedal and a bend off centre on the device sound on the synth the route feeds,
/// and dropping the port ends them with a note off, a pedal up and a centred
/// bend, before the input node is taken out. (The executor's close of the
/// inlet's edge ends the keys and the pedal; the bend is the input node's own
/// release alone.)
#[test]
fn held_notes_are_released_when_the_port_is_dropped() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(2, 60), Instant::now()));
    let pedal = midi::m1_cc(MidiChannel::of(2), CCNumber::SUSTAIN, U7::of(127));
    assert!(fake.send_input(EndpointId::from_raw(1), pedal, Instant::now()));
    let bend = midi::pitch_bend(
        Address::new(MidiGroup::FIRST, MidiChannel::of(2)),
        0xC000_0000,
    );
    assert!(fake.send_input(EndpointId::from_raw(1), bend, Instant::now()));
    roll(&s, 8_192);
    assert_eq!(note_ons(&seen), [60]);
    assert!(note_offs(&seen).is_empty(), "the key is still down");
    assert_eq!(pedal_ups(&seen), 0, "the pedal is still down");
    assert_eq!(centred_bends(&seen), 0, "the bend is still off centre");
    let node = port.node();

    drop(port);
    let _ = s.update();
    assert!(
        s.graph_ref().contains(node),
        "the node stays until its release has gone out"
    );
    blocks(&s, 2);
    assert_eq!(
        note_offs(&seen),
        [60],
        "the port's removal released the key"
    );
    assert_eq!(pedal_ups(&seen), 1, "and the pedal");
    assert_eq!(centred_bends(&seen), 1, "and the bend");
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "and the node went after");
}

/// **A device that is unplugged has what it held released too**: the key, the
/// pedal and a bend off centre, by the port's node, which stays in the graph for
/// the replug; and the replug does not bring them back.
#[test]
fn held_notes_are_released_when_the_device_is_unplugged() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    let node = port.node();
    roll(&s, 1_024);
    hold(&fake, 1, 0, 64);
    roll(&s, 8_192);
    assert_eq!(note_ons(&seen), [64]);
    assert_eq!((pedal_ups(&seen), centred_bends(&seen)), (0, 0));

    unplug(&fake, 1);
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(note_offs(&seen), [64]);
    assert_eq!(pedal_ups(&seen), 1, "the pedal");
    assert_eq!(centred_bends(&seen), 1, "the bend");
    assert!(
        s.graph_ref().contains(node),
        "the node waits for the replug"
    );

    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    blocks(&s, 4);
    assert_eq!(note_offs(&seen), [64], "nothing is released twice");
    assert_eq!((pedal_ups(&seen), centred_bends(&seen)), (1, 1));
    assert_eq!(port.presence(), Presence::Live);
}

/// **Every input port has an input node of its own, and there is no cap**: ten
/// devices (the fixed input node took eight) are ten live ports on ten nodes, and
/// each device's note reaches a synth routed to all of them.
#[test]
fn every_input_port_has_its_own_input_node() {
    let (mut s, fake) = session();
    let names: Vec<String> = (1..=10).map(|n| format!("Keys {n}")).collect();
    for (n, name) in names.iter().enumerate() {
        plug(&fake, n as u32 + 1, name, PortDirection::Input);
    }
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let devices: Vec<DeviceInfo> = names.iter().map(|name| device(&mut s, name)).collect();
    let ports: Vec<_> = devices
        .iter()
        .map(|d| s.open_midi_input(d.clone()).expect("opens"))
        .collect();
    let _ = s.update();
    roll(&s, 1_024);
    assert!(ports.iter().all(|p| p.presence() == Presence::Live));
    let nodes: BTreeSet<NodeKey> = ports.iter().map(|p| p.node()).collect();
    assert_eq!(nodes.len(), 10, "one node each");
    for n in 0..10u64 {
        assert!(fake.send_input(
            EndpointId::from_raw(n + 1),
            key(0, 40 + n as u8),
            Instant::now()
        ));
    }
    roll(&s, 8_192);
    let mut heard = note_ons(&seen);
    heard.sort_unstable();
    assert_eq!(heard, (40..50).collect::<Vec<u8>>(), "each device once");
}

/// **One device going leaves another's port alone.**
#[test]
fn another_devices_removal_leaves_a_port_open() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 2, "Pads", PortDirection::Input);
    let _ = s.update();
    let (keys, pads) = (device(&mut s, "Keys"), device(&mut s, "Pads"));
    let (keys, pads) = (input(&mut s, &keys), input(&mut s, &pads));
    unplug(&fake, 2);
    let _ = s.update();
    assert_eq!(keys.presence(), Presence::Live);
    assert_eq!(pads.presence(), Presence::Lost(PortCause::Unplugged));
}

/// **What a device says of itself is its handle's info**: the update that
/// settles the UMP Stream negotiation says the port's info changed, and the
/// handle then reads the stream.
#[test]
fn a_ports_info_follows_what_the_device_says() {
    use tutti_midi_device::{Capabilities, Endpoint, Supports};
    use tutti_midi_hardware::Basis;
    use tutti_ump::Protocol;

    let (mut s, fake) = session();
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    let wire = device(&mut s, "Wire");
    let port = input(&mut s, &wire);
    assert!(port.info().stream().is_none(), "nothing settled yet");

    let mut device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    }));
    let raw = EndpointId::from_raw(7);
    let mut said = Vec::new();
    for _ in 0..20 {
        said.extend(changed(&s.update(), port.id()));
        let mut answers = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message) = ump.decode() {
                let _ = device.reply(message, &mut answers);
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, Instant::now()));
        }
    }
    assert!(said.contains(&ResourceFact::Info), "{said:?}");
    let info = port.info();
    let stream = info.stream().expect("the stream settled");
    assert_eq!(stream.basis(), Basis::Negotiated);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
}

/// **An output port sends what is wired to its node to the device**: a clip fed
/// to the port's node arrives at the fake device as its note on.
#[test]
fn an_output_port_sends_what_its_node_is_fed_to_the_device() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    assert_eq!(out.presence(), Presence::Live);

    let (clip, _) = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(EventInPort::new(out.node(), 0), [clip.events_out()])
        .expect("the port's node takes events");
    let _ = s.update();
    roll(&s, 1_024);
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(
        sent.iter().any(
            |u| matches!(u.note_key_event(), Some(NoteKeyEvent::On(k)) if k.note().get() == 60)
        ),
        "the device received the note on: {sent:?}"
    );
}

/// **An output whose device is unplugged reads `Lost` and counts what it was sent
/// as dropped, and sends again when the device returns.** The session's clock
/// feeds every output port.
#[test]
fn an_output_port_follows_its_device_through_an_unplug() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    let _ = s.update();
    roll(&s, 4_096);
    let _ = s.update();
    assert!(
        !fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "the clock reaches the device"
    );

    unplug(&fake, 10);
    let events = s.update();
    assert_eq!(changed(&events, out.id()), [ResourceFact::Presence]);
    assert_eq!(out.presence(), Presence::Lost(PortCause::Unplugged));
    let before = out.losses().refused;
    blocks(&s, 8);
    let _ = s.update();
    assert!(
        out.losses().refused > before,
        "events with no device are counted refused by the port"
    );

    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(out.presence(), Presence::Live);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    blocks(&s, 8);
    let _ = s.update();
    assert!(
        !fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "it sends again"
    );
}

/// **A port is a peer**: its id is the one in the events about it, and two
/// ports have two.
#[test]
fn a_ports_id_names_it_in_the_events() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 2, "Pads", PortDirection::Input);
    let _ = s.update();
    let (keys, pads) = (device(&mut s, "Keys"), device(&mut s, "Pads"));
    let (a, b) = (input(&mut s, &keys), input(&mut s, &pads));
    assert_ne!(a.id(), b.id());
    unplug(&fake, 1);
    let events = s.update();
    assert_eq!(changed(&events, a.id()), [ResourceFact::Presence]);
    assert!(changed(&events, b.id()).is_empty(), "only Keys went");
}

// --- Active Sensing ---

const SYNTH: u32 = 10;

/// The wait of a check that nothing is sent: about four of the thread's 270 ms
/// Active Sensing intervals.
const SECOND: std::time::Duration = std::time::Duration::from_secs(1);

fn is_active_sensing(u: &tutti_ump::Ump) -> bool {
    matches!(
        u.decode(),
        Ok(Message::System(_, tutti_ump::System::ActiveSensing))
    )
}

/// The `Synth` output of a MIDI 1.0 port that declares no block: the Protocol in
/// use is MIDI 1.0 and there is no UMP Stream to ask.
fn midi1_synth() -> tutti_midi_hardware::OsEndpoint {
    let mut synth = persistent_endpoint(SYNTH, "Synth");
    synth.capability = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    }));
    synth
}

/// A session with a MIDI 1.0 Protocol `Synth` output opened with Active Sensing
/// on or off, and its handle.
fn sensing_session(active_sensing: bool) -> (Session, FakeCounters, MidiOutput) {
    let (mut s, fake) = session();
    fake.plug(midi1_synth(), PortDirection::Output);
    let _ = s.update();
    let device = device(&mut s, "Synth");
    let out = s
        .open_midi_output(device)
        .active_sensing(active_sensing)
        .run()
        .expect("opens");
    (s, fake, out)
}

fn sent_by_the_port(fake: &FakeCounters) -> Vec<tutti_ump::Ump> {
    fake.take_sent(EndpointId::from_raw(u64::from(SYNTH)))
}

/// What the port sent, collected until `done` accepts it. A port that sends
/// nothing fails after a hang guard of half a minute.
fn sent_until(
    fake: &FakeCounters,
    what: &str,
    done: impl Fn(&[tutti_ump::Ump]) -> bool,
) -> Vec<tutti_ump::Ump> {
    let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(30);
    let mut sent = Vec::new();
    while !done(&sent) {
        assert!(
            tutti_platform::Instant::now() < deadline,
            "{what}: {sent:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        sent.extend(sent_by_the_port(fake));
    }
    sent
}

/// **An idle output port with Active Sensing sends it before a receiver gives
/// up, goes on sending it with no `update()` to drive it, and a re-declared
/// node does not stop it.**
#[test]
fn an_idle_output_port_active_sensing_sends_it_with_no_update_to_drive_it() {
    let (mut s, fake, out) = sensing_session(true);
    let first = sent_until(&fake, "no Active Sensing was sent", |sent| !sent.is_empty());
    assert!(first.iter().all(is_active_sensing), "{first:?}");
    // The node's sources are replaced, and no update runs while it goes on.
    let (clip, _) = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(EventInPort::new(out.node(), 0), [clip.events_out()])
        .expect("the port's node takes events");
    s.graph()
        .set_sources(EventInPort::new(out.node(), 0), [])
        .expect("and drops them");
    let later = sent_until(&fake, "it did not go on", |sent| sent.len() >= 2);
    assert!(
        later.iter().all(is_active_sensing),
        "and it goes on: {later:?}"
    );
}

/// **An output port declared without the option never sends Active Sensing.**
#[test]
fn an_output_port_without_the_option_never_sends_active_sensing() {
    let (_s, fake, _out) = sensing_session(false);
    std::thread::sleep(SECOND);
    assert!(sent_by_the_port(&fake).is_empty());
}

/// **Closing the port stops Active Sensing**: the last handle dropped, nothing
/// more reaches the device.
#[test]
fn closing_an_output_port_stops_its_active_sensing() {
    let (mut s, fake, out) = sensing_session(true);
    let _ = sent_until(&fake, "precondition: it runs", |sent| !sent.is_empty());
    drop(out);
    let _ = s.update();
    // The device closes once the graph has rendered past the drop.
    blocks(&s, 3);
    let _ = s.update();
    let _ = sent_by_the_port(&fake);
    std::thread::sleep(SECOND);
    assert!(sent_by_the_port(&fake).is_empty());
}

/// **A reopened output keeps its Active Sensing**: the device unplugged and
/// back, the port sends Active Sensing again on the new connection.
#[test]
fn a_reopened_output_port_sends_active_sensing_again() {
    let (mut s, fake, _out) = sensing_session(true);
    let _ = sent_until(&fake, "precondition: it runs", |sent| !sent.is_empty());
    unplug(&fake, SYNTH);
    let _ = s.update();
    let _ = sent_by_the_port(&fake);
    fake.plug(midi1_synth(), PortDirection::Output);
    let _ = s.update();
    let sent = sent_until(&fake, "the reopened port sends", |sent| {
        sent.iter().any(is_active_sensing)
    });
    assert!(sent.iter().all(is_active_sensing), "{sent:?}");
}

/// **A port whose device is in the MIDI 2.0 Protocol sends no Active Sensing**,
/// the option set or not.
#[test]
fn a_midi2_protocol_output_port_sends_no_active_sensing() {
    let (mut s, fake) = session();
    plug(&fake, SYNTH, "Synth", PortDirection::Output);
    let _ = s.update();
    let device = device(&mut s, "Synth");
    let _out = s
        .open_midi_output(device)
        .active_sensing(true)
        .run()
        .expect("opens");
    std::thread::sleep(SECOND);
    assert!(sent_by_the_port(&fake).is_empty());
}

// --- Losses ---

/// **A port counts what its device refused, in its own `losses()`.**
#[test]
fn an_output_port_counts_what_its_device_refused() {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], 0);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    plug(&fake, SYNTH, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    roll(&s, 4_096);
    let _ = s.update();
    let losses = out.losses();
    assert!(losses.refused > 0, "{losses:?}");
    assert_eq!(losses.lost, 0);
}

/// **A port counts the events its node's tap had no room for**: a clock
/// that outruns the tap between two updates.
#[test]
fn an_output_port_counts_the_events_its_tap_had_no_room_for() {
    let (mut s, fake) = session();
    plug(&fake, SYNTH, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    roll(&s, 1_100_000);
    let _ = s.update();
    assert!(out.losses().lost > 0, "{:?}", out.losses());
}

/// How many Registered Controller messages the synth heard.
fn registered(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| matches!(e.decode(), Ok(Message::Midi2(_, Voice2::Registered { .. }))))
        .count()
}

/// **The session's MPE mode reaches a port's input node**: with no mode the
/// routed synth hears no bend range; once `session.mpe()` is written with a
/// lower zone of three members, the input node sends the zone's seven default
/// bend ranges (the master's, and a channel's and a per-note one on each
/// member) from the next block, and writing the same mode again sends none.
#[test]
fn the_sessions_mpe_mode_reaches_a_ports_input_node() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let _port = input(&mut s, &keys);
    roll(&s, 1_024);
    assert_eq!(registered(&seen), 0);

    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(registered(&seen), 7);

    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(registered(&seen), 7, "the same mode: nothing sent");
}
