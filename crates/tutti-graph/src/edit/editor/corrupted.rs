//! What a commit does with a spec an outside caller cannot write: the typed
//! calls never leave a meter whose lanes disagree with its cells, a
//! generation that moved without its unit, or a gain entry without its cell,
//! so these write the spec directly. And what `package` and a re-prepare's
//! resume do with what only the editor builds: a plan that carries a
//! crossfade, the slot of a suspend commit.

use crate::testing::WireDeclared;

use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_units::{SampleRate, Samples};

use super::{CommitError, Editor, MainOutputGate};
use crate::testing::{Kind, TestNode};
use crate::{
    EdgeGain, EdgeId, Fade, ForkByClone, ForkError, ForkTarget, MainOutputBank, MainOutputId,
    MeterId, MeterSpec, NodeKey, OutputPort, PortSource, Prepare, QUEUE_CAPACITY, Unforkable,
    compile,
};

/// A mono source of 0.5 a fork can clone.
#[derive(Clone)]
struct Dc;

impl crate::Node for Dc {
    fn shape(&self) -> crate::Shape {
        crate::Shape::source_unassigned(tutti_buffers::ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &crate::Cx<'_>, mut io: crate::Io<'_>) -> crate::Status {
        io.output(0).fill(0.5);
        crate::Status::Modified
    }
    fn reset(&mut self) {}
}

fn editor() -> (Editor, crate::Executor) {
    Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ))
}

fn out(node: u64, port: u16) -> PortSource {
    PortSource::Node(OutputPort::new(NodeKey(node), port))
}

/// A meter's lanes are fixed by how it was opened: editing a `Ports` list to
/// another length is refused at the commit.
#[test]
fn a_meter_cannot_change_its_width_under_its_cells() {
    let (mut ed, _exec) = editor();
    ed.insert(
        NodeKey(1),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.5,
            width: 2,
        })),
    );
    let _ports = ed
        .open_meter(MeterId::of(1), MeterSpec::raw(vec![out(1, 0), out(1, 1)]))
        .expect("the sources exist");
    ed.commit().expect("commits");
    let Some(MeterSpec::Ports { sources, .. }) =
        ed.spec_mut().topology.meters.get_mut(&MeterId::of(1))
    else {
        panic!("open");
    };
    sources.push(PortSource::Zero);
    assert_eq!(
        ed.commit(),
        Err(CommitError::MeterWidth {
            meter: MeterId::of(1),
            sources: 3,
            capacity: 2
        })
    );
}

/// A fork source from an older generation is refused, loudly: if the spec's
/// generation at a key moved on without a new source, forking would copy a
/// unit that is no longer there. Debug builds assert; release builds return
/// `NotForkable`.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "is from generation 0"))]
fn a_fork_source_from_an_older_generation_is_refused() {
    let pre = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64)));
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(1), "const", ForkByClone(Dc));
    ed.wire_declared_outputs(vec![out(1, 0)]).unwrap();
    assert!(ed.fork(ForkTarget::MainOutput, pre).is_ok());
    ed.spec_mut().generations.insert(NodeKey(1), 7);
    assert_eq!(
        ed.fork(ForkTarget::MainOutput, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );
}

/// A gain entry written with no cell on a connected edge is a lane at the
/// editor's values, unity and the default range: a commit makes its cell and
/// renders the plain figure, never a panic.
#[test]
fn an_entry_without_a_cell_commits_as_a_unity_lane() {
    let (mut ed, mut exec) = editor();
    ed.insert(
        NodeKey(1),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.5,
            width: 1,
        })),
    );
    ed.wire_declared_outputs(vec![out(1, 0)]).unwrap();
    let id = EdgeId::into_output(0, out(1, 0));
    ed.spec_mut().set_edge_gain(id, EdgeGain::default());
    assert!(ed.edge_gain_cell(id).is_none(), "no cell yet");
    ed.commit().expect("commits");
    exec.apply_pending();
    let mut block = vec![0.0f32; 64];
    let transport = crate::TransportPosition::default();
    exec.process(64, &transport, &[], &mut [&mut block[..]]);
    exec.process(64, &transport, &[], &mut [&mut block[..]]);
    assert!(block.iter().all(|&x| x == 0.5), "the lane weighs unity");
    assert_eq!(ed.edge_gain(id), Some(tutti_units::Gain::UNITY));
    assert_eq!(ed.edge_gain_range(id), Some(EdgeGain::DEFAULT_RANGE));
}

/// A const wired to a one-channel main output, committed and not yet applied.
fn committed() -> (Editor, crate::Executor) {
    let (mut ed, exec) = editor();
    ed.insert(
        NodeKey(1),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.5,
            width: 1,
        })),
    );
    ed.wire_declared_outputs([out(1, 0)])
        .expect("the node exists");
    ed.commit().expect("commits");
    (ed, exec)
}

/// A host that has published no main output.
struct Unpublished;

impl MainOutputGate for Unpublished {
    fn published(&self, _: MainOutputId) -> bool {
        false
    }
}

/// A plan compiled against the plan sent last, with `bank` when given.
fn plan_of(ed: &Editor, bank: Option<MainOutputBank>) -> (crate::Plan, crate::Delta) {
    let valid = ed.spec().validate().expect("valid");
    let valid = match bank {
        Some(bank) => valid.with_main_output_bank(bank),
        None => valid,
    };
    compile(&valid, ed.shapes(), ed.prepare(), ed.base().map(|p| &**p)).expect("compiles")
}

/// The editor cannot continue a crossfade it never began, so a plan that
/// carries one is refused and nothing is sent.
#[test]
fn package_refuses_a_plan_that_carries_a_crossfade() {
    let (mut ed, _exec) = committed();
    let bank = MainOutputBank {
        old: MainOutputId::GENESIS,
        base: 1,
        fade: Fade::new(Samples::new(64)),
        continues: false,
    };
    let (plan, delta) = plan_of(&ed, Some(bank));
    let in_flight = ed.in_flight();
    assert_eq!(
        ed.package(plan, delta, BTreeMap::new()),
        Err(CommitError::PackagedCrossfade)
    );
    assert_eq!(ed.in_flight(), in_flight, "nothing was sent");
    assert!(!ed.main_output_fade_running());
}

/// `package` asks the gates `commit` asks: a main output the host never
/// published is refused by both with the same error.
#[test]
fn package_is_refused_a_main_output_the_host_never_published_as_commit_is() {
    let (mut ed, mut exec) = committed();
    exec.apply_pending();
    ed.collect();
    let (plan, delta) = plan_of(&ed, None);
    ed.set_main_output_gate(Arc::new(Unpublished));
    let refused = CommitError::MainOutputNotPublished {
        main_output: ed.spec().main_output_id,
        outputs: 1,
    };
    assert_eq!(ed.commit(), Err(refused.clone()));
    assert_eq!(ed.package(plan, delta, BTreeMap::new()), Err(refused));
}

/// A re-prepare's resume takes the slot its suspend commit held: with the
/// queue full when the re-prepare is sent, the resume still goes, and it
/// starts no crossfade.
#[test]
fn a_resume_always_has_the_slot_of_its_suspend_commit() {
    let (mut ed, mut exec) = committed();
    // One commit is out; fill every slot but the suspend's.
    for key in 2..=u64::try_from(QUEUE_CAPACITY - 1).expect("small") {
        ed.insert(
            NodeKey(key),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: 0.5,
                width: 1,
            })),
        );
        ed.commit().expect("room");
    }
    assert_eq!(ed.in_flight(), QUEUE_CAPACITY - 1);
    ed.reprepare(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(128)),
    ))
    .expect("the last slot");
    assert_eq!(ed.in_flight(), QUEUE_CAPACITY, "the queue is full");
    exec.apply_pending();
    ed.collect();
    assert_eq!(ed.poison_cause(), None);
    assert!(!ed.is_repreparing());
    assert_eq!(ed.in_flight(), 1, "the resume was sent");
    assert_eq!(ed.fades_in_flight(), 0);
    exec.apply_pending();
    assert_eq!(exec.prepare().max_block().get(), 128);
}
