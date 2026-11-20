//! The cases, on macOS.

use std::sync::mpsc;
use std::time::Duration;
use tutti_coremidi_ffi::{
    BlockRefusal, Error, InputPort, OutputPort, SharedClient, UmpEndpoint, UmpEndpointInfo,
    find_destination, find_source, ump_endpoints,
};
use tutti_platform::MainThreadMarker;
use tutti_ump::{
    BlockDirection, DeviceIdentity, FirstGroup, FunctionBlockCount, FunctionBlockInfo,
    FunctionBlocks, GroupSpan, ManufacturerId, Message, Midi1Port, OsFunctionBlock,
    ProductInstanceId, Protocol, SoftwareRevision, U7, UiHint, Ump, Utility, read_function_block,
};

const KEYS: &str = "tutti-ffi-test Keys";
const PORT: &str = "tutti-ffi-test Port";
const INSTANCE: &str = "tutti-ffi-test-1";

fn instance(text: &str) -> ProductInstanceId {
    ProductInstanceId::new(text).expect("printable ASCII, at most 42 bytes")
}

/// A bound that only guards a hang: each message arrives on CoreMIDI's thread.
const HANG_GUARD: Duration = Duration::from_secs(10);

fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("a harness = false target runs on the main thread")
}

fn block(os: OsFunctionBlock) -> FunctionBlockInfo {
    read_function_block(os)
}

fn base(id: u8) -> OsFunctionBlock {
    OsFunctionBlock {
        active: true,
        id,
        direction: 3,
        ui_hint: UiHint::Both,
        midi1: 0,
        first_group: 0,
        groups_spanned: 1,
        midi_ci_version: 0,
        max_sysex8_streams: 0,
    }
}

fn keys() -> FunctionBlockInfo {
    block(OsFunctionBlock {
        groups_spanned: 2,
        max_sysex8_streams: 3,
        ..base(0)
    })
}

fn port() -> FunctionBlockInfo {
    block(OsFunctionBlock {
        direction: 1,
        ui_hint: UiHint::Sender,
        midi1: 2,
        first_group: 5,
        ..base(1)
    })
}

/// The non-commercial manufacturer id (M2-104 §7.10) with a family, model and
/// revision whose bytes differ, so a swapped byte shows.
fn identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::NON_COMMERCIAL,
        family: [U7::of(1), U7::of(2)],
        model: [U7::of(3), U7::of(4)],
        revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
    }
}

fn is_ours(endpoint: &UmpEndpointInfo) -> bool {
    endpoint
        .blocks
        .iter()
        .any(|b| b.name.as_deref() == Some(KEYS))
}

fn jr() -> Ump {
    Ump::from(Message::Utility(Utility::JrTimestamp(0x1234)))
}

/// Creates the endpoint of the cases: `Some` on macOS 15 or later, where it must
/// be created, and `None` before it, where it must be refused by name.
fn endpoint(on_ump: impl FnMut(Option<Duration>, Ump) + Send + 'static) -> Option<UmpEndpoint> {
    let major = tutti_fixtures::macos_major().expect("sw_vers names the macOS version");
    let created = UmpEndpoint::new(
        main_thread(),
        "tutti-ffi-test-endpoint",
        Protocol::Midi2,
        &identity(),
        Some(&instance(INSTANCE)),
        &[(keys(), Some(KEYS)), (port(), Some(PORT))],
        on_ump,
    );
    if major >= 15 {
        Some(created.expect("macOS 15 creates the endpoint"))
    } else {
        assert_eq!(created.err(), Some(Error::UmpEndpointUnsupported));
        None
    }
}

fn listed() -> UmpEndpointInfo {
    ump_endpoints()
        .expect("CoreMIDI lists the endpoints")
        .into_iter()
        .find(is_ours)
        .expect("the created endpoint is listed")
}

/// **A block as CoreMIDI lists it is the block that was created** (M2-104
/// §7.1.8, §7.1.6.2): the endpoint is in the Protocol it was created in, its
/// blocks are static and counted, and each block's Groups, direction, UI hint,
/// MIDI 1.0 port field and SysEx 8 streams are the ones given. The endpoint
/// carries the Device Identity and Product Instance Id it was created with
/// (§7.1.3, §7.1.5).
pub fn an_endpoint_lists_the_blocks_it_was_created_with() {
    let Some(endpoint) = endpoint(|_, _| {}) else {
        return;
    };
    let ours = listed();
    assert_eq!(ours.protocol, Protocol::Midi2);
    assert!(ours.offered.static_function_blocks);
    assert_eq!(
        ours.offered.function_blocks,
        FunctionBlocks::Count(FunctionBlockCount::of(2))
    );
    assert!(ours.source_unique_id.is_some() && ours.destination_unique_id.is_some());
    assert_eq!(ours.identity, Some(identity()));
    assert_eq!(ours.instance.as_deref(), Some(INSTANCE));
    let named = |name: &str| {
        ours.blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(name))
            .expect("the block is listed")
            .info
    };
    let groups = |info: FunctionBlockInfo| {
        (
            info.first_group.value().map(FirstGroup::get),
            info.groups_spanned.value().map(GroupSpan::get),
        )
    };
    let keys = named(KEYS);
    assert_eq!(keys.direction, BlockDirection::Bidirectional);
    assert_eq!(groups(keys), (Some(0), Some(2)));
    assert_eq!(keys.midi1, Midi1Port::No);
    assert_eq!(
        (keys.ui_hint, keys.max_sysex8_streams, keys.active),
        (UiHint::Both, 3, true)
    );
    let port = named(PORT);
    assert_eq!(port.direction, BlockDirection::Input);
    assert_eq!(groups(port), (Some(5), Some(1)));
    assert_eq!(port.midi1, Midi1Port::Restricted);
    assert_eq!(port.ui_hint, UiHint::Sender);
    drop(endpoint);
}

/// **The numbers `blocks` reports are the numbers CoreMIDI lists**: the first
/// block's absolute number, then the second's, in the order registered.
pub fn the_block_numbers_read_back_are_the_numbers_coremidi_lists() {
    let Some(endpoint) = endpoint(|_, _| {}) else {
        return;
    };
    let ours = listed();
    let listed_number = |name: &str| {
        ours.blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(name))
            .and_then(|b| b.info.id.value())
    };
    assert_eq!(
        [listed_number(KEYS), listed_number(PORT)],
        [Some(endpoint.blocks()[0]), Some(endpoint.blocks()[1])]
    );
    assert_ne!(endpoint.blocks()[0], endpoint.blocks()[1]);
}

/// A block that declares a MIDI-CI version is refused by name before any
/// endpoint exists: `MIDIUMPMutableFunctionBlock` has no such field.
pub fn a_block_declaring_a_midi_ci_version_is_refused_by_name() {
    let ci = block(OsFunctionBlock {
        midi_ci_version: 2,
        ..base(0)
    });
    let refused = UmpEndpoint::new(
        main_thread(),
        "tutti-ffi-test-ci",
        Protocol::Midi2,
        &identity(),
        Some(&instance(INSTANCE)),
        &[(ci, None)],
        |_, _| {},
    );
    assert_eq!(
        refused.err(),
        Some(Error::Block(BlockRefusal::MidiCiVersion {
            id: 0,
            version: 2
        }))
    );
}

/// **The longest Product Instance Id, 42 characters, is listed back intact**
/// (M2-104 §7.1.5): `ProductInstanceId` makes a longer one unwritable, and
/// CoreMIDI accepts the longest.
pub fn a_42_character_instance_id_is_listed_back_intact() {
    let longest = "x".repeat(42);
    let major = tutti_fixtures::macos_major().expect("sw_vers names the macOS version");
    let created = UmpEndpoint::new(
        main_thread(),
        "tutti-ffi-test-instance",
        Protocol::Midi2,
        &identity(),
        Some(&instance(&longest)),
        &[(keys(), Some(KEYS))],
        |_, _| {},
    );
    if major < 15 {
        assert_eq!(created.err(), Some(Error::UmpEndpointUnsupported));
        return;
    }
    let _endpoint = created.expect("macOS 15 accepts 42 characters");
    assert_eq!(listed().instance.as_deref(), Some(longest.as_str()));
}

/// What an output port sends to the endpoint's destination reaches the callback
/// it was created with, as it does for a bare virtual destination (M2-104 §3:
/// a UMP Endpoint is a source and a destination). CoreMIDI sends the endpoint
/// messages of its own first (MIDI-CI Discovery), so the callback is read until
/// the one sent arrives.
pub fn an_endpoint_receives_what_is_sent_to_its_destination() {
    let (tx, rx) = mpsc::channel();
    let Some(endpoint) = endpoint(move |_, ump| {
        let _ = tx.send(ump);
    }) else {
        return;
    };
    let destination = find_destination(
        listed()
            .destination_unique_id
            .expect("the endpoint has a destination"),
    )
    .expect("the destination is listed");
    let client = SharedClient::start("tutti-ffi-test", |_| {}).expect("CoreMIDI client");
    let out = OutputPort::new(client, "tutti-ffi-test-out").expect("an output port");
    out.send(&destination, Protocol::Midi2, &[(None, jr())])
        .expect("the destination accepts the list");
    while rx
        .recv_timeout(HANG_GUARD)
        .expect("the message sent to the destination arrives")
        != jr()
    {}
    assert_eq!(endpoint.faults(), 0);
}

/// What the sender publishes reaches an input port connected to the endpoint's
/// source. CoreMIDI sends messages of its own first, so the port is read until
/// the one sent arrives.
pub fn what_the_sender_publishes_reaches_a_port_connected_to_the_source() {
    let Some(endpoint) = endpoint(|_, _| {}) else {
        return;
    };
    let source = find_source(
        listed()
            .source_unique_id
            .expect("the endpoint has a source"),
    )
    .expect("the source is listed");
    let client = SharedClient::start("tutti-ffi-test", |_| {}).expect("CoreMIDI client");
    let (tx, rx) = mpsc::channel();
    let _port = InputPort::connect(
        client,
        "tutti-ffi-test-in",
        Protocol::Midi2,
        &source,
        || (),
        move |(), _, ump| {
            let _ = tx.send(ump);
        },
    )
    .expect("an input port connected to the source");
    endpoint
        .sender()
        .send(&jr())
        .expect("the source accepts the send");
    while rx
        .recv_timeout(HANG_GUARD)
        .expect("the message published on the source arrives")
        != jr()
    {}
}

/// A sender on another thread sees the endpoint go by name: it sends until the
/// drop, every send after the drop returned is `EndpointGone`, and none is any other
/// error. (A send racing the dispose is excluded by the sender's lock; it cannot be
/// observed, since CoreMIDI accepts a send on a disposed reference silently.)
pub fn a_sender_on_another_thread_sees_the_endpoint_go_by_name() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(endpoint) = endpoint(|_, _| {}) else {
        return;
    };
    let sender = endpoint.sender();
    let dropped = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = {
        let dropped = Arc::clone(&dropped);
        std::thread::spawn(move || {
            let mut started = false;
            loop {
                let after_drop = dropped.load(Ordering::SeqCst);
                let sent = sender.send(&jr());
                if !started {
                    started = true;
                    let _ = started_tx.send(());
                }
                match sent {
                    Ok(()) => assert!(!after_drop, "a send after the drop was accepted"),
                    Err(Error::EndpointGone) => break,
                    Err(other) => panic!("a send failed with {other}"),
                }
            }
            let _ = done_tx.send(());
        })
    };
    started_rx
        .recv_timeout(HANG_GUARD)
        .expect("the sender sends before the drop");
    drop(endpoint);
    dropped.store(true, Ordering::SeqCst);
    done_rx
        .recv_timeout(HANG_GUARD)
        .expect("the sender sees the endpoint gone");
    worker.join().expect("the sender thread ends cleanly");
}

/// A send on a sender whose endpoint was dropped is refused by name, and its
/// counters still read.
pub fn a_sender_outlives_its_endpoint_and_refuses_by_name() {
    let Some(endpoint) = endpoint(|_, _| {}) else {
        return;
    };
    let sender = endpoint.sender();
    assert_eq!(sender.send(&jr()), Ok(()));
    drop(endpoint);
    assert_eq!(sender.send(&jr()), Err(Error::EndpointGone));
    assert_eq!((sender.faults(), sender.truncated()), (0, 0));
}

/// Runs every case on the main thread, as `harness = false` does.
pub fn cases() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        an_endpoint_lists_the_blocks_it_was_created_with,
        the_block_numbers_read_back_are_the_numbers_coremidi_lists,
        a_block_declaring_a_midi_ci_version_is_refused_by_name,
        a_42_character_instance_id_is_listed_back_intact,
        an_endpoint_receives_what_is_sent_to_its_destination,
        what_the_sender_publishes_reaches_a_port_connected_to_the_source,
        a_sender_on_another_thread_sees_the_endpoint_go_by_name,
        a_sender_outlives_its_endpoint_and_refuses_by_name,
    ]);
}
