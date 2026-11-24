//! What a commit does with a spec an outside caller cannot write: the typed
//! calls never leave a tap or meter whose lanes disagree with its writer, a
//! generation that moved without its unit, or a gain entry without its cell,
//! so these write the spec directly.

use tutti_buffers::MaxBlock;
use tutti_units::{SampleRate, Samples};

use super::{CommitError, Editor};
use crate::testing::{Kind, TestNode};
use crate::{
    Edge, EdgeGain, EdgeId, ForkByClone, ForkError, ForkTarget, MeterId, MeterSpec, NodeKey,
    OutputPort, PortSink, PortSource, Prepare, TapAlign, TapId, TapOptions, TapSpec, Unforkable,
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

/// A tap's lanes are fixed by how it was opened: editing `TapSpec::Ports`'
/// sources to another length is refused at the commit (the writer the
/// executor holds has that many rings).
#[test]
fn a_tap_cannot_change_its_width_under_its_writer() {
    let (mut ed, _exec) = editor();
    ed.insert(
        NodeKey(1),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.5,
            width: 3,
        })),
    );
    let _wide = ed
        .open_tap(
            TapId::of(1),
            vec![out(1, 0), out(1, 1), out(1, 2)],
            TapOptions::new(Samples::new(64)).align(TapAlign::Raw),
        )
        .expect("the sources exist");
    ed.commit().expect("commits");
    let Some(TapSpec::Ports { sources, .. }) = ed.spec_mut().topology.taps.get_mut(&TapId::of(1))
    else {
        panic!("a ports tap")
    };
    sources.push(PortSource::Zero);
    assert_eq!(
        ed.commit(),
        Err(CommitError::TapWidth {
            tap: TapId::of(1),
            sources: 4,
            opened: 3
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
    ed.set_outputs(vec![out(1, 0)]).expect("the sources exist");
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
    ed.set_outputs(vec![out(1, 0)]).expect("the sources exist");
    let id = EdgeId {
        sink: PortSink::Output(0),
        from: Edge::Direct(out(1, 0)),
    };
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
