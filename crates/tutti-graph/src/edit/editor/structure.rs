//! The structure counter a commit is skipped by: a value control leaves it
//! and the next commit sends nothing; every structural write moves it and the
//! next commit compiles.

use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_units::{Latency, Mix, SampleRate, Samples};

use super::Editor;
use crate::testing::{Kind, TestNode, WireDeclared, declare_outputs};
use crate::{
    Edge, EdgeId, Fade, InputPort, NodeKey, NodeMode, OutputPort, PortSource, Prepare, TapAlign,
    TapId, TapOptions, Unforkable,
};

fn out(node: u64) -> PortSource {
    PortSource::Node(OutputPort::new(NodeKey(node), 0))
}

fn unit(kind: Kind) -> Unforkable<TestNode> {
    Unforkable(TestNode::new(kind))
}

/// A const into a gain into the main output, committed, with its gain lane
/// declared.
fn committed() -> (Editor, crate::Executor, EdgeId) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(
        NodeKey(1),
        "const",
        unit(Kind::Const {
            value: 0.5,
            width: 1,
        }),
    );
    ed.insert(
        NodeKey(2),
        "gain",
        unit(Kind::Gain {
            gain: 2.0,
            width: 1,
        }),
    );
    let feed = EdgeId::into_port(InputPort::new(NodeKey(2), 0), Edge::Direct(out(1)));
    ed.connect_edge(feed).expect("both nodes exist");
    ed.wire_declared_outputs([out(2)]).expect("the node exists");
    ed.set_edge_gain(feed, tutti_units::Gain::UNITY)
        .expect("the edge exists");
    ed.commit().expect("compiles");
    (ed, exec, feed)
}

/// Whether committing sends a plan: a sent commit makes a new base.
fn sends(ed: &mut Editor) -> bool {
    let before = ed.base().map(Arc::clone);
    ed.commit().expect("commits");
    !Arc::ptr_eq(
        before.as_ref().expect("planned"),
        ed.base().expect("planned"),
    )
}

/// **A control is not structure.** A param range, a lane's base, range and
/// fade, a mode and a mix move neither the structure nor what the next
/// commit sends.
#[test]
fn a_value_control_leaves_the_structure_and_the_commit_sends_nothing() {
    let (mut ed, _exec, feed) = committed();
    let at = crate::ParamIn {
        node: NodeKey(2),
        param: tutti_units::UnitParam::Volume.into(),
    };
    let structure = ed.structure.moves;
    assert!(!ed.edited(), "a sent commit clears the flag");
    ed.set_param_range(at, crate::ParamRange::UNBOUNDED)
        .expect("the node exists");
    ed.set_edge_gain(feed, tutti_units::Gain::new(0.5))
        .expect("the edge exists");
    ed.set_edge_gain_range(feed, crate::EdgeGain::DEFAULT_RANGE)
        .expect("the edge exists");
    ed.set_edge_fade(feed, crate::EdgeFade::None)
        .expect("the edge exists");
    ed.set_mode(NodeKey(2), NodeMode::Bypassed)
        .expect("a gain bypasses");
    ed.set_mix(NodeKey(2), Mix::new(0.5)).expect("a gain mixes");
    assert_eq!(ed.structure.moves, structure);
    assert!(!ed.edited(), "a value control needs no commit");
    assert!(!sends(&mut ed), "a value moved, and nothing compiled");
}

/// **Every structural write moves it and sets `edited`, and the next commit
/// clears the flag and compiles when the
/// write changed what the plan is made of**: the spec, a shape, a placed
/// unit, a staged crossfade, a latency cut and a tap waiting for its commit.
/// A leaving node writes the spec without changing it (its edges' fade is a
/// value), so its commit finds the spec as committed and sends nothing.
#[test]
fn every_structural_write_moves_the_structure_and_the_commit_compiles() {
    #[allow(clippy::type_complexity)]
    let writes: Vec<(&str, bool, Box<dyn Fn(&mut Editor)>)> = vec![
        (
            "spec",
            true,
            Box::new(|e| {
                declare_outputs(e, tutti_buffers::ChannelLayout::STEREO);
            }),
        ),
        (
            "shape",
            true,
            Box::new(|e| {
                e.set_tail(NodeKey(1), tutti_units::Tail::Unbounded)
                    .expect("the node exists");
            }),
        ),
        (
            "placing",
            true,
            Box::new(|e| {
                e.insert(
                    NodeKey(3),
                    "const",
                    unit(Kind::Const {
                        value: 0.25,
                        width: 1,
                    }),
                );
            }),
        ),
        (
            "staged fade",
            true,
            Box::new(|e| {
                e.replace(
                    NodeKey(1),
                    unit(Kind::Const {
                        value: 0.75,
                        width: 1,
                    }),
                    Fade::new(Samples::new(64)),
                )
                .expect("the shape fits");
            }),
        ),
        (
            "latency cut",
            true,
            Box::new(|e| {
                e.set_latency(NodeKey(1), Latency::new(Samples::new(8)))
                    .expect("the node exists");
            }),
        ),
        (
            "pending tap",
            true,
            Box::new(|e| {
                let _reader = e
                    .open_tap(
                        TapId::of(1),
                        vec![out(1)],
                        TapOptions::new(Samples::new(64)).align(TapAlign::Raw),
                    )
                    .expect("the source exists");
            }),
        ),
        (
            "leave",
            false,
            Box::new(|e| {
                assert!(!e.remove_declicked(NodeKey(1)), "its edge fades out");
            }),
        ),
    ];
    for (name, compiles, write) in writes {
        let (mut ed, _exec, _) = committed();
        let structure = ed.structure.moves;
        write(&mut ed);
        assert_ne!(ed.structure.moves, structure, "{name} moves the structure");
        assert!(ed.edited(), "{name} sets the flag");
        assert_eq!(sends(&mut ed), compiles, "{name}: the next commit");
        assert!(!ed.edited(), "{name}: the commit clears it");
        assert!(!sends(&mut ed), "{name}: the one after has nothing to do");
    }
}
