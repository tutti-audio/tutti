//! The session's edge gains (doc 013, "Edge gain", G3): a gain lane declared
//! with the wire (`Connection::gain`), its value the `GainTarget`'s, `clear_gain`,
//! and what removing a source does to its weight.
//!
//! Every figure is analytic: constants of exactly representable values and
//! weights, so the weighted fold `(0.0 + w * a) + b` is exact in `f32`.

use std::sync::Arc;
use tutti_graph::{Fade, ForkByClone};

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::NodeKey;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    ChannelEdge, DeclarationError, GainTarget, GainTargetError, InputPort, MainOutput, OutputPort,
    Replacement, Session, SetRefusal, ValueErrorKind,
};
use tutti_units::{Gain, Samples};

const A: f32 = 0.25;
const B: f32 = 0.5;
const BLOCK: usize = 64;

fn offline() -> Session {
    Session::builder().build().expect("builds")
}

fn amp(x: f64) -> Gain {
    Gain::new(x)
}

/// Commits and renders two blocks, so a weight that moved has finished its
/// one-block ramp; returns the last block's channel `ch`.
fn settled(s: &mut Session, ch: usize) -> Vec<f32> {
    let _ = s.update();
    let mut block = vec![f32::NAN; BLOCK * 2];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
    block.iter().skip(ch).step_by(2).copied().collect()
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

/// Constants `a` (`A`) and `b` (`B`) and a pass-through `sink` feeding output
/// channel 0, committed once so `has_uncommitted_edits` is false. Nothing feeds the sink.
struct Rig {
    s: Session,
    a: NodeKey,
    b: NodeKey,
    sink: NodeKey,
}

fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(A)));
    let b = g.add(ForkByClone(Const::mono(B)));
    let sink = g.add(ForkByClone(Through::mono()));
    g.set_sources(MainOutput::channel(0), [OutputPort::new(sink, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
    Rig {
        s,
        a: a.erase(),
        b: b.erase(),
        sink: sink.erase(),
    }
}

fn node(k: NodeKey) -> OutputPort {
    OutputPort::new(k, 0)
}

impl Rig {
    fn edge_from(&self, from: NodeKey) -> ChannelEdge {
        ChannelEdge::new(node(from), InputPort::new(self.sink, 0))
    }

    /// The value of the gain lane of `from`'s edge into the sink.
    fn gain_of(&self, from: NodeKey) -> Gain {
        let edge = self.edge_from(from);
        let target = self
            .s
            .graph_ref()
            .gain_target(edge.source(), edge.sink())
            .expect("a lane");
        self.s.get(target).unwrap()
    }

    /// Whether `from`'s edge into the sink has no gain lane.
    fn no_lane(&self, from: NodeKey) -> bool {
        let edge = self.edge_from(from);
        self.s
            .graph_ref()
            .gain_target(edge.source(), edge.sink())
            .is_err()
    }
}

/// Declare, read, write and clear: a plain edge (aliased, not weighted at
/// unity) has no gain lane, so it has no target; declaring one is structural
/// and moves the sum; a write to it reads back at once and reaches the render
/// with the update; clearing returns the plain sum and is structural too, a
/// second clear finds nothing, and the cleared lane's target is gone.
#[test]
fn an_edge_gain_is_declared_written_read_and_cleared() {
    let mut r = rig();
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    r.s.graph()
        .connect(node(r.b), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(all_are(&settled(&mut r.s, 0), A + B));
    let edge = r.edge_from(r.a);
    assert_eq!(
        r.s.graph_ref().gain_target(edge.source(), edge.sink()),
        Err(GainTargetError::NoLane(edge)),
        "plain: no lane"
    );

    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    assert!(
        r.s.graph_ref().has_uncommitted_edits(),
        "a declared lane is structural"
    );
    let gain =
        r.s.graph_ref()
            .gain_target(edge.source(), edge.sink())
            .expect("a lane");
    assert_eq!(r.s.get(gain).unwrap(), amp(0.5));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));

    r.s.set(gain.to(amp(-2.0))).unwrap();
    assert_eq!(r.s.get(gain).unwrap(), amp(-2.0), "read back at once");
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + -2.0 * A) + B));

    assert!(edge.clear_gain(r.s.graph()));
    assert!(
        r.s.graph_ref().has_uncommitted_edits(),
        "clearing is structural"
    );
    assert!(all_are(&settled(&mut r.s, 0), A + B), "the plain sum again");
    assert_eq!(
        r.s.set(gain.to(amp(1.0))).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget,
        "the cleared lane has no target"
    );
    assert!(!edge.clear_gain(r.s.graph()), "nothing left to clear");
    assert!(!r.s.graph_ref().has_uncommitted_edits());
}

/// An edge that is not there, or a plain one, has no gain target
/// (`NoLane`), a gain that is not a number is refused at the call and lands
/// nothing (the lane keeps its weight, nothing is edited), and a lane that
/// goes with its edge takes its target with it.
#[test]
fn a_gain_target_needs_a_lane_and_a_number() {
    let mut r = rig();
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    let _ = r.s.update();
    let missing = r.edge_from(r.b);
    assert_eq!(
        r.s.graph_ref()
            .gain_target(missing.source(), missing.sink()),
        Err(GainTargetError::NoLane(missing))
    );
    let real = r.edge_from(r.a);
    let gain =
        r.s.graph_ref()
            .gain_target(real.source(), real.sink())
            .expect("a lane");
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let refused = r.s.set(gain.to(amp(bad))).unwrap_err();
        assert_eq!(
            refused.kind,
            ValueErrorKind::Refused(SetRefusal::NotFinite),
            "{bad}"
        );
    }
    assert_eq!(r.s.get(gain).unwrap(), amp(0.5), "nothing landed");
    assert!(
        all_are(&settled(&mut r.s, 0), 0.5 * A),
        "the render keeps it"
    );
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "a refusal edits nothing"
    );

    r.s.graph()
        .disconnect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    let _ = r.s.update();
    assert_eq!(
        r.s.set(gain.to(amp(1.0))).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget,
        "the edge is gone"
    );
}

/// `add_source_weighted` adds the edge and its weight in one edit, and the
/// render is the weighted fold; a source the port already holds only has its
/// weight set; silence adds nothing (it is no edge); a gain that is not a
/// number adds nothing.
#[test]
fn add_source_weighted_adds_the_edge_and_its_weight() {
    let mut r = rig();
    let (a, b, sink) = (r.a, r.b, r.sink);
    r.s.graph()
        .connect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .connect(node(b), InputPort::new(sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(-0.25))
        .unwrap();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(sink, 0)).unwrap(),
        [node(a), node(b)]
    );
    assert_eq!(r.gain_of(a), amp(0.5));
    assert_eq!(r.gain_of(b), amp(-0.25));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + -0.25 * B));

    // Held already: the weight is set, nothing else changes.
    r.s.graph()
        .connect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(2.0))
        .unwrap();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(sink, 0)).unwrap(),
        [node(a), node(b)]
    );
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 2.0 * A) + -0.25 * B));

    // A bad weight adds nothing.
    let c = r.s.graph().add(ForkByClone(Const::mono(0.125)));
    assert_eq!(
        r.s.graph()
            .connect(node(c.erase()), InputPort::new(sink, 0))
            .unwrap()
            .unfaded()
            .gain(amp(f64::NAN))
            .err(),
        Some(DeclarationError::NotFinite)
    );
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(sink, 0)).unwrap(),
        [node(a), node(b)]
    );
}

/// The output-channel twin: weights on what feeds main output 1, read on
/// the right channel.
#[test]
fn add_output_source_weighted_weights_the_channel() {
    let mut r = rig();
    let (a, b) = (r.a, r.b);
    r.s.graph()
        .connect(node(a), MainOutput::channel(1))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .connect(node(b), MainOutput::channel(1))
        .unwrap()
        .unfaded()
        .gain(amp(2.0))
        .unwrap();
    assert_eq!(
        r.s.graph_ref().sources(MainOutput::channel(1)).unwrap(),
        [node(a), node(b)]
    );
    let weight = |r: &Rig, from| {
        r.s.graph_ref()
            .gain_target(node(from), MainOutput::channel(1))
            .expect("a lane")
    };
    assert_eq!(r.s.get(weight(&r, a)).unwrap(), amp(0.5));
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + 2.0 * B));
    r.s.set(weight(&r, b).to(amp(1.0))).unwrap();
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + 1.0 * B));
    assert!(ChannelEdge::new(node(a), MainOutput::channel(1)).clear_gain(r.s.graph()));
    assert!(ChannelEdge::new(node(b), MainOutput::channel(1)).clear_gain(r.s.graph()));
    assert!(all_are(&settled(&mut r.s, 1), A + B));
}

/// A source that goes takes its weight with it, by whichever call removes it
/// (`remove_source`, `set_sources`, `remove_output_source`,
/// `set_output_sources`): the next commit is valid (a stale entry would fail
/// it), and the source added back is plain, not weighted again.
#[test]
fn removing_a_source_drops_its_weight() {
    let mut r = rig();
    let (a, b, sink) = (r.a, r.b, r.sink);
    let g = |r: &mut Rig| {
        r.s.graph()
            .connect(node(a), InputPort::new(sink, 0))
            .unwrap()
            .unfaded()
            .gain(amp(0.5))
            .unwrap();
        r.s.graph()
            .connect(node(b), InputPort::new(sink, 0))
            .unwrap()
            .unfaded();
    };
    g(&mut r);
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));

    r.s.graph()
        .disconnect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded();
    assert!(r.no_lane(a));
    assert!(all_are(&settled(&mut r.s, 0), B), "valid, and just b");
    r.s.graph()
        .connect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded();
    assert!(
        all_are(&settled(&mut r.s, 0), A + B),
        "added back plain, no stale weight"
    );

    r.s.graph()
        .connect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .set_sources(InputPort::new(sink, 0), [node(b)])
        .unwrap()
        .unfaded();
    assert!(r.no_lane(a));
    assert!(all_are(&settled(&mut r.s, 0), B));

    r.s.graph()
        .connect(node(a), MainOutput::channel(1))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .connect(node(b), MainOutput::channel(1))
        .unwrap()
        .unfaded();
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + B));
    r.s.graph()
        .disconnect(node(a), MainOutput::channel(1))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().gain_target(node(a), MainOutput::channel(1)),
        Err(GainTargetError::NoLane(ChannelEdge::new(
            node(a),
            MainOutput::channel(1)
        )))
    );
    assert!(all_are(&settled(&mut r.s, 1), B));
    r.s.graph()
        .connect(node(a), MainOutput::channel(1))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .set_sources(MainOutput::channel(1), [node(b)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().gain_target(node(a), MainOutput::channel(1)),
        Err(GainTargetError::NoLane(ChannelEdge::new(
            node(a),
            MainOutput::channel(1)
        )))
    );
    assert!(all_are(&settled(&mut r.s, 1), B));
}

/// Moving the weight of an edge that already has one is a control: nothing is
/// edited, no plan is sent (the sent plan is the same `Arc`, the commit count
/// the same), and the render still follows.
#[test]
fn moving_a_weight_marks_no_recompile() {
    let mut r = rig();
    let (a, sink) = (r.a, r.sink);
    r.s.graph()
        .connect(node(a), InputPort::new(sink, 0))
        .unwrap()
        .unfaded()
        .gain(amp(0.5))
        .unwrap();
    r.s.graph()
        .connect(node(r.b), InputPort::new(sink, 0))
        .unwrap()
        .unfaded();
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));
    let plan = r.s.graph_ref().testing().sent_plan().expect("a plan");
    let commits = r.s.graph_ref().testing().commits_sent();

    let gain =
        r.s.graph_ref()
            .gain_target(node(a), InputPort::new(sink, 0))
            .expect("a lane");
    r.s.set(gain.to(amp(1.5))).unwrap();
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "a control, not a rewire"
    );
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 1.5 * A) + B));
    assert!(Arc::ptr_eq(
        &plan,
        &r.s.graph_ref().testing().sent_plan().expect("a plan")
    ));
    assert_eq!(r.s.graph_ref().testing().commits_sent(), commits);
}

/// **A gain target whose lane is gone is refused at the call**, with no
/// update in between, by every path that removes a lane: removing the node,
/// disconnecting the edge, clearing the gain, replacing the sink with a node
/// that has no such input, and rewriting the sink's sources.
#[test]
fn a_gain_target_of_a_removed_lane_is_refused_at_the_call() {
    let lane = |r: &mut Rig| {
        r.s.graph()
            .connect(node(r.a), InputPort::new(r.sink, 0))
            .unwrap()
            .unfaded()
            .gain(amp(0.5))
            .unwrap();
        let _ = r.s.update();
        let edge = r.edge_from(r.a);
        r.s.graph_ref()
            .gain_target(edge.source(), edge.sink())
            .expect("a lane")
    };
    let refused = |r: &mut Rig, gain: GainTarget, path: &str| {
        assert_eq!(
            r.s.set(gain.to(amp(1.0))).unwrap_err().kind,
            ValueErrorKind::NoSuchTarget,
            "{path}"
        );
    };

    let mut r = rig();
    let gain = lane(&mut r);
    r.s.remove(r.a).unwrap();
    refused(&mut r, gain, "node removal");

    let mut r = rig();
    let gain = lane(&mut r);
    r.s.graph()
        .disconnect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap();
    refused(&mut r, gain, "disconnect");

    let mut r = rig();
    let gain = lane(&mut r);
    assert!(r.edge_from(r.a).clear_gain(r.s.graph()));
    refused(&mut r, gain, "clear_gain");

    let mut r = rig();
    let gain = lane(&mut r);
    r.s.graph()
        .replace(
            r.sink,
            Replacement::new(ForkByClone(Const::mono(A))).fade(Fade::new(Samples::new(0))),
        )
        .unwrap();
    refused(&mut r, gain, "replace at an existing key");

    let mut r = rig();
    let gain = lane(&mut r);
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.b)])
        .unwrap();
    refused(&mut r, gain, "set_sources");
}
