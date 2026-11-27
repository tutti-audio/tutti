//! The cases, on macOS.

use std::time::Duration;
use tutti_coremidi_ffi::{BlockRefusal, Error, ump_endpoints};
use tutti_fixtures::unique_name;
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_device::FunctionBlock;
use tutti_midi_hardware::{MidiHardwareConfig, MidiHardwareError, MidiSession, VirtualSpec};
use tutti_midi_runtime::InputInlets;
use tutti_platform::Executor;
use tutti_ump::EndpointName;
use tutti_ump::{
    BlockDirection, CiVersion, Field, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port,
    Sysex8StreamCount, UiHint, Ump,
};

use super::common::listener::Listener;

/// A bound that only guards a hang.
const PATIENCE: Duration = Duration::from_secs(30);
use super::common::{INSTANCE, main_thread, open, opening, started, two_blocks};

fn has_ump_endpoints() -> bool {
    tutti_fixtures::macos_major().expect("sw_vers names the macOS version") >= 15
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
/// (M2-104 §7.1.8): the one endpoint CoreMIDI lists has both blocks, with the
/// direction, Groups, UI hint and MIDI 1.0 port field the spec declared, and the
/// spec's Device Identity and Product Instance Id. Where the OS is before macOS
/// 15 the spec is refused by name.
///
/// CoreMIDI's listing on macOS 15 reports every block's direction as 0, so there
/// it reads `Reserved(0)`; from macOS 26 it is the registered direction.
pub(crate) fn an_endpoint_with_function_blocks_is_listed_with_those_blocks() {
    let name = &unique_name("tutti-vend-blocks");
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(
            &executor,
            tutti_midi_hardware::HardwareMidiInputs::new(inlets),
            MidiHardwareConfig::new(),
        )
        .expect("a session"),
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
    let from_26 = tutti_fixtures::macos_major().is_some_and(|major| major >= 26);
    let is_zero = |d: BlockDirection| matches!(d, BlockDirection::Reserved(r) if r.get() == 0);
    if from_26 {
        assert_eq!(out.direction, BlockDirection::Output);
    } else {
        assert!(is_zero(out.direction), "{:?}", out.direction);
    }
    assert_eq!(out.ui_hint, UiHint::Sender);
    assert_eq!(out.first_group.value().map(FirstGroup::get), Some(1));
    assert_eq!(out.groups_spanned.value().map(GroupSpan::get), Some(2));
    let both = block("Both");
    if from_26 {
        assert_eq!(both.direction, BlockDirection::Bidirectional);
    } else {
        assert!(is_zero(both.direction), "{:?}", both.direction);
    }
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
pub(crate) fn a_block_declaring_a_midi_ci_version_is_refused_by_name() {
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(CiVersion::of(2), Sysex8StreamCount::new(1))
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
        MidiSession::open(
            &executor,
            tutti_midi_hardware::HardwareMidiInputs::new(inlets),
            MidiHardwareConfig::new(),
        )
        .expect("a session"),
    );
    let (producer, _inlet) = std::sync::Arc::clone(session.ports()).open_input_port("virtual");
    assert!(matches!(
        opening::open(&executor, &spec, producer, main_thread()),
        Err(MidiHardwareError::Os(Error::Block(
            BlockRefusal::MidiCiVersion {
                id: 0,
                version: Field::Value(version)
            }
        ))) if version == CiVersion::of(2)
    ));
}

/// **What another app writes to the endpoint reaches the producer, whole and in
/// order, on the one endpoint that also sends**: a session output sends a burst of
/// 100 distinct notes to the endpoint's destination in one call, and every one is
/// heard from the producer's inlet.
///
/// The inlet holds more than the burst plus the few messages CoreMIDI sends the
/// endpoint itself (MIDI-CI and stream messages, mostly at creation), so nothing
/// can overflow it; those messages are skipped by matching only the notes sent.
pub(crate) fn what_is_written_to_an_endpoints_destination_reaches_its_producer() {
    const BURST: u16 = 100;
    let name = &unique_name("tutti-vend-input");
    let inlets = InputInlets::new(EventCapacity::of(256), Duration::ZERO);
    let mut graph = Listener::over(&inlets);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(
            &executor,
            tutti_midi_hardware::HardwareMidiInputs::new(inlets),
            MidiHardwareConfig::new(),
        )
        .expect("a session"),
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
    let sent: Vec<Ump> = (0..BURST)
        .map(|i| {
            let channel = tutti_ump::MidiChannel::new((i % 16) as u8).expect("below 16");
            tutti_fixtures::midi::note_on(channel, (i % 128) as u8, 0x1000 + i)
        })
        .collect();
    let burst: Vec<(Offset, Ump)> = sent.iter().map(|&note| (Offset::ZERO, note)).collect();
    assert_eq!(out.queue(&burst).packets(), usize::from(BURST));
    // The deadline only guards a hang: what arrived is asserted after it, so a
    // short burst names what came instead.
    let start = tutti_platform::Instant::now();
    let mut heard = Vec::new();
    let mut others = Vec::new();
    while heard.len() < sent.len() && start.elapsed() < PATIENCE {
        for ump in graph.listen(Duration::from_millis(20)) {
            if sent.contains(&ump) {
                heard.push(ump);
            } else {
                others.push(ump);
            }
        }
    }
    assert_eq!(
        heard,
        sent,
        "every note of the burst arrived once, in order; {} of {} arrived, and {} other messages: {:?}",
        heard.len(),
        sent.len(),
        others.len(),
        &others[..others.len().min(12)]
    );
    assert!(endpoint.is_alive());
}

/// A sender whose endpoint was dropped is refused by name, and one sent while it
/// lived was accepted.
pub(crate) fn a_sender_outlives_its_endpoint_and_refuses_by_name() {
    let inlets = InputInlets::new(EventCapacity::of(8), Duration::ZERO);
    let executor = Executor::new();
    let session = started(
        MidiSession::open(
            &executor,
            tutti_midi_hardware::HardwareMidiInputs::new(inlets),
            MidiHardwareConfig::new(),
        )
        .expect("a session"),
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
pub(crate) fn cases() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        an_endpoint_with_function_blocks_is_listed_with_those_blocks,
        a_block_declaring_a_midi_ci_version_is_refused_by_name,
        what_is_written_to_an_endpoints_destination_reaches_its_producer,
        a_sender_outlives_its_endpoint_and_refuses_by_name,
    ]);
}
