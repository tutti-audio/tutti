//! What the cases of the platforms that publish Function Blocks share: the spec they
//! declare, the endpoint they open and the session they open it beside.

use std::time::Duration;
use tutti_graph::EventInletNode;
use tutti_midi_device::FunctionBlock;
use tutti_midi_hardware::{MidiHardwareError, MidiSession, VirtualEndpoint, VirtualSpec};
use tutti_platform::{Executor, MainThreadMarker};
use tutti_ump::{BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port, UiHint};
use tutti_ump::{BlockName, EndpointName};

#[path = "../input/listener.rs"]
#[allow(
    dead_code,
    reason = "the listener is shared with the input tests, which use more of it"
)]
pub(crate) mod listener;
#[path = "../input/opening.rs"]
pub(crate) mod opening;

/// A bound that only guards a hang.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);
pub(crate) const INSTANCE: &str = "tutti-test-instance";

/// `session` once its backend has started: a backend may make its client on a thread of its
/// own, so a port is opened with the proof that it resolved.
pub(crate) fn started(session: MidiSession) -> MidiSession {
    tutti_fixtures::block_on(session.start()).expect("the backend starts");
    session
}

pub(crate) fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("a harness = false target runs on the main thread")
}

/// Block 0 an output over Groups 1 to 2, block 1 a bidirectional one over Group
/// 4, both named.
pub(crate) fn two_blocks(name: &str) -> VirtualSpec {
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

/// An endpoint of `spec` whose input goes to an inlet of `session`.
pub(crate) fn open(
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
