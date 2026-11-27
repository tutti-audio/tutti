//! The cases, on Windows MIDI Services.

use std::time::Duration;
use tutti_fixtures::midi::m1_note_on;
use tutti_fixtures::unique_name;
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_hardware::{HardwareMidiInputs, MidiHardwareConfig, MidiHardwareError, MidiSession};
use tutti_midi_runtime::InputInlets;
use tutti_platform::Executor;
use tutti_ump::{FunctionBlockId, MidiChannel, U7, Ump};

use super::common::listener::Listener;
use super::common::{PATIENCE, open, opening, started, two_blocks};
#[path = "../input/listing.rs"]
mod listing;

/// A session over `inlets`, started.
fn session(executor: &Executor, inlets: std::sync::Arc<InputInlets>) -> MidiSession {
    started(
        MidiSession::open(
            executor,
            HardwareMidiInputs::new(inlets),
            MidiHardwareConfig::new(),
        )
        .expect("a session"),
    )
}

/// **The service lists an endpoint that declares Function Blocks once, as one device that
/// sends and receives, under the name it was declared with**, and the blocks are numbered as
/// declared, in the order the spec declares them.
pub(crate) fn an_endpoint_with_function_blocks_is_listed_once_and_numbered_as_declared() {
    let name = &unique_name("tutti-vend-blocks");
    let executor = Executor::new();
    let session = session(
        &executor,
        InputInlets::new(EventCapacity::of(8), Duration::ZERO),
    );
    let (opened, _inlet) = open(&executor, &two_blocks(name), &session);
    let endpoint = opened.expect("the service publishes the endpoint");
    let device = listing::listed(&session, name, |d| d.name.contains(name.as_str()));
    assert!(device.direction.is_input() && device.direction.is_output());
    let same = |devices: Vec<tutti_midi_device::DeviceInfo>| {
        devices
            .iter()
            .filter(|d| d.name.contains(name.as_str()))
            .count()
    };
    assert_eq!((same(session.inputs()), same(session.outputs())), (1, 1));
    assert_eq!(
        endpoint.blocks(),
        [FunctionBlockId::of(0), FunctionBlockId::of(1)]
    );
    assert!(endpoint.is_alive());
}

/// **What a session output sends to the endpoint reaches its producer unchanged and in
/// order, on the one endpoint that also sends**: MIDI 1.0 notes, which the service's
/// endpoint takes as they are.
pub(crate) fn what_a_session_output_sends_reaches_the_endpoints_producer_in_order() {
    let name = &unique_name("tutti-vend-input");
    let inlets = InputInlets::new(EventCapacity::of(64), Duration::ZERO);
    let mut graph = Listener::over(&inlets);
    let executor = Executor::new();
    let session = session(&executor, inlets);
    let (opened, inlet) = open(&executor, &two_blocks(name), &session);
    let endpoint = opened.expect("the service publishes the endpoint");
    graph.attach(inlet);
    let device = listing::listed(&session, name, |d| {
        d.direction.is_output() && d.name.contains(name.as_str())
    });
    let mut out = session
        .connect_output(opening::started(&session), &device.id)
        .expect("connects");
    let note = |n: u8| m1_note_on(MidiChannel::of(1), n, U7::of(0x40));
    // A probe on a key no test asserts on, so a late one is never a test note.
    graph.ready(
        || {
            let _ = out.queue(&[(Offset::ZERO, note(59))]);
        },
        PATIENCE,
    );
    let sent: Vec<Ump> = (60..65).map(note).collect();
    for ump in &sent {
        assert_eq!(out.queue(&[(Offset::ZERO, *ump)]).packets(), 1);
    }
    assert_eq!(graph.hear(sent.len(), PATIENCE), sent);
    assert!(endpoint.is_alive());
}

/// A sender whose endpoint was dropped is refused by name, and one sent while it lived was
/// accepted.
pub(crate) fn a_sender_outlives_its_endpoint_and_refuses_by_name() {
    let executor = Executor::new();
    let session = session(
        &executor,
        InputInlets::new(EventCapacity::of(8), Duration::ZERO),
    );
    let (opened, _inlet) = open(
        &executor,
        &two_blocks(&unique_name("tutti-vend-sender")),
        &session,
    );
    let endpoint = opened.expect("the service publishes the endpoint");
    let sender = endpoint.sender();
    let note = tutti_fixtures::midi::note_on(MidiChannel::of(1), 60, 0x8000);
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
        an_endpoint_with_function_blocks_is_listed_once_and_numbered_as_declared,
        what_a_session_output_sends_reaches_the_endpoints_producer_in_order,
        a_sender_outlives_its_endpoint_and_refuses_by_name,
    ]);
}
