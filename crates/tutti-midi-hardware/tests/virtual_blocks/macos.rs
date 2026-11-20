//! The cases, on macOS.

use std::time::Duration;
use tutti_coremidi_ffi::{BlockRefusal, Error, ump_endpoints};
use tutti_fixtures::unique_name;
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_device::FunctionBlock;
use tutti_midi_hardware::{
    MidiHardwareConfig, MidiHardwareError, MidiSession, VirtualDestination, VirtualSource,
    VirtualSpec,
};
use tutti_midi_runtime::InputInlets;
use tutti_platform::MainThreadMarker;
use tutti_ump::{BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port, UiHint, Ump};
use tutti_ump::{BlockName, EndpointName};

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);
const INSTANCE: &str = "tutti-test-instance";

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

/// **A spec's Function Blocks are the blocks other apps read from CoreMIDI**
/// (M2-104 §7.1.8): the endpoint CoreMIDI lists has both blocks, with the
/// direction, Groups, UI hint and MIDI 1.0 port field the spec declared, and the
/// spec's Device Identity and Product Instance Id. Where the OS is before macOS
/// 15 the spec is refused by name.
pub fn a_source_with_function_blocks_is_listed_with_those_blocks() {
    let name = &unique_name("tutti-vsrc-blocks");
    let opened = VirtualSource::open(&two_blocks(name), main_thread());
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let source = opened.expect("macOS 15 opens the source");
    let listed = listed(&format!("{name} Out"));
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
        [Some(source.blocks()[0]), Some(source.blocks()[1])],
        [listed_number("Out"), listed_number("Both")]
    );
}

/// The same spec opened as a destination is listed with the same blocks and
/// identity.
pub fn a_destination_with_function_blocks_is_listed_with_those_blocks() {
    let name = unique_name("tutti-vdst-blocks");
    let spec = two_blocks(&name);
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let session = MidiSession::open(inlets, MidiHardwareConfig::new()).expect("a session");
    let (producer, _inlet) = std::sync::Arc::clone(session.ports()).open_input_port("virtual");
    let opened = VirtualDestination::open(&spec, producer, main_thread());
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let destination = opened.expect("macOS 15 opens the destination");
    let listed = listed(&format!("{name} Both"));
    assert_eq!(listed.blocks.len(), 2);
    let number = |suffix: &str| {
        listed
            .blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(&format!("{name} {suffix}")))
            .and_then(|b| b.info.id.value())
    };
    assert_eq!(
        [Some(destination.blocks()[0]), Some(destination.blocks()[1])],
        [number("Out"), number("Both")]
    );
    assert_eq!(
        listed.identity,
        Some(tutti_fixtures::midi::device_identity())
    );
    assert_eq!(listed.instance.as_deref(), Some(INSTANCE));
    assert!(destination.is_alive());
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
    assert!(matches!(
        VirtualSource::open(&spec, main_thread()),
        Err(MidiHardwareError::Os(Error::Block(
            BlockRefusal::MidiCiVersion { id: 0, version: 2 }
        )))
    ));
}

/// **What is written to the destination the OS lists beside a source with
/// Function Blocks is counted as refused**: a session output sends notes to it one
/// at a time, and after each one `losses` rises above the level read before it.
///
/// One at a time because the path from the session's output to CoreMIDI is not
/// lossless under load (a loaded CI Mac delivered 83 of 100 notes sent in a burst),
/// so a burst cannot say how many arrived. CoreMIDI also sends the endpoint a few
/// messages of its own (MIDI-CI and stream messages, mostly at creation), which the
/// count includes; a counter that counts nothing fails the first step and every one
/// after it, which no message of CoreMIDI's can stand in for.
pub fn what_is_written_to_a_sources_destination_is_counted_as_refused() {
    let name = &unique_name("tutti-vsrc-losses");
    let opened = VirtualSource::open(&two_blocks(name), main_thread());
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let source = opened.expect("macOS 15 opens the source");
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let session = MidiSession::open(inlets, MidiHardwareConfig::new()).expect("a session");
    tutti_fixtures::wait_until("the destination is listed", PATIENCE, || {
        session.outputs().iter().any(|d| d.name.contains(name))
    });
    let device = session
        .outputs()
        .into_iter()
        .find(|d| d.name.contains(name))
        .expect("the destination is listed");
    let mut out = session.connect_output(&device.id).expect("connects");
    let note: Ump = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::of(1), 60, 0x8000);
    for _ in 0..20 {
        let before = source.losses().refused;
        assert_eq!(out.queue(&[(Offset::ZERO, note)]).packets(), 1);
        tutti_fixtures::wait_until("the note is counted as refused", PATIENCE, || {
            source.losses().refused > before
        });
    }
}

/// A sender whose source was dropped is refused by name, and one sent while it
/// lived was accepted.
pub fn a_sender_outlives_its_source_and_refuses_by_name() {
    let opened = VirtualSource::open(
        &two_blocks(&unique_name("tutti-vsrc-sender")),
        main_thread(),
    );
    if !has_ump_endpoints() {
        assert!(matches!(
            opened,
            Err(MidiHardwareError::VirtualBlocksUnsupported)
        ));
        return;
    }
    let source = opened.expect("macOS 15 opens the source");
    let sender = source.sender();
    let note = tutti_fixtures::midi::note_on(tutti_ump::MidiChannel::of(1), 60, 0x8000);
    assert!(sender.send(note).is_ok());
    drop(source);
    assert!(matches!(
        sender.send(note),
        Err(MidiHardwareError::EndpointGone)
    ));
}

/// Runs every case on the main thread, as `harness = false` does.
pub fn cases() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        a_source_with_function_blocks_is_listed_with_those_blocks,
        a_destination_with_function_blocks_is_listed_with_those_blocks,
        a_block_declaring_a_midi_ci_version_is_refused_by_name,
        what_is_written_to_a_sources_destination_is_counted_as_refused,
        a_sender_outlives_its_source_and_refuses_by_name,
    ]);
}
