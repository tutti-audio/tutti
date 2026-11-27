//! A virtual device against the service: a second session's connection is the peer another
//! application would be.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use tutti_ump::{
    BlockFilter, BlockSelect, DeviceIdentity, EndpointInfoNotification, EndpointName,
    FunctionBlockCount, FunctionBlockInfo, FunctionBlocks, ManufacturerId, Message,
    OsFunctionBlock, SoftwareRevision, Stream, Sysex8StreamCount, U7, UiHint, Ump, UmpVersion,
    read_function_block,
};
use tutti_winmidi2_ffi::{
    Block, Change, Connection, Endpoint, Session, VirtualDevice, VirtualDeviceSpec, Watcher,
    endpoints, ensure_service_available,
};

/// Only a hang guard: it bounds a wait for an event that never comes, and is not a speed.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// The blocks declared: block 0 an output over Groups 2 and 3, block 1 a bidirectional
/// MIDI 1.0 port over Group 5, both named.
fn blocks() -> Vec<Block> {
    let block = |os: OsFunctionBlock, name: &str| Block {
        info: read_function_block(os),
        name: Some(name.to_owned()),
    };
    let base = OsFunctionBlock {
        active: true,
        id: 0,
        direction: 2,
        ui_hint: UiHint::Sender,
        midi1: 0,
        first_group: 1,
        groups_spanned: 2,
        midi_ci_version: 0,
        max_sysex8_streams: Sysex8StreamCount::new(0),
    };
    vec![
        block(base, "Out"),
        block(
            OsFunctionBlock {
                id: 1,
                direction: 3,
                ui_hint: UiHint::Both,
                midi1: 1,
                first_group: 4,
                groups_spanned: 1,
                ..base
            },
            "Both",
        ),
    ]
}

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::MIDI2_NON_COMMERCIAL,
        family: [U7::masked(1), U7::masked(2)],
        model: [U7::masked(3), U7::masked(4)],
        revision: SoftwareRevision::from([
            U7::masked(0),
            U7::masked(1),
            U7::masked(2),
            U7::masked(3),
        ]),
    }
}

/// A device published under a name of this test's own, with the app's side received.
struct Published {
    device: VirtualDevice,
    name: String,
    declared: Vec<Block>,
    /// What peers sent to the endpoint.
    received: Receiver<Ump>,
    _session: Session,
}

fn publish(tag: &str) -> Published {
    ensure_service_available().expect("the MIDI service is available");
    let name = format!("tutti-ffi-virtual-{}-{tag}", std::process::id());
    let declared = blocks();
    let info = EndpointInfoNotification {
        version: UmpVersion { major: 1, minor: 1 },
        static_function_blocks: true,
        function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(declared.len())),
        midi2: true,
        midi1: false,
        rx_jr: false,
        tx_jr: false,
    };
    let session = Session::create("tutti-winmidi2-ffi virtual device").expect("a session");
    let (tx, received) = channel();
    let device = session
        .publish(
            &VirtualDeviceSpec {
                name: &EndpointName::new(&name).expect("a name"),
                manufacturer: "tutti",
                description: "A test device",
                identity: &identity(),
                instance: None,
                info: &info,
                blocks: &declared,
            },
            move |received| {
                // The test may have ended: a send to a dropped receiver is the end.
                if let Ok((_, ump)) = received {
                    let _ = tx.send(ump);
                }
            },
        )
        .expect("the service publishes the device");
    Published {
        device,
        name,
        declared,
        received,
        _session: session,
    }
}

/// The endpoint of `name` the service lists, once it does: a watcher reports it when it
/// appears.
fn listed(name: &str) -> (Endpoint, Watcher, Receiver<Change>) {
    let (tx, changes) = channel();
    let watcher = Watcher::start(move |change| {
        if let Ok(change) = change {
            let _ = tx.send(change);
        }
    })
    .expect("the watcher starts");
    loop {
        match changes
            .recv_timeout(HANG_GUARD)
            .unwrap_or_else(|e| panic!("{name} is never listed: {e}"))
        {
            Change::Added(endpoint) | Change::Updated(endpoint) if endpoint.name == name => {
                return (endpoint, watcher, changes);
            }
            _ => {}
        }
    }
}

/// A peer's connection to `endpoint` and what arrives on it.
fn peer(session: &Session, endpoint: &Endpoint) -> (Connection, Receiver<Ump>) {
    let (tx, arrived) = channel();
    let connection = session
        .connect(&endpoint.id, move |received| {
            if let Ok((_, ump)) = received {
                let _ = tx.send(ump);
            }
        })
        .expect("a connection to the published endpoint");
    (connection, arrived)
}

/// One run of notes that differ in every word.
fn notes() -> Vec<Ump> {
    (0..16)
        .map(|i| Ump([0x4091_0000 | (i << 8), 0xC000_0000 | i, 0, 0]))
        .collect()
}

/// The first `want` of `sent` that arrive on `arrived`, in the order they do: the service's
/// own messages among them are not notes.
fn hear(arrived: &Receiver<Ump>, sent: &[Ump]) -> Vec<Ump> {
    let mut heard = Vec::new();
    while heard.len() < sent.len() {
        let ump = arrived
            .recv_timeout(HANG_GUARD)
            .unwrap_or_else(|e| panic!("{} of {} notes arrived: {e}", heard.len(), sent.len()));
        if sent.contains(&ump) {
            heard.push(ump);
        }
    }
    heard
}

/// **The service lists the published endpoint once, under the name it was declared with.**
#[test]
fn a_published_endpoint_is_listed_once_under_its_name() {
    let published = publish("listed");
    let (endpoint, _watcher, _changes) = listed(&published.name);
    assert!(!endpoint.id.is_empty());
    let listed = endpoints().expect("the service lists its endpoints");
    let same: Vec<&Endpoint> = listed.iter().filter(|e| e.name == published.name).collect();
    assert_eq!(same.len(), 1, "{listed:?}");
}

/// **A peer's Function Block Discovery is answered with the blocks as declared** (M2-104
/// §7.1.9): each Function Block Info Notification is the declared block's fields, and each
/// Function Block Name Notification its name, under the number declared.
#[test]
fn a_peer_discovers_the_function_blocks_as_declared() {
    let published = publish("blocks");
    let (endpoint, _watcher, _changes) = listed(&published.name);
    let session = Session::create("tutti-winmidi2-ffi peer").expect("a session");
    let (peer, arrived) = peer(&session, &endpoint);
    peer.send(
        None,
        &Message::Stream(Stream::FunctionBlockDiscovery {
            block: BlockSelect::All,
            filter: BlockFilter {
                info: true,
                name: true,
            },
        })
        .into(),
    )
    .expect("the service takes the discovery");

    let mut infos: BTreeMap<u8, FunctionBlockInfo> = BTreeMap::new();
    let mut names: BTreeMap<u8, String> = BTreeMap::new();
    while infos.len() < published.declared.len() || names.len() < published.declared.len() {
        let ump = arrived.recv_timeout(HANG_GUARD).unwrap_or_else(|e| {
            panic!(
                "{infos:?} and {names:?} are the blocks heard of {:?}: {e}",
                published.declared
            )
        });
        match ump.decode() {
            Ok(Message::Stream(Stream::FunctionBlockInfo(info))) => {
                if let Some(id) = info.id.value() {
                    infos.insert(id.get(), info);
                }
            }
            Ok(Message::Stream(Stream::FunctionBlockName { block, bytes, .. })) => {
                if let Some(id) = block.value() {
                    let text = bytes.iter().take_while(|b| **b != 0).copied().collect();
                    names.insert(id.get(), String::from_utf8(text).expect("a UTF-8 name"));
                }
            }
            _ => {}
        }
    }
    for declared in &published.declared {
        let number = declared.info.id.value().expect("a number").get();
        assert_eq!(infos.get(&number), Some(&declared.info), "block {number}");
        assert_eq!(names.get(&number), declared.name.as_ref(), "block {number}");
    }
}

/// **Every word a peer sends to the endpoint reaches the app's handler, and every word the
/// app sends reaches the peer, unchanged and in order.**
#[test]
fn words_cross_the_endpoint_both_ways_unchanged_and_in_order() {
    let published = publish("words");
    let (endpoint, _watcher, _changes) = listed(&published.name);
    let session = Session::create("tutti-winmidi2-ffi peer").expect("a session");
    let (peer, arrived) = peer(&session, &endpoint);
    let sent = notes();
    for ump in &sent {
        peer.send(None, ump).expect("the service takes the note");
    }
    assert_eq!(hear(&published.received, &sent), sent);
    for ump in &sent {
        published
            .device
            .send(ump)
            .expect("the service takes the note");
    }
    assert_eq!(hear(&arrived, &sent), sent);
}

/// **Dropping the device removes its endpoint from the service's list.**
#[test]
fn dropping_the_device_removes_its_endpoint() {
    let published = publish("dropped");
    let (endpoint, _watcher, changes) = listed(&published.name);
    drop(published.device);
    loop {
        match changes
            .recv_timeout(HANG_GUARD)
            .unwrap_or_else(|e| panic!("{} is never removed: {e}", published.name))
        {
            Change::Removed(id) if id.eq_ignore_ascii_case(&endpoint.id) => break,
            _ => {}
        }
    }
}
