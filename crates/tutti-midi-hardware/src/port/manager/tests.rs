use super::*;
use crate::Delivered;
use crate::testing::{RATE, Rig, ports};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_units::Losses;

use tutti_fixtures::midi;
use tutti_midi_runtime::{InputInlets, InputWriters};
use tutti_ump::{Message, MidiChannel, Ump, Utility};

fn now() -> Instant {
    Instant::now()
}

fn note(n: u8) -> Ump {
    midi::note_on(MidiChannel::FIRST, n, 0x8000)
}

/// A rig over inlets of `capacity` events with `lead`, whose clock has read once
/// (frame 0 at `origin`), so an arrival can be placed.
fn rig(capacity: u32, lead: Duration) -> (Arc<HardwareMidiInputs>, Rig) {
    let inlets = InputInlets::new(tutti_graph::EventCapacity::of(capacity), lead);
    let manager = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let mut rig = Rig::over(&inlets, now());
    rig.run(1);
    (manager, rig)
}

#[test]
fn test_create_ports() {
    let manager = ports();

    let input_id = manager.open_input_port("Test Input").0.port();
    assert_eq!(input_id.get(), 1, "ids count from 1");

    let input_ports = manager.list_input_ports();
    assert_eq!(input_ports.len(), 1);
    let input_info = &input_ports[0];
    assert_eq!(input_info.name, "Test Input");
    assert!(input_info.active);
}

#[test]
fn test_list_ports() {
    let manager = ports();

    let id1 = manager.open_input_port("Input 1").0.port();
    let id2 = manager.open_input_port("Input 2").0.port();

    let inputs = manager.list_input_ports();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().any(|p| p.id == id1));
    assert!(inputs.iter().any(|p| p.id == id2));
}

#[test]
fn test_port_active_state() {
    let manager = ports();

    let port_id = manager.open_input_port("Test").0.port();
    assert!(manager.port_info(port_id).unwrap().active);

    manager.set_port_active(port_id, false).unwrap();
    assert!(!manager.port_info(port_id).unwrap().active);

    manager.set_port_active(port_id, true).unwrap();
    assert!(manager.port_info(port_id).unwrap().active);
}

/// **Every port is a new port, and there is no cap**: a hundred opens each make
/// a port with an id of its own, none refused.
#[test]
fn every_open_makes_a_port_of_its_own() {
    let manager = ports();
    let ids: Vec<_> = (0..100)
        .map(|i| manager.open_input_port(format!("P{i}")).0.port())
        .collect();
    assert_eq!(manager.list_input_ports().len(), 100);
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        100
    );
}

/// **An event reaches the graph on its frame**: pushed for an arrival 130
/// frames after the clock's reading, it is the graph's at frame 130 of the
/// graph input its inlet feeds, and the second port's on the other.
#[test]
fn a_ports_events_reach_the_graph_on_their_frames_and_inputs() {
    let (manager, mut rig) = rig(64, Duration::ZERO);
    let (mut a, inlet) = manager.open_input_port("A");
    rig.attach(inlet);
    let (mut b, inlet) = manager.open_input_port("B");
    rig.attach(inlet);
    assert_eq!(a.push(note(60), rig.time_of(130)), Ok(Delivered::Sent));
    assert_eq!(b.push(note(64), rig.time_of(200)), Ok(Delivered::Sent));
    rig.run(4);
    assert_eq!(rig.take(), vec![(0, 130, note(60)), (1, 200, note(64))]);
}

/// **The spacing of a device's events is the spacing of their arrivals**,
/// whatever the block boundaries:
/// arrivals at 130, 200 and 333 frames after the reading sound at those
/// frames plus the lead, the lead being the same for every event.
#[test]
fn arrivals_keep_their_spacing_and_the_lead_is_added_to_each() {
    let lead = Duration::from_secs_f64(100.0 / RATE);
    let (manager, mut rig) = rig(64, lead);
    let (mut port, inlet) = manager.open_input_port("A");
    rig.attach(inlet);
    for (n, frame) in [(60u8, 130u64), (61, 200), (62, 333)] {
        assert_eq!(port.push(note(n), rig.time_of(frame)), Ok(Delivered::Sent));
    }
    rig.run(8);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.1).collect();
    assert_eq!(frames, vec![230, 300, 433]);
}

/// **An event that arrives after its frame has begun is late, not lost**: it
/// sounds at the first frame of the next block and is counted
/// ([`Losses::late`](tutti_units::Losses::late)).
#[test]
fn an_arrival_past_its_block_is_emitted_late_and_counted() {
    let (manager, mut rig) = rig(64, Duration::ZERO);
    let (writers, readers) = InputWriters::new();
    let (mut port, inlet) = manager.open_input_port_with("A", writers.split().0);
    rig.attach(inlet);
    rig.run(3); // with the block `rig` ran, frames 0..256 have been rendered
    assert_eq!(port.push(note(60), rig.time_of(10)), Ok(Delivered::Sent));
    rig.run(1);
    assert_eq!(
        rig.take(),
        vec![(0, 256, note(60))],
        "the next block's first frame"
    );
    let losses = readers.inlet.get();
    assert_eq!(
        (losses.late, losses.lost, losses.evicted),
        (1, 0, 0),
        "late, not lost"
    );
}

/// **An inactive port discards what its driver pushes**: nothing reaches the
/// graph while it is paused, nothing arrives in a burst when it is active again,
/// and a discard is not a loss.
#[test]
fn an_inactive_port_discards_its_events() {
    let (manager, mut rig) = rig(64, Duration::ZERO);
    let (mut port, inlet) = manager.open_input_port("A");
    let id = port.port();
    rig.attach(inlet);

    manager.set_port_active(id, false).unwrap();
    assert_eq!(
        port.push(note(60), rig.time_of(100)),
        Ok(Delivered::Diverted),
        "discarded, not lost"
    );
    manager.set_port_active(id, true).unwrap();
    assert_eq!(port.push(note(61), rig.time_of(120)), Ok(Delivered::Sent));
    rig.run(4);
    assert_eq!(rig.take(), vec![(0, 120, note(61))]);
}

/// Concurrent creators each land their port: none is lost to another's open,
/// and the ids handed back are all distinct.
///
/// Hotplug can create ports from more than one control thread (a backend's
/// notification thread, a session's rescan).
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
fn concurrent_creators_all_land() {
    use std::collections::BTreeSet;
    use std::sync::Barrier;

    const THREADS: usize = 8;
    const PER_THREAD: usize = 50;
    let manager = ports();
    let start = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let manager = Arc::clone(&manager);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                (0..PER_THREAD)
                    .map(|i| {
                        let (handle, inlet) = manager.open_input_port(format!("{t}/{i}"));
                        (handle.port(), t, i, handle, inlet)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let created: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("creator thread"))
        .collect();

    let listed = manager.list_input_ports();
    assert_eq!(
        listed.len(),
        THREADS * PER_THREAD,
        "a creator's port was lost"
    );
    let ids: BTreeSet<InputId> = created.iter().map(|c| c.0).collect();
    assert_eq!(
        ids.len(),
        THREADS * PER_THREAD,
        "every creator was handed its own id"
    );
    for (id, t, i, ..) in &created {
        let info = manager.port_info(*id).expect("the creator's port");
        assert_eq!(
            info.name,
            format!("{t}/{i}"),
            "the id a creator was handed names its own port"
        );
    }
}

/// Hot-plugging does not grow the list: a port removed and opened again leaves
/// the list as large as the live ports, and a removed port is gone.
#[test]
fn hot_plug_cycles_do_not_grow_the_list() {
    let manager = ports();
    let (mut interior, _inlet) = manager.open_input_port("Interior");
    let _anchor = manager.open_input_port("Anchor");

    let mut gone = None;
    for cycle in 0..100 {
        let old = interior.port();
        drop(interior);
        assert!(
            manager.remove_input_port(old),
            "cycle {cycle}: the interior port is there"
        );
        interior = manager.open_input_port("Interior").0;
        assert_ne!(interior.port(), old, "cycle {cycle}: a new port, a new id");
        gone = Some(old);
        assert_eq!(manager.list_input_ports().len(), 2, "cycle {cycle}");
        assert_eq!(
            manager.port_count(),
            2,
            "cycle {cycle}: the list does not grow"
        );
    }
    let gone = gone.expect("the loop ran");
    assert!(!manager.remove_input_port(gone), "a removed port is gone");
    assert!(manager.port_info(gone).is_none());
}

/// A stale `InputId` cannot reach the port opened after it was removed: ids are
/// never reused.
///
/// Port A is removed and port B created. A's id must not remove, toggle or
/// describe B.
#[test]
fn a_stale_id_cannot_reach_the_port_opened_after_it() {
    let manager = ports();
    let a = manager.open_input_port("A").0.port();
    assert!(manager.remove_input_port(a));
    let (b, _inlet) = manager.open_input_port("B");
    let b_id = b.port();
    assert_ne!(b_id, a);

    assert_eq!(manager.set_port_active(a, false), Err(StaleInput(a)));
    assert_eq!(
        manager.set_input_timing(a, InputTiming::JitterReduction),
        Err(StaleInput(a))
    );
    assert!(manager.port_info(b_id).unwrap().active, "B is untouched");
    assert!(manager.port_info(a).is_none());
    assert!(!manager.remove_input_port(a), "a stale id removes nothing");

    let info = manager.port_info(b_id).expect("B is still there");
    assert_eq!((info.name.as_str(), info.active), ("B", true));
}

/// **A full inlet counts what it drops** in the input's shared tally, which the
/// removed port leaves as it was.
#[test]
fn a_full_inlet_counts_what_it_drops() {
    let (manager, mut rig) = rig(4, Duration::ZERO);
    let (writers, readers) = InputWriters::new();
    let (mut handle, inlet) = manager.open_input_port_with("Busy", writers.split().0);
    rig.attach(inlet);
    let id = handle.port();
    let taken = (0..10)
        .filter(|_| {
            handle.push(Ump::from(Message::Utility(Utility::Noop)), now()) == Ok(Delivered::Sent)
        })
        .count();
    assert_eq!(taken, 4);
    assert_eq!(readers.inlet.get(), Losses::lost(6));
    assert!(manager.remove_input_port(id));
    assert_eq!(
        readers.inlet.get(),
        Losses::lost(6),
        "a removed port's drops stay"
    );
}

/// **The stream drain is lazy**: it takes a message per step, so a caller that
/// stops after one leaves the rest queued for its next read, and an id that
/// names no port drains nothing.
#[test]
fn the_stream_drain_takes_a_message_per_step() {
    use tutti_ump::{DiscoveryFilter, Message, Stream};
    let manager = ports();
    let (mut port, _inlet) = manager.open_input_port("A");
    let slot = port.port();
    // Three distinct Stream messages: which of the filter's fields is asked.
    let stream = |which: usize| {
        Ump::from(Message::Stream(Stream::EndpointDiscovery {
            version: tutti_midi_device::UMP_VERSION,
            filter: DiscoveryFilter {
                endpoint_info: which == 0,
                device_identity: which == 1,
                endpoint_name: which == 2,
                product_instance_id: false,
                stream_configuration: false,
            },
        }))
    };
    let (a, b, c) = (stream(0), stream(1), stream(2));
    for ump in [a, b, c] {
        assert_eq!(port.push(ump, now()), Ok(Delivered::Diverted));
    }
    let mut drain = manager.drain_stream(slot);
    assert_eq!(drain.next(), Some(a));
    drop(drain);
    assert_eq!(
        manager.drain_stream(slot).collect::<Vec<_>>(),
        [b, c],
        "what was not read stays, in order"
    );
    assert_eq!(manager.drain_stream(slot).count(), 0);
    drop(port);
    assert!(manager.remove_input_port(slot));
    assert_eq!(
        manager.drain_stream(slot).count(),
        0,
        "a stale id drains nothing"
    );
}
