//! MIDI ports as pool entries: a `midi_input` or `midi_output` declares a saved
//! device, the session opens it, and its handle reads whether it is there. A
//! fake OS plugs and unplugs the devices.

use std::collections::BTreeSet;
use std::time::Instant;

use crate::common::{MF, Seen, blocks, note, play, tap};
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_midi_device::{DeviceInfo, EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint, persistent_endpoint};
use tutti_midi_runtime::MidiClipNode;
use tutti_session::{
    Declared, EventInPort, HandleError, LoadError, MidiInputEntry, MidiPortError, MidiRouteRule,
    Peer, PoolEvent, PoolState, PortCause, Presence, Resource, ResourceFact, Session, SessionEvent,
    midi_input, midi_output,
};
use tutti_types::{Beat, BeatDuration, SampleRate};
use tutti_ump::{Address, CCNumber, KeyEvent, Message, MidiChannel, MidiGroup, U7, Voice2};

const RATE: f64 = 48_000.0;

/// A session over a fake OS with no devices, and the handle that plugs some.
fn session() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareOptions::new())
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

type Keys = &'static str;

/// Declares the input ports and the output ports, whole.
fn declare(s: &mut Session, inputs: &[(Keys, &DeviceInfo)], outputs: &[(Keys, &DeviceInfo)]) {
    let inputs: Vec<_> = inputs
        .iter()
        .map(|&(key, info)| midi_input(key, info.clone()))
        .collect();
    let outputs: Vec<_> = outputs
        .iter()
        .map(|&(key, info)| midi_output(key, info.clone()))
        .collect();
    s.pool((inputs, outputs)).expect("declares");
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

fn heard(seen: &Seen, wanted: fn(KeyEvent) -> Option<u8>) -> Vec<u8> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|&(_, e)| e.key_event().and_then(wanted))
        .collect()
}

fn note_ons(seen: &Seen) -> Vec<u8> {
    heard(seen, |e| match e {
        KeyEvent::On(k) => Some(k.note().get()),
        _ => None,
    })
}

fn note_offs(seen: &Seen) -> Vec<u8> {
    heard(seen, |e| match e {
        KeyEvent::Off(k) => Some(k.note().get()),
        _ => None,
    })
}

/// **A saved device that is absent loads, and reads `Lost(Unplugged)`**: a port
/// needs no device to exist.
#[test]
fn an_absent_device_loads_lost_unplugged() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    unplug(&fake, 1);
    let _ = s.update();

    declare(&mut s, &[("keys", &keys)], &[]);
    let events = s.update();
    assert!(matches!(s.pool_state(&"keys"), Some(PoolState::Loaded)));
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Pool {
                event: PoolEvent::Loaded,
                ..
            }
        )),
        "the load landed: {events:?}"
    );
    let port = s.midi_input(&"keys").expect("the port is there");
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert_eq!(port.info().device(), &keys, "it reads the saved device");
}

/// **A device that is unplugged and plugged again goes `Lost` and `Live` on the
/// same key**: the handle read before follows it, the pool entry stays loaded
/// throughout, each change is one `ResourceChanged` of presence, and the same
/// ingest node serves the port.
#[test]
fn a_replug_goes_live_on_the_same_key_and_node() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    let node = port.node();
    assert_eq!(port.presence(), Presence::Live);

    unplug(&fake, 1);
    let events = s.update();
    assert_eq!(changed(&events, port.id()), [ResourceFact::Presence]);
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    assert!(matches!(s.pool_state(&"keys"), Some(PoolState::Loaded)));
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
    let again = s.midi_input(&"keys").expect("still loaded");
    assert_eq!(again.id(), port.id(), "the same port, not a new one");
    assert_eq!(again.node(), node, "wired to the same ingest node");
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
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    assert_eq!(
        port.presence(),
        Presence::Live,
        "the only one of its name is bound at load"
    );

    unplug(&fake, 1);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
}

/// **Two devices of the saved name returning stay Lost**: either could be the
/// saved one, which is the load's ambiguity too, so nothing is bound unasked.
#[test]
fn two_returning_devices_of_the_saved_name_leave_the_port_lost() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
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
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
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

/// **A port whose device is held by another port fails its load `InUse`**: the
/// second entry on one device cannot open it, and names it.
#[test]
fn a_second_port_on_one_device_fails_in_use() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    declare(&mut s, &[("first", &keys), ("second", &keys)], &[]);
    let _ = s.update();
    assert_eq!(s.midi_input(&"first").unwrap().presence(), Presence::Live);
    let Some(PoolState::Failed(LoadError::Midi(error))) = s.pool_state(&"second") else {
        panic!("the second port failed: {:?}", s.pool_state(&"second"));
    };
    assert!(
        matches!(&*error, MidiPortError::InUse(id) if *id == keys.id),
        "{error:?}"
    );
    assert_eq!(
        fake.connected_inputs().len(),
        1,
        "the failed load left no port"
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
    declare(&mut s, &[("keys", &keys)], &[("synth", &synth)]);
    let _ = s.update();
    let (input, output) = (
        s.midi_input(&"keys").unwrap(),
        s.midi_output(&"synth").unwrap(),
    );
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
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").unwrap();

    fake.kill_input_reader(EndpointId::from_raw(1));
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live, "reopened at once");
    assert!(!fake.connected_inputs().is_empty());

    fake.refuse_input_opens(true);
    fake.kill_input_reader(EndpointId::from_raw(1));
    let events = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::ReaderFailed));
    assert_eq!(changed(&events, port.id()), [ResourceFact::Presence]);

    fake.refuse_input_opens(false);
    unplug(&fake, 1);
    let _ = s.update();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Live);
}

/// **An unrouted port is processed, settles and leaves**: no synth is routed to
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
        declare(&mut s, &[("keys", &keys)], &[]);
        let _ = s.update();
        blocks(&s, 1);
        nodes.push(s.midi_input(&"keys").unwrap().node());
        s.pool(Vec::<Declared<Keys, MidiInputEntry>>::new())
            .expect("declares nothing");
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

/// **An ambiguous saved device fails the load with its candidates, and `retry`
/// loads it again**: two present devices of one name could be the saved one;
/// once one leaves, the retry binds the other.
#[test]
fn an_ambiguous_match_fails_with_candidates_and_retry_loads_it_again() {
    let (mut s, fake) = session();
    fake.plug(endpoint(1, "Keys"), PortDirection::Input);
    fake.plug(endpoint(2, "Keys"), PortDirection::Input);
    let _ = s.update();
    let saved = s.midi().devices().remove(0);

    declare(&mut s, &[("keys", &saved)], &[]);
    let events = s.update();
    let Some(PoolState::Failed(LoadError::Midi(error))) = s.pool_state(&"keys") else {
        panic!("the load failed: {:?}", s.pool_state(&"keys"));
    };
    let MidiPortError::Ambiguous(candidates) = &*error else {
        panic!("ambiguous, got {error:?}");
    };
    assert_eq!(candidates.len(), 2, "both devices are candidates");
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Pool {
                event: PoolEvent::Failed(_),
                ..
            }
        )),
        "the failure is a pool event: {events:?}"
    );
    assert!(matches!(
        s.midi_input(&"keys"),
        Err(HandleError::NotLoaded(PoolState::Failed(_)))
    ));

    unplug(&fake, 2);
    let _ = s.update();
    s.retry(&"keys").expect("a failed entry retries");
    let events = s.update();
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::Pool {
            event: PoolEvent::Loaded,
            ..
        }
    )));
    let port = s.midi_input(&"keys").expect("loaded after the retry");
    assert_eq!(port.presence(), Presence::Live);
}

/// **A session without the OS MIDI layer fails the load with a named error.**
#[test]
fn a_session_without_the_hardware_layer_fails_the_load_by_name() {
    let (mut with, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = with.update();
    let keys = device(&mut with, "Keys");

    let mut s = Session::builder().build().expect("builds");
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let Some(PoolState::Failed(LoadError::Midi(error))) = s.pool_state(&"keys") else {
        panic!("the load failed: {:?}", s.pool_state(&"keys"));
    };
    assert!(matches!(&*error, MidiPortError::NoHardware), "{error:?}");
}

/// **A released port reads `Closed`**: once the pool no longer names the entry,
/// its handle reads `Closed`, the key is not pooled, and the device's
/// connection and ingest node go with it.
#[test]
fn a_released_handle_reads_closed_and_the_port_goes() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let keys = device(&mut s, "Keys");
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    let node = port.node();
    assert!(s.graph_ref().contains(node));

    s.pool(Vec::<Declared<Keys, MidiInputEntry>>::new())
        .expect("declares nothing");
    let events = s.update();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Pool {
                event: PoolEvent::Released,
                ..
            }
        )),
        "{events:?}"
    );
    assert_eq!(port.presence(), Presence::Closed);
    assert!(matches!(
        s.midi_input(&"keys"),
        Err(HandleError::NotPooled("keys"))
    ));
    assert!(fake.connected_inputs().is_empty());
    // The node leaves once it has released what its device held: after a block.
    blocks(&s, 2);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the ingest node went");
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
/// and the entry's removal ends them with a note off, a pedal up and a centred
/// bend, before the ingest node is taken out. (The executor's close of the
/// inlet's edge ends the keys and the pedal; the bend is the ingest node's own
/// release alone.)
#[test]
fn held_notes_are_released_when_the_port_is_removed() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let keys = device(&mut s, "Keys");
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
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
    let node = s.midi_input(&"keys").expect("loaded").node();

    s.pool(Vec::<Declared<Keys, MidiInputEntry>>::new())
        .expect("declares nothing");
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
    declare(&mut s, &[("keys", &keys)], &[]);
    let _ = s.update();
    let node = s.midi_input(&"keys").unwrap().node();
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
}

/// **Every input port has an ingest node of its own, and there is no cap**: ten
/// devices (the fixed ingest took eight) are ten live ports on ten nodes, and
/// each device's note reaches a synth routed to all of them.
#[test]
fn every_input_port_has_its_own_ingest_node() {
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
    let keys: Vec<(Keys, &DeviceInfo)> = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]
        .into_iter()
        .zip(&devices)
        .collect();
    declare(&mut s, &keys, &[]);
    let _ = s.update();
    roll(&s, 1_024);

    let ports: Vec<_> = keys
        .iter()
        .map(|(k, _)| s.midi_input(k).expect("loaded"))
        .collect();
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
    declare(&mut s, &[("keys", &keys), ("pads", &pads)], &[]);
    let _ = s.update();
    unplug(&fake, 2);
    let _ = s.update();
    assert_eq!(s.midi_input(&"keys").unwrap().presence(), Presence::Live);
    assert_eq!(
        s.midi_input(&"pads").unwrap().presence(),
        Presence::Lost(PortCause::Unplugged)
    );
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
    declare(&mut s, &[("wire", &wire)], &[]);
    let _ = s.update();
    let port = s.midi_input(&"wire").expect("loaded");
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
    declare(&mut s, &[], &[("synth", &synth)]);
    let _ = s.update();
    let out = s.midi_output(&"synth").expect("loaded");
    assert_eq!(out.presence(), Presence::Live);

    let (clip, _) = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat(0.0),
        BeatDuration(1.0),
    )));
    s.graph()
        .set_sources(EventInPort::new(out.node(), 0), [clip.events_out()])
        .expect("the port's node takes events");
    let _ = s.update();
    roll(&s, 1_024);
    let _ = s.update();
    let sent = fake.take_sent(EndpointId::from_raw(10));
    assert!(
        sent.iter()
            .any(|u| matches!(u.key_event(), Some(KeyEvent::On(k)) if k.note().get() == 60)),
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
    declare(&mut s, &[], &[("synth", &synth)]);
    let _ = s.update();
    let out = s.midi_output(&"synth").expect("loaded");
    s.midi_nodes().unwrap().clock_master.set_enabled(true);
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
    let before = s.midi().out_drops();
    blocks(&s, 8);
    let _ = s.update();
    assert!(
        s.midi().out_drops() > before,
        "events with no device are dropped"
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

/// **A port is a peer**: its token's id is the one in the events about it, and
/// two ports have two.
#[test]
fn a_ports_id_names_it_in_the_events() {
    let (mut s, fake) = session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    plug(&fake, 2, "Pads", PortDirection::Input);
    let _ = s.update();
    let (keys, pads) = (device(&mut s, "Keys"), device(&mut s, "Pads"));
    declare(&mut s, &[("keys", &keys), ("pads", &pads)], &[]);
    let _ = s.update();
    let (a, b) = (
        s.midi_input(&"keys").unwrap(),
        s.midi_input(&"pads").unwrap(),
    );
    assert_ne!(a.id(), b.id());
    unplug(&fake, 1);
    let events = s.update();
    assert_eq!(changed(&events, a.id()), [ResourceFact::Presence]);
    assert!(changed(&events, b.id()).is_empty(), "only Keys went");
}
