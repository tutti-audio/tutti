//! The cases, on macOS.

use std::time::Duration;
use tutti_coremidi_ffi::{BlockRefusal, Error, ump_endpoints};
use tutti_fixtures::unique_name;
use tutti_graph::{EventCapacity, EventInletNode, Offset};
use tutti_midi_device::FunctionBlock;
use tutti_midi_hardware::{
    MidiHardwareConfig, MidiHardwareError, MidiSession, VirtualEndpoint, VirtualSpec,
};
use tutti_midi_runtime::InputInlets;
use tutti_platform::{Executor, MainThreadMarker};
use tutti_ump::{BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port, UiHint, Ump};
use tutti_ump::{BlockName, EndpointName};

#[path = "../input/listener.rs"]
#[allow(
    dead_code,
    reason = "the listener is shared with the input tests, which use more of it"
)]
mod listener;
use listener::Listener;
#[path = "../input/opening.rs"]
mod opening;

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);
const INSTANCE: &str = "tutti-test-instance";

/// `session` once its backend has started: CoreMIDI's client is made on a thread of
/// the backend's own, so a port is opened with the proof that it resolved.
fn started(session: MidiSession) -> MidiSession {
    tutti_fixtures::block_on(session.start()).expect("the backend starts");
    session
}

fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("a harness = false target runs on the main thread")
}

fn has_ump_endpoints() -> bool {
    tutti_fixtures::macos_major().expect("sw_vers names the macOS version") >= 15
}

/// Block 0 an output over Groups 1 to 2, block 1 a bidirectional one over Group
/// 4, both named.
fn two_blocks(name: &str) -> VirtualSpec {
    let block = |id, direction, first, span| {
        FunctionBlock::builder(
            FunctionBlockId::new(id).unwrap(),
            direction,
            FirstGroup::new(first).unwrap(),
            GroupSpan::new(span).unwrap(),
        )
    };
    let out = block(0, BlockDirection::Output, 1, 2)
        .ui_hint(UiHint::Sender)
        .name(BlockName::new(&format!("{name} Out")).unwrap())
        .build()
        .unwrap();
    let both = block(1, BlockDirection::Bidirectional, 4, 1)
        .midi1(Midi1Port::Unrestricted)
        .name(BlockName::new(&format!("{name} Both")).unwrap())
        .build()
        .unwrap();
    VirtualSpec::new(
        EndpointName::new(name).unwrap(),
        tutti_fixtures::midi::device_identity(),
        tutti_ump::ProductInstanceId::new(INSTANCE),
    )
    .block(out)
    .block(both)
}

/// The endpoint CoreMIDI lists that has the block `block`, once it lists one: the
/// manager learns of a new endpoint from a notification on the device watch's run
/// loop, so the listing follows the creation.
fn listed(block: &str) -> tutti_coremidi_ffi::UmpEndpointInfo {
    let find = || {
        ump_endpoints()
            .expect("CoreMIDI lists the UMP Endpoints")
            .into_iter()
            .find(|e| e.blocks.iter().any(|b| b.name.as_deref() == Some(block)))
    };
    tutti_fixtures::wait_until("CoreMIDI lists the endpoint", PATIENCE, || find().is_some());
    find().expect("the endpoint is listed")
}

/// An endpoint of `spec` whose input goes to an inlet of `session`.
fn open(
    executor: &Executor,
    spec: &VirtualSpec,
    session: &MidiSession,
) -> (Result<VirtualEndpoint, MidiHardwareError>, EventInletNode) {
    let (producer, inlet) = std::sync::Arc::clone(session.ports()).open_input_port("virtual");
    (
        opening::open(executor, spec, producer, main_thread()),
        inlet,
    )
}

/// **A spec's Function Blocks are the blocks other apps read from CoreMIDI**
/// (M2-104 §7.1.8): the one endpoint CoreMIDI lists has both blocks, with the
/// direction, Groups, UI hint and MIDI 1.0 port field the spec declared, and the
/// spec's Device Identity and Product Instance Id. Where the OS is before macOS
/// 15 the spec is refused by name.
pub fn an_endpoint_with_function_blocks_is_listed_with_those_blocks() {
    let name = &unique_name("tutti-vend-blocks");
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(&executor, inlets, MidiHardwareConfig::new()).expect("a session"),
    );
    let (opened, _inlet) = open(&executor, &two_blocks(name), &session);
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let endpoint = opened.expect("macOS 15 opens the endpoint");
    let listed = listed(&format!("{name} Out"));
    assert_eq!(listed.blocks.len(), 2);
    let block = |suffix: &str| {
        listed
            .blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(&format!("{name} {suffix}")))
            .expect("the block is listed")
            .info
    };
    let out = block("Out");
    assert_eq!(out.direction, BlockDirection::Output);
    assert_eq!(out.ui_hint, UiHint::Sender);
    assert_eq!(out.first_group.value().map(FirstGroup::get), Some(1));
    assert_eq!(out.groups_spanned.value().map(GroupSpan::get), Some(2));
    let both = block("Both");
    assert_eq!(both.direction, BlockDirection::Bidirectional);
    assert_eq!(both.midi1, Midi1Port::Unrestricted);
    assert_eq!(both.first_group.value().map(FirstGroup::get), Some(4));
    assert_eq!(
        listed.identity,
        Some(tutti_fixtures::midi::device_identity())
    );
    assert_eq!(listed.instance.as_deref(), Some(INSTANCE));
    // `blocks` is what peers read, in the order the spec declares the blocks.
    let listed_number = |suffix: &str| block(suffix).id.value();
    assert_eq!(
        [Some(endpoint.blocks()[0]), Some(endpoint.blocks()[1])],
        [listed_number("Out"), listed_number("Both")]
    );
    assert!(endpoint.is_alive());
}

/// CoreMIDI has no MIDI-CI version on a block, so a spec that declares one is
/// refused with the refusal named, never opened without it, on every macOS.
pub fn a_block_declaring_a_midi_ci_version_is_refused_by_name() {
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 1)
    .build()
    .unwrap();
    let spec = VirtualSpec::new(
        EndpointName::new("tutti-ci-block").unwrap(),
        tutti_fixtures::midi::device_identity(),
        None,
    )
    .block(block);
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(&executor, inlets, MidiHardwareConfig::new()).expect("a session"),
    );
    let (producer, _inlet) = std::sync::Arc::clone(session.ports()).open_input_port("virtual");
    assert!(matches!(
        opening::open(&executor, &spec, producer, main_thread()),
        Err(MidiHardwareError::Os(Error::Block(
            BlockRefusal::MidiCiVersion { id: 0, version: 2 }
        )))
    ));
}

/// **What another app writes to the endpoint reaches the producer, on the one
/// endpoint that also sends**: a session output sends notes to the endpoint's
/// destination one at a time, and each is heard from the producer's inlet.
///
/// One at a time because the path from the session's output to CoreMIDI is not
/// lossless under load (a loaded CI Mac delivered 83 of 100 notes sent in a burst),
/// so a burst cannot say how many arrived. CoreMIDI also sends the endpoint a few
/// messages of its own (MIDI-CI and stream messages, mostly at creation), so each
/// step listens until the note it sent is among what arrived.
pub fn what_is_written_to_an_endpoints_destination_reaches_its_producer() {
    let name = &unique_name("tutti-vend-input");
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut graph = Listener::over(&inlets);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(&executor, inlets, MidiHardwareConfig::new()).expect("a session"),
    );
    let (opened, inlet) = open(&executor, &two_blocks(name), &session);
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let endpoint = opened.expect("macOS 15 opens the endpoint");
    graph.attach(inlet);
    tutti_fixtures::wait_until("the destination is listed", PATIENCE, || {
        session.outputs().iter().any(|d| d.name.contains(name))
    });
    let device = session
        .outputs()
        .into_iter()
        .find(|d| d.name.contains(name))
        .expect("the destination is listed");
    let mut out = session
        .connect_output(opening::started(&session), &device.id)
        .expect("connects");
    for key in 40..45 {
        let note: Ump = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::of(1), key, 0x8000);
        assert_eq!(out.queue(&[(Offset::ZERO, note)]).packets(), 1);
        let mut heard = Vec::new();
        tutti_fixtures::wait_until("the note reaches the producer", PATIENCE, || {
            heard.extend(graph.listen(Duration::from_millis(20)));
            heard.contains(&note)
        });
    }
    assert!(endpoint.is_alive());
}

/// A sender whose endpoint was dropped is refused by name, and one sent while it
/// lived was accepted.
pub fn a_sender_outlives_its_endpoint_and_refuses_by_name() {
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(&executor, inlets, MidiHardwareConfig::new()).expect("a session"),
    );
    let (opened, _inlet) = open(
        &executor,
        &two_blocks(&unique_name("tutti-vend-sender")),
        &session,
    );
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let endpoint = opened.expect("macOS 15 opens the endpoint");
    let sender = endpoint.sender();
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::of(1), 60, 0x8000);
    assert!(sender.send(note).is_ok());
    drop(endpoint);
    assert!(matches!(
        sender.send(note),
        Err(MidiHardwareError::EndpointGone)
    ));
}

/// Runs every case on the main thread, as `harness = false` does.
pub fn cases() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        an_endpoint_with_function_blocks_is_listed_with_those_blocks,
        a_block_declaring_a_midi_ci_version_is_refused_by_name,
        what_is_written_to_an_endpoints_destination_reaches_its_producer,
        a_sender_outlives_its_endpoint_and_refuses_by_name,
    ]);
}
