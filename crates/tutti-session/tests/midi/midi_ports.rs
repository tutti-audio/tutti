//! MIDI ports: `open_midi_input` and `open_midi_output` open a saved device,
//! and the handle reads whether it is there. A fake OS plugs and unplugs the
//! devices.

use std::collections::BTreeSet;
use tutti_platform::Instant;

use crate::common::OpenPorts as _;
use crate::common::{MF, Seen, blocks, headed_clip, note, play, program, tap, volume};
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_midi_device::{Capabilities, DeviceInfo, Endpoint, EndpointId, PortDirection, Supports};
use tutti_midi_hardware::InputFault;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint, persistent_endpoint};
use tutti_midi_runtime::{ClipEvents, MidiClipConfig, MidiClipNode};
use tutti_session::{
    EventInputPort, MidiInput, MidiOutput, MidiPortError, MidiRouteRule, Peer, PeerEvent,
    PortCause, Presence, Resource, ResourceFact, Session, SessionEvent, Stamp,
};
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteKeyEvent, NoteNumber, NoteVelocity, U7,
    Ump, Voice2,
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
    let port = s.open_input(info.clone()).expect("opens");
    let _ = s.update();
    port
}

/// Opens the output port of `info`, and updates.
fn output(s: &mut Session, info: &DeviceInfo) -> MidiOutput {
    let port = s.open_output(info.clone()).expect("opens");
    let _ = s.update();
    port
}

/// What an update said of the resource `id`.
fn changed(events: &[SessionEvent], id: tutti_session::PeerResourceId) -> Vec<ResourceFact> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Peer(PeerEvent::ResourceChanged { id: of, fact }) if *of == id => {
                Some(*fact)
            }
            _ => None,
        })
        .collect()
}

fn roll(s: &mut Session, frames: usize) {
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
/// the same handle**: each change is one `PeerEvent::ResourceChanged` of presence, and the
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
    let error = s.open_input(keys.clone()).expect_err("the device is held");
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
    let events = s.update();
    assert_eq!(port.presence(), Presence::Live, "reopened at once");
    assert!(!fake.connected_inputs().is_empty());
    assert_eq!(
        changed(&events, port.id()),
        [],
        "a reopen that says the same raises no change"
    );

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
    let before = s.graph_ref().testing().sent_plan().map(|p| p.units().len());
    let mut nodes = Vec::new();
    for _ in 0..3 {
        let port = input(&mut s, &keys);
        let _ = s.update();
        blocks(&mut s, 1);
        nodes.push(port.node());
        drop(port);
        let _ = s.update();
        assert!(s.graph_ref().contains(*nodes.last().unwrap()));
        blocks(&mut s, 2);
        let _ = s.update();
    }
    for node in nodes {
        assert!(!s.graph_ref().contains(node), "{node:?} left");
    }
    let after = s.graph_ref().testing().sent_plan().map(|p| p.units().len());
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
        .open_input(saved.clone())
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
    let port = s.open_input(saved).expect("opens once one left");
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
    let error = s.open_input(keys).expect_err("no OS MIDI layer");
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
    roll(&mut s, 4_096);
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
    blocks(&mut s, 3);
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

/// A session with an output port on `Synth`, and the keys of its node.
fn retiring_rig() -> (
    Session,
    FakeCounters,
    MidiOutput,
    tutti_midi_runtime::MidiKeys,
    DeviceInfo,
) {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    let keys = s.midi().input(out.node()).expect("the node takes events");
    let _ = s.update();
    let _ = fake.take_sent(EndpointId::from_raw(10));
    (s, fake, out, keys, synth)
}

fn is_off(ump: &tutti_ump::Ump, note: u8) -> bool {
    matches!(ump.note_key_event(), Some(NoteKeyEvent::Off(k)) if k.note().get() == note)
}

fn is_on(ump: &tutti_ump::Ump, note: u8) -> bool {
    matches!(ump.note_key_event(), Some(NoteKeyEvent::On(k)) if k.note().get() == note)
}

/// **A device an output still retires on opens again at once, on one
/// connection**: the open ends the retiring output with what its tap held sent,
/// so the device never has two connections of the session, and the new port is
/// live on it.
#[test]
fn a_device_opens_again_while_its_dropped_output_is_retiring() {
    let (mut s, fake, out, mut keys, synth) = retiring_rig();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .expect("queued");
    blocks(&mut s, 1);
    let node = out.node();
    drop(out);
    let _ = s.update();
    assert!(s.graph_ref().contains(node), "it is retiring");
    let again = s.open_output(synth).expect("opens at once");
    assert!(!s.graph_ref().contains(node), "the retiring output ended");
    assert_eq!(again.presence(), Presence::Live);
    assert_eq!(fake.most_output_connections(), 1, "never two connections");
    assert_eq!(fake.connected_outputs().len(), 1, "one connection now");
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(
        sent.iter().any(|u| is_off(u, 60)),
        "the rendered note off was drained: {sent:?}"
    );
}

/// **Opening a device again loses what the graph had not rendered**: the
/// retiring output is ended at the open, so a note off queued and not yet
/// rendered does not reach the device. This is the cost of the open being
/// at once.
#[test]
fn reopening_a_device_loses_what_its_retiring_output_had_not_rendered() {
    let (mut s, fake, out, mut keys, synth) = retiring_rig();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .expect("queued");
    drop(out);
    let _again = s.open_output(synth).expect("opens at once");
    blocks(&mut s, 3);
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(
        !sent.iter().any(|u| is_off(u, 60)),
        "the unrendered note off was lost: {sent:?}"
    );
}

/// **A note on that a source sends after the drop is ended before the device
/// closes**: the node passes it in the window, and ends it in the last block it
/// passes events.
#[test]
fn a_note_on_sent_after_the_drop_is_ended_before_the_device_closes() {
    let (mut s, fake, out, mut keys, _) = retiring_rig();
    let node = out.node();
    drop(out);
    let _ = s.update();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(64),
        tutti_ump::NoteVelocity::new(MF),
    )
    .expect("queued");
    blocks(&mut s, 3);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the output retired");
    let sent = fake.take_sent(EndpointId::from_raw(10));
    let on = sent.iter().position(|u| is_on(u, 64)).expect("the note on");
    let off = sent
        .iter()
        .position(|u| is_off(u, 64))
        .expect("the note off");
    assert!(on < off, "the off follows the on: {sent:?}");
    assert!(fake.connected_outputs().is_empty(), "the device closed");
}

/// **An event queued in the update of the drop reaches the device**: the blocks
/// that render it run before the update that retires the port.
#[test]
fn an_event_queued_in_the_update_of_the_drop_reaches_the_device() {
    let (mut s, fake, out, mut keys, _) = retiring_rig();
    let node = out.node();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(61))
        .expect("queued");
    drop(out);
    blocks(&mut s, 1);
    let _ = s.update();
    blocks(&mut s, 3);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the output retired");
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(sent.iter().any(|u| is_off(u, 61)), "{sent:?}");
}

/// **With no block rendered a retiring output stays open, and the session's drop
/// closes it**: a stopped engine does not advance the generation, so the
/// connection and the node stay, update after update, until the device is
/// opened again or the session ends.
#[test]
fn a_retiring_output_stays_open_while_no_block_renders_and_closes_with_the_session() {
    let (mut s, fake, out, _keys, _) = retiring_rig();
    let node = out.node();
    drop(out);
    for _ in 0..3 {
        let _ = s.update();
        assert!(s.graph_ref().contains(node), "the node stays");
        assert_eq!(fake.connected_outputs().len(), 1, "the connection stays");
    }
    drop(s);
    assert!(fake.connected_outputs().is_empty(), "the session closed it");
}

/// **A retiring output lets go of an unplugged device and is not reopened with
/// it**: it neither errors nor holds a connection a returning device would
/// meet, and still ends once the graph has rendered past the drop.
#[test]
fn a_retiring_output_lets_go_of_a_device_that_unplugs() {
    let (mut s, fake, out, _keys, _) = retiring_rig();
    let node = out.node();
    drop(out);
    let _ = s.update();
    unplug(&fake, 10);
    let _ = s.update();
    assert!(fake.connected_outputs().is_empty(), "the connection went");
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert!(fake.connected_outputs().is_empty(), "it did not reopen");
    blocks(&mut s, 3);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "it ended");
}

/// **A host that removes a retiring output's node ends the port**: there is no
/// block left to wait for, and the device closes.
#[test]
fn a_host_removed_node_ends_a_retiring_output() {
    let (mut s, fake, out, _keys, _) = retiring_rig();
    let node = out.node();
    drop(out);
    let _ = s.update();
    assert_eq!(fake.connected_outputs().len(), 1, "it is retiring");
    let _ = s.remove(node);
    let _ = s.update();
    assert!(fake.connected_outputs().is_empty(), "the device closed");
}

/// **A retiring output follows the graph across a re-rate**: its tap is
/// reopened at the new rate, so what it rendered still reaches the device and
/// the port ends.
#[test]
fn a_retiring_output_survives_a_re_rate() {
    let (mut s, fake, out, mut keys, _) = retiring_rig();
    let node = out.node();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(62))
        .expect("queued");
    drop(out);
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let _ = s.update();
    // The re-prepare's second half lands on a render, then the commit.
    blocks(&mut s, 8);
    let _ = s.update();
    blocks(&mut s, 4);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the output ended");
    assert!(fake.connected_outputs().is_empty(), "the device closed");
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(sent.iter().any(|u| is_off(u, 62)), "{sent:?}");
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
    let again = s.open_input(keys).expect("the dropped port freed it");
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
    let _second = s.open_output(synth).expect("opens");
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
    blocks(&mut s, 2);
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
    blocks(&mut s, 3);
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
    roll(&mut s, 4_096);
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
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    roll(&mut s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&mut s, 4_096);
    assert_eq!(note_ons(&seen), [60]);

    unplug(&fake, 1);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 62), Instant::now()));
    roll(&mut s, 4_096);
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
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    roll(&mut s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(2, 60), Instant::now()));
    let pedal = midi::m1_cc(MidiChannel::of(2), CCNumber::SUSTAIN, U7::of(127));
    assert!(fake.send_input(EndpointId::from_raw(1), pedal, Instant::now()));
    let bend = midi::pitch_bend(
        Address::new(MidiGroup::FIRST, MidiChannel::of(2)),
        0xC000_0000,
    );
    assert!(fake.send_input(EndpointId::from_raw(1), bend, Instant::now()));
    roll(&mut s, 8_192);
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
    blocks(&mut s, 2);
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
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let port = input(&mut s, &keys);
    let node = port.node();
    roll(&mut s, 1_024);
    hold(&fake, 1, 0, 64);
    roll(&mut s, 8_192);
    assert_eq!(note_ons(&seen), [64]);
    assert_eq!((pedal_ups(&seen), centred_bends(&seen)), (0, 0));

    unplug(&fake, 1);
    let _ = s.update();
    blocks(&mut s, 2);
    assert_eq!(note_offs(&seen), [64]);
    assert_eq!(pedal_ups(&seen), 1, "the pedal");
    assert_eq!(centred_bends(&seen), 1, "the bend");
    assert!(
        s.graph_ref().contains(node),
        "the node waits for the replug"
    );

    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    blocks(&mut s, 4);
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
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let devices: Vec<DeviceInfo> = names.iter().map(|name| device(&mut s, name)).collect();
    let ports: Vec<_> = devices
        .iter()
        .map(|d| s.open_input(d.clone()).expect("opens"))
        .collect();
    let _ = s.update();
    roll(&mut s, 1_024);
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
    roll(&mut s, 8_192);
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

    let clip = s
        .graph()
        .add(MidiClipNode::from(MidiClipConfig::default().events(
            ClipEvents::from_iter(note(60, MF, Beat::new(0.0), BeatDuration::new(1.0))),
        )));
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the port's node takes events");
    let _ = s.update();
    roll(&mut s, 1_024);
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
    roll(&mut s, 4_096);
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
    blocks(&mut s, 8);
    let _ = s.update();
    assert!(
        out.losses().refused > before,
        "events with no device are counted refused by the port"
    );

    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(out.presence(), Presence::Live);
    let _ = fake.take_sent(EndpointId::from_raw(10));
    blocks(&mut s, 8);
    let _ = s.update();
    assert!(
        !fake.take_sent(EndpointId::from_raw(10)).is_empty(),
        "it sends again"
    );
}

/// A session over a fake OS with the output `Synth` (endpoint 10) open, fed by
/// a `headed_clip`: its node, and the clip's node.
fn clip_into_synth() -> (Session, FakeCounters, MidiOutput) {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    let clip = s.graph().add(MidiClipNode::from(MidiClipConfig::default()));
    s.set(clip.events().to(headed_clip())).expect("sets");
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the port's node takes events");
    let _ = s.update();
    blocks(&mut s, 1);
    let _ = s.update();
    (s, fake, out)
}

/// What the clip puts on the wire: its header and chase, from what the device
/// was sent.
fn setup_sent(fake: &FakeCounters) -> Vec<tutti_ump::Ump> {
    fake.take_sent(EndpointId::from_raw(10))
        .into_iter()
        .filter(|u| [program(1), volume(), program(5)].contains(u))
        .collect()
}

/// **A device that returns to a port a clip plays into is caught up to the
/// state the port said, once, when it returns.** The device lost its program and
/// controllers with its power; the port's record has the last program and the
/// volume (the header's program 1 was replaced by the clip's program 5), and the
/// returned device hears them at the return: program first, then the
/// controller, the record's catch-up order (the channel's bank and program, then
/// its controllers).
#[test]
fn a_returned_output_device_is_caught_up_to_the_ports_state_once() {
    let (mut s, fake, out) = clip_into_synth();
    play(&s);
    blocks(&mut s, 2);
    let _ = s.update();
    assert_eq!(
        setup_sent(&fake),
        [program(1), volume(), program(5)],
        "the device hears the setup at the start"
    );

    unplug(&fake, 10);
    let _ = s.update();
    blocks(&mut s, 2);
    let _ = s.update();
    let _ = setup_sent(&fake);
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(out.presence(), Presence::Live);
    assert_eq!(
        setup_sent(&fake),
        [program(5), volume()],
        "the returned device hears the state at the return"
    );
    blocks(&mut s, 4);
    let _ = s.update();
    assert_eq!(setup_sent(&fake), [], "once");
}

/// **What a live source sent through the port comes back with the device**: a
/// program, a controller and a key held down, whichever node sent them, in the
/// record's catch-up order: the program, the controller, then the key (held notes
/// last, so each sounds on its channel's patch).
#[test]
fn a_returned_output_device_hears_what_a_live_source_sent_through_the_port() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let out = output(&mut s, &synth);
    let mut keys = s.midi().input(out.node()).expect("the output takes MIDI");
    let _ = s.update();
    let note: Ump = Message::midi2(
        MidiChannel::FIRST,
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
    )
    .into();
    for ump in [note, volume(), program(5)] {
        keys.push(Stamp::NextBlock, ump).expect("room");
    }
    blocks(&mut s, 2);
    let _ = s.update();
    let heard = |fake: &FakeCounters| -> Vec<Ump> {
        fake.take_sent(EndpointId::from_raw(10))
            .into_iter()
            .filter(|u| [program(5), volume(), note].contains(u))
            .collect()
    };
    assert_eq!(heard(&fake), [note, volume(), program(5)], "as sent");

    unplug(&fake, 10);
    let _ = s.update();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(
        heard(&fake),
        [program(5), volume(), note],
        "the program and controller, then the key that is down"
    );
}

/// **A replug while stopped sends nothing, as the port has said nothing; the
/// next start sends the header once.**
#[test]
fn a_returned_output_device_while_stopped_hears_the_header_at_the_start() {
    let (mut s, fake, _out) = clip_into_synth();
    blocks(&mut s, 2);
    unplug(&fake, 10);
    let _ = s.update();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    blocks(&mut s, 2);
    let _ = s.update();
    assert_eq!(setup_sent(&fake), [], "stopped: nothing");
    play(&s);
    blocks(&mut s, 2);
    let _ = s.update();
    assert_eq!(setup_sent(&fake), [program(1), volume(), program(5)]);
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

/// A second output, always opened with Active Sensing after the output under
/// test: once it has sent twice, the output under test has had two chances to.
const WITNESS: u32 = 11;

fn is_active_sensing(u: &tutti_ump::Ump) -> bool {
    matches!(
        u.decode(),
        Ok(Message::System(_, tutti_ump::System::ActiveSensing))
    )
}

/// The output of a MIDI 1.0 port that declares no block: the Protocol in use is
/// MIDI 1.0 and there is no UMP Stream to ask.
fn midi1_output(raw: u32, name: &str) -> tutti_midi_hardware::OsEndpoint {
    let mut synth = persistent_endpoint(raw, name);
    synth.capability = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        ..Capabilities::default()
    }));
    synth
}

fn midi1_synth() -> tutti_midi_hardware::OsEndpoint {
    midi1_output(SYNTH, "Synth")
}

/// A session with a MIDI 1.0 Protocol `Synth` output opened with Active Sensing
/// on or off, and its handle.
fn sensing_session(active_sensing: bool) -> (Session, FakeCounters, MidiOutput) {
    let (mut s, fake) = session();
    fake.plug(midi1_synth(), PortDirection::Output);
    let _ = s.update();
    let device = device(&mut s, "Synth");
    let out = s
        .open_sensing_output(device, active_sensing)
        .expect("opens");
    (s, fake, out)
}

/// Plugs the witness output into `s` and opens it with Active Sensing.
fn open_witness(s: &mut Session, fake: &FakeCounters) -> MidiOutput {
    fake.plug(midi1_output(WITNESS, "Witness"), PortDirection::Output);
    let _ = s.update();
    let device = device(s, "Witness");
    s.open_sensing_output(device, true).expect("opens")
}

fn sent_by(fake: &FakeCounters, raw: u32) -> Vec<tutti_ump::Ump> {
    fake.take_sent(EndpointId::from_raw(u64::from(raw)))
}

fn sent_by_the_port(fake: &FakeCounters) -> Vec<tutti_ump::Ump> {
    sent_by(fake, SYNTH)
}

/// What output `raw` sent, collected until `done` accepts it. An output that
/// sends nothing fails after a hang guard of half a minute.
fn sent_until(
    fake: &FakeCounters,
    raw: u32,
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
        std::thread::yield_now();
        sent.extend(sent_by(fake, raw));
    }
    sent
}

/// Waits until the witness has sent two Active Sensing.
fn witness_sends_twice(fake: &FakeCounters) {
    let _ = sent_until(fake, WITNESS, "the witness sends", |sent| sent.len() >= 2);
}

/// **An idle output port with Active Sensing sends it before a receiver gives
/// up, goes on sending it with no `update()` to drive it, and a re-declared
/// node does not stop it.**
#[test]
fn an_idle_output_port_active_sensing_sends_it_with_no_update_to_drive_it() {
    let (mut s, fake, out) = sensing_session(true);
    let first = sent_until(&fake, SYNTH, "no Active Sensing was sent", |sent| {
        !sent.is_empty()
    });
    assert!(first.iter().all(is_active_sensing), "{first:?}");
    // The node's sources are replaced, and no update runs while it goes on.
    let clip = s
        .graph()
        .add(MidiClipNode::from(MidiClipConfig::default().events(
            ClipEvents::from_iter(note(60, MF, Beat::new(0.0), BeatDuration::new(1.0))),
        )));
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the port's node takes events");
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [])
        .expect("and drops them");
    let later = sent_until(&fake, SYNTH, "it did not go on", |sent| sent.len() >= 2);
    assert!(
        later.iter().all(is_active_sensing),
        "and it goes on: {later:?}"
    );
}

/// **An output port declared without the option never sends Active Sensing**:
/// it sends nothing while the witness sends twice.
#[test]
fn an_output_port_without_the_option_never_sends_active_sensing() {
    let (mut s, fake, _out) = sensing_session(false);
    let _witness = open_witness(&mut s, &fake);
    witness_sends_twice(&fake);
    assert!(sent_by_the_port(&fake).is_empty());
}

/// **Closing the port stops Active Sensing**: the last handle dropped, nothing
/// more reaches the device while the witness sends twice.
#[test]
fn closing_an_output_port_stops_its_active_sensing() {
    let (mut s, fake, out) = sensing_session(true);
    let _witness = open_witness(&mut s, &fake);
    let _ = sent_until(&fake, SYNTH, "precondition: it runs", |sent| {
        !sent.is_empty()
    });
    drop(out);
    let _ = s.update();
    // The device closes once the graph has rendered past the drop.
    blocks(&mut s, 3);
    let _ = s.update();
    let _ = sent_by_the_port(&fake);
    let _ = sent_by(&fake, WITNESS);
    witness_sends_twice(&fake);
    assert!(sent_by_the_port(&fake).is_empty());
}

/// **A retiring output on a MIDI 1.0 device ends a held note with a MIDI 1.0
/// Note Off**: the node's MIDI 2.0 off is translated for the device, so the wire
/// carries a 0x8n (or a velocity 0 0x9n) for the key (UMP message type 2).
#[test]
fn a_retiring_output_ends_a_held_note_on_a_midi1_device_with_a_midi1_off() {
    let (mut s, fake, out) = sensing_session(false);
    let mut keys = s.midi().input(out.node()).expect("the node takes events");
    let _ = s.update();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(66),
        tutti_ump::NoteVelocity::new(MF),
    )
    .expect("queued");
    blocks(&mut s, 1);
    drop(out);
    let _ = s.update();
    blocks(&mut s, 3);
    let _ = s.update();
    let sent = sent_by_the_port(&fake);
    let off = sent
        .iter()
        .find(|u| is_off(u, 66))
        .unwrap_or_else(|| panic!("the note off: {sent:?}"));
    let word = off.0[0];
    assert_eq!(word >> 28, 2, "a MIDI 1.0 channel voice message: {word:#x}");
    let status = (word >> 20) & 0xF;
    let velocity = word & 0x7F;
    assert!(
        status == 0x8 || (status == 0x9 && velocity == 0),
        "a Note Off, or a Note On at velocity 0: {word:#x}"
    );
}

/// **A reopened output keeps its Active Sensing**: the device unplugged and
/// back, the port sends Active Sensing again on the new connection.
#[test]
fn a_reopened_output_port_sends_active_sensing_again() {
    let (mut s, fake, _out) = sensing_session(true);
    let _ = sent_until(&fake, SYNTH, "precondition: it runs", |sent| {
        !sent.is_empty()
    });
    unplug(&fake, SYNTH);
    let _ = s.update();
    let _ = sent_by_the_port(&fake);
    fake.plug(midi1_synth(), PortDirection::Output);
    let _ = s.update();
    let sent = sent_until(&fake, SYNTH, "the reopened port sends", |sent| {
        sent.iter().any(is_active_sensing)
    });
    assert!(sent.iter().all(is_active_sensing), "{sent:?}");
}

/// **A port whose device is in the MIDI 2.0 Protocol sends no Active Sensing**,
/// the option set or not: it sends nothing while the witness sends twice.
#[test]
fn a_midi2_protocol_output_port_sends_no_active_sensing() {
    let (mut s, fake) = session();
    plug(&fake, SYNTH, "Synth", PortDirection::Output);
    let _ = s.update();
    let device = device(&mut s, "Synth");
    let _out = s.open_sensing_output(device, true).expect("opens");
    let _witness = open_witness(&mut s, &fake);
    witness_sends_twice(&fake);
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
    roll(&mut s, 4_096);
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
    roll(&mut s, 1_100_000);
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
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    let _port = input(&mut s, &keys);
    roll(&mut s, 1_024);
    assert_eq!(registered(&seen), 0);

    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    let _ = s.update();
    blocks(&mut s, 2);
    assert_eq!(registered(&seen), 7);

    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    let _ = s.update();
    blocks(&mut s, 2);
    assert_eq!(registered(&seen), 7, "the same mode: nothing sent");
}

/// A session over a fake OS with an input `Keys` (endpoint 1) and an output `Synth`
/// (endpoint 2) plugged.
fn a_session_with_keys_and_synth() -> (Session, FakeCounters) {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 2, "Synth", PortDirection::Output);
    let _ = s.update();
    (s, fake)
}

/// **An open over a backend with nothing to start is ready on its first poll, and
/// its apply installs a live input that hears**: the wait resolves to the proof
/// and the saved device, and the port opens only at the apply.
#[test]
fn an_open_is_ready_on_the_first_poll_and_its_apply_installs_a_live_input() {
    let (mut s, fake) = a_session_with_keys_and_synth();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::new().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");

    let opening = s.open_midi_input(keys).expect("the device binds");
    assert!(
        fake.connected_inputs().is_empty(),
        "nothing opens before the apply"
    );
    let opened = crate::common::polled(opening)
        .expect("a backend with nothing to start is ready on the first poll")
        .expect("it started");
    let port = s.apply(opened).expect("installs");
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);

    roll(&mut s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&mut s, 4_096);
    assert_eq!(note_ons(&seen), [60], "the installed port hears its device");
}

/// **A call that cannot bind fails at the call, with a named error and no
/// future**: a session with no OS MIDI layer, and a saved device that several
/// present ones could be.
#[test]
fn a_device_that_cannot_bind_fails_at_the_call() {
    let (with, _fake) = a_session_with_keys_and_synth();
    let mut with = with;
    let keys = device(&mut with, "Keys");
    let without = Session::builder().build().expect("builds");
    let error = without
        .open_midi_output(keys)
        .expect_err("no OS MIDI layer");
    assert!(matches!(error, MidiPortError::NoHardware(_)), "{error:?}");

    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Pad"), PortDirection::Input);
    fake.plug(endpoint(2, "Pad"), PortDirection::Input);
    fake.plug(endpoint(3, "Pod"), PortDirection::Output);
    fake.plug(endpoint(4, "Pod"), PortDirection::Output);
    let _ = s.update();
    let pad = s.midi().devices().remove(0);
    let pod = s.midi().devices().remove(2);
    let error = s
        .open_midi_input(pad.clone())
        .expect_err("two present devices could be the saved one");
    assert!(
        matches!(&error, MidiPortError::Ambiguous(c) if c.len() == 2),
        "{error:?}"
    );
    let error = s.open_midi_output(pod).expect_err("so could two outputs");
    assert!(matches!(error, MidiPortError::Ambiguous(_)), "{error:?}");
}

/// **An open made by one session is stale in another**: an opened input or output
/// applied to a session other than the one that began it is refused as `Stale`
/// and installs nothing.
#[test]
fn an_open_of_a_session_with_keys_and_synth_is_stale() {
    let (a, _fake_a) = a_session_with_keys_and_synth();
    let (mut b, fake_b) = a_session_with_keys_and_synth();
    let mut a = a;
    let keys = device(&mut a, "Keys");
    let synth = device(&mut a, "Synth");

    let opened = crate::common::ready(a.open_midi_input(keys).expect("binds"));
    let error = b.apply(opened).expect_err("another session's open");
    assert!(matches!(error, MidiPortError::Stale), "{error:?}");
    let opened = crate::common::ready(a.open_midi_output(synth).expect("binds"));
    let error = b.apply(opened).expect_err("another session's open");
    assert!(matches!(error, MidiPortError::Stale), "{error:?}");
    assert!(fake_b.connected_inputs().is_empty(), "nothing opened in b");
}

/// **An open holds nothing of its session, so a dropped session does not end it,
/// and what it resolves to is stale in every other**: the wait resolves after
/// its session is gone, and its apply refuses by name instead of pending.
#[test]
fn an_open_outlives_its_session_and_is_stale_in_another() {
    let (mut a, _fake_a) = a_session_with_keys_and_synth();
    let (mut b, _fake_b) = a_session_with_keys_and_synth();
    let keys = device(&mut a, "Keys");
    let synth = device(&mut a, "Synth");
    let opening_input = a.open_midi_input(keys).expect("binds");
    let opening_output = a.open_midi_output(synth).expect("binds");
    drop(a);

    let opened = crate::common::polled(opening_input)
        .expect("a dropped session does not hold the wait")
        .expect("it started");
    let error = b.apply(opened).expect_err("the session is gone");
    assert!(matches!(error, MidiPortError::Stale), "{error:?}");
    let opened = crate::common::polled(opening_output)
        .expect("a dropped session does not hold the wait")
        .expect("it started");
    let error = b.apply(opened).expect_err("the session is gone");
    assert!(matches!(error, MidiPortError::Stale), "{error:?}");
}

/// **The apply binds against the device table as it then is**: a device that
/// departed while the open was pending installs a port reading `Lost`, which
/// opens behind the same handle when the device returns.
#[test]
fn a_device_that_departs_while_the_open_is_pending_installs_a_lost_port() {
    let (mut s, fake) = a_session_with_keys_and_synth();
    let keys = device(&mut s, "Keys");
    let opened = crate::common::ready(s.open_midi_input(keys).expect("binds"));

    unplug(&fake, 1);
    let _ = s.update();
    let port = s.apply(opened).expect("installs, unplugged");
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert!(fake.connected_inputs().is_empty());

    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live, "it opens when it returns");
}

/// **A second device of the saved name arriving while the open is pending makes
/// the apply refuse it as ambiguous**, for an input and for an output, and
/// installs nothing.
#[test]
fn a_device_that_makes_the_saved_one_ambiguous_while_pending_fails_the_apply() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Pad"), PortDirection::Input);
    fake.plug(endpoint(2, "Pod"), PortDirection::Output);
    let _ = s.update();
    let input = device(&mut s, "Pad");
    let output = device(&mut s, "Pod");
    let opened_input = crate::common::ready(s.open_midi_input(input).expect("binds"));
    let opened_output = crate::common::ready(s.open_midi_output(output).expect("binds"));

    fake.plug(endpoint(3, "Pad"), PortDirection::Input);
    fake.plug(endpoint(4, "Pod"), PortDirection::Output);
    let _ = s.update();
    let error = s.apply(opened_input).expect_err("two could be it now");
    assert!(matches!(error, MidiPortError::Ambiguous(_)), "{error:?}");
    let error = s.apply(opened_output).expect_err("two could be it now");
    assert!(matches!(error, MidiPortError::Ambiguous(_)), "{error:?}");
    assert!(
        fake.connected_inputs().is_empty(),
        "the failed apply left no port"
    );
}

/// The MPE Configuration Message of a lower zone of `members` Member Channels
/// on Group `group`, in MIDI 2.0 (M1-100 section 2.2.1): the RPN 6 of the
/// Manager Channel.
fn configuration(group: u8, members: u32) -> Ump {
    Ump([0x4020_0006 | u32::from(group) << 24, members << 25, 0, 0])
}

fn lower_zone(members: u8) -> tutti_midi_protocol::Mpe11Zones {
    use tutti_midi_protocol::{MemberCount, Mpe11Zone, Mpe11Zones, PitchBendSensitivity};
    Mpe11Zones::single(Mpe11Zone::lower(
        MemberCount::of(members),
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    ))
}

/// **An app chooses MPE 1.1 for one Group of a device output, and the device
/// hears the zone's Configuration Message**: it goes in the update after the
/// write, on the Group written and no other, and `None` sends the message again
/// with no Member Channels (M1-100 section 2.2.1).
#[test]
fn an_mpe11_zone_written_for_an_output_is_sent_as_its_configuration_message() {
    use tutti_session::Mpe11Target;
    let (mut s, fake) = session();
    plug(&fake, 1, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let port = output(&mut s, &synth);
    let _ = fake.take_sent(EndpointId::from_raw(1));
    let target = Mpe11Target::of(port.node(), MidiGroup::of(1));

    s.set(target.to(Some(lower_zone(7)))).expect("an output");
    assert_eq!(s.get(target), Ok(Some(lower_zone(7))));
    assert_eq!(
        fake.take_sent(EndpointId::from_raw(1)),
        [],
        "nothing before the update"
    );
    let _ = s.update();
    assert_eq!(
        fake.take_sent(EndpointId::from_raw(1)),
        [configuration(1, 7)]
    );

    s.set(target.to(None)).expect("an output");
    let _ = s.update();
    assert_eq!(
        fake.take_sent(EndpointId::from_raw(1)),
        [configuration(1, 0)],
        "the zone ends with no Member Channels"
    );
}

/// **The zone an app chose outlives the device**: a device that is unplugged and
/// plugged again is sent the zone in force, ahead of what the port said, and a
/// value written while it was away waits for it.
#[test]
fn an_mpe11_zone_is_sent_again_to_a_device_that_returns() {
    use tutti_session::Mpe11Target;
    let (mut s, fake) = session();
    plug(&fake, 1, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let port = output(&mut s, &synth);
    let first = Mpe11Target::of(port.node(), MidiGroup::FIRST);
    let second = Mpe11Target::of(port.node(), MidiGroup::of(2));
    s.set(first.to(Some(lower_zone(7)))).expect("an output");
    let _ = s.update();

    unplug(&fake, 1);
    let _ = s.update();
    s.set(second.to(Some(lower_zone(3)))).expect("an output");
    let _ = s.update();
    plug(&fake, 1, "Synth", PortDirection::Output);
    let _ = fake.take_sent(EndpointId::from_raw(1));
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(1));
    assert_eq!(
        sent,
        [configuration(0, 7), configuration(2, 3)],
        "each Group's zone in force, and the one written while away"
    );
}

/// **Only a MIDI output port of the session has the target**: a write that names
/// the node of an input port, or of a node that is no port, fails at the call.
#[test]
fn an_mpe11_write_for_a_node_that_is_not_an_output_fails_at_the_call() {
    use tutti_session::{Mpe11Target, ValueErrorKind};
    let (mut s, fake) = session();
    plug(&fake, 1, "Synth", PortDirection::Duplex);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    let keys = input(&mut s, &synth);
    let clip = s.graph().add(MidiClipNode::from(MidiClipConfig::default()));
    for node in [keys.node(), clip.erase()] {
        let target = Mpe11Target::of(node, MidiGroup::FIRST);
        assert_eq!(
            s.set(target.to(Some(lower_zone(7)))).unwrap_err().kind,
            ValueErrorKind::NoSuchTarget
        );
    }
    let port = output(&mut s, &synth);
    let node = port.node();
    drop(port);
    let _ = s.update();
    let _ = s.remove(node);
    let _ = s.update();
    assert_eq!(
        s.set(Mpe11Target::of(node, MidiGroup::FIRST).to(None))
            .unwrap_err()
            .kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// **A device has one output port**: a second open of a device a held output has
/// open is `InUse`, as an input's is, and the first port still sends.
#[test]
fn a_second_output_on_a_device_is_in_use_and_the_first_still_sends() {
    let (mut s, fake, out, mut keys, synth) = retiring_rig();
    let error = s
        .open_output(synth.clone())
        .expect_err("the device is held by an output");
    assert!(
        matches!(&error, MidiPortError::InUse(id) if *id == synth.id),
        "{error:?}"
    );
    assert_eq!(fake.connected_outputs().len(), 1, "no second connection");
    assert_eq!(out.presence(), Presence::Live);
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(64),
        NoteVelocity::new(MF),
    )
    .expect("queued");
    blocks(&mut s, 3);
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(sent.iter().any(|u| is_on(u, 64)), "{sent:?}");
}

/// **A restore gives a device to the first port in saved order**: two ports opened on
/// a device while it was away reopen when it returns, the first on it and the second
/// refused the way an input's failed reopen is, reading `ReopenFailed`.
#[test]
fn a_restore_of_two_outputs_on_one_device_opens_the_first_and_refuses_the_second() {
    let (mut s, fake) = session();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    let synth = device(&mut s, "Synth");
    unplug(&fake, 10);
    let _ = s.update();
    let first = s.open_output(synth.clone()).expect("opens, away");
    let second = s.open_output(synth).expect("opens, away");
    let _ = s.update();
    plug(&fake, 10, "Synth", PortDirection::Output);
    let _ = s.update();
    assert_eq!(first.presence(), Presence::Live);
    assert_eq!(
        second.presence(),
        Presence::Lost(PortCause::ReopenFailed),
        "the second is refused"
    );
    assert_eq!(fake.connected_outputs().len(), 1, "one connection");
}
