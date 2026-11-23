//! The session's bus edges (doc 013, "Buses and coercion between declared
//! layouts", B2): `Graph::connect_bus`, `disconnect_bus`, `bus_sources` and
//! `buses`, and what a bus edge does to the sum its sink holds.
//!
//! Every figure is analytic: constants of exactly representable values, so
//! `0.5 L + 0.5 R` is exact in `f32`.

use crate::offline::offline;
use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology};
use tutti_buffers::{FoldError, InterleavedMut};
use tutti_graph::{BusError, BusOutputPort, Buses, NodeBus, NodeKey, Unforkable};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_session::BusInputPort;
use tutti_session::{
    ChannelEdge, Committed, ConnectError, GainTargetError, InputPort, MainOutput, OutputPort,
    Session, SessionEvent,
};

const BLOCK: usize = 64;

fn layout(n: u16) -> ChannelLayout {
    ChannelLayout::from_count(n)
}

fn named(n: u16) -> BusTopology {
    BusTopology::try_from(&ChannelTopology::default_for(layout(n)).unwrap()).unwrap()
}

fn unassigned(n: u16) -> BusTopology {
    BusTopology::unassigned(layout(n)).unwrap()
}

/// Constant `values` on its output channels, declaring `out` as its bus.
struct Src {
    values: Vec<f32>,
    out: BusTopology,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::source(self.out)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c)[0] = v;
        }
        Status::Constant
    }
    fn reset(&mut self) {}
}

/// A pass-through of its input, declaring `input` as its bus.
struct Pass {
    input: BusTopology,
}

impl Node for Pass {
    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(self.input),
            Buses::unassigned(self.input.layout()),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Bypass
    }
    fn reset(&mut self) {}
}

/// Commits and renders a block; returns output channel 0.
fn out0(s: &mut Session) -> Vec<f32> {
    let _ = s.update();
    let mut block = vec![f32::NAN; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    block.iter().step_by(2).copied().collect()
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

fn bus(node: NodeKey) -> BusInputPort {
    BusInputPort::new(node, 0)
}

fn node_bus(node: NodeKey) -> NodeBus {
    NodeBus::new(node, 0)
}

struct Rig {
    s: Session,
    stereo: NodeKey,
    own: NodeKey,
    sink: NodeKey,
}

/// A stereo source (`0.25`, `0.75`), a mono source of `0.125`, and a mono
/// pass-through feeding output 0, committed once.
fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let stereo = g.add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: named(2),
    }));
    let own = g.add(Unforkable(Src {
        values: vec![0.125],
        out: named(1),
    }));
    let sink = g.add(Unforkable(Pass { input: named(1) }));
    g.set_sources(MainOutput::channel(0), [OutputPort::new(sink, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
    Rig {
        s,
        stereo: stereo.erase(),
        own: own.erase(),
        sink: sink.erase(),
    }
}

/// Connect, render, disconnect: the fold sums beside an ordinary source of the
/// sink's port while it is connected, and removing it leaves the ordinary
/// source alone. The declared buses read back.
#[test]
fn connecting_and_disconnecting_a_bus_edge_round_trips() {
    let mut r = rig();
    let g = r.s.graph();
    let (ins, outs) = g.node_buses(r.stereo).expect("declared");
    assert!(ins.is_empty(), "no input channels, no input bus");
    assert_eq!(outs.len(), 1);
    assert_eq!(outs.get(0).unwrap().topology, named(2));
    assert_eq!(
        g.node_buses(NodeKey(u64::MAX)).unwrap_err().node(),
        NodeKey(u64::MAX)
    );

    g.connect(OutputPort::new(r.own, 0), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(all_are(&out0(&mut r.s), 0.125));

    let g = r.s.graph();
    g.connect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
        .unwrap()
        .unfaded();
    assert!(g.has_uncommitted_edits());
    assert_eq!(
        g.bus_sources(BusInputPort::new(r.sink, 0)).unwrap(),
        [BusOutputPort::Node(node_bus(r.stereo))]
    );
    // A second connect of the same edge is the same edge.
    g.connect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
        .unwrap()
        .unfaded();
    assert_eq!(
        g.bus_sources(BusInputPort::new(r.sink, 0)).unwrap().len(),
        1
    );
    // The fold (0.5 * 0.25 + 0.5 * 0.75) summed with the ordinary source
    // (bus edge's node sorts first): (0.125 + 0.375) + 0.125.
    let want = (0.5f32 * 0.25 + 0.5 * 0.75) + 0.125;
    assert_eq!(want, 0.625);
    assert!(all_are(&out0(&mut r.s), want));

    let g = r.s.graph();
    assert!(
        g.disconnect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(
        !g.disconnect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(g.has_uncommitted_edits());
    assert!(
        g.bus_sources(BusInputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        g.sources(InputPort::new(r.sink, 0)).unwrap(),
        [OutputPort::new(r.own, 0)],
        "the author's own source stays"
    );
    assert!(all_are(&out0(&mut r.s), 0.125));
}

/// **A bus edge fades per lane** (doc 030 section 4): both lanes of the fold
/// rise from 0 to their weight along the smootherstep over `DECLICK`, 240 frames
/// (the fifth 64-frame block is at the weight), and a disconnect fades them
/// out again.
#[test]
fn a_bus_edge_fades_in_and_out_lane_by_lane() {
    let mut r = rig();
    let g = r.s.graph();
    g.connect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
        .unwrap();
    let first = out0(&mut r.s);
    for (j, &x) in first.iter().enumerate() {
        let k = (j + 1) as f64 / 241.0;
        let s = k * k * k * (6.0 * k * k - 15.0 * k + 10.0);
        let want = 0.5 * s;
        assert!(
            (f64::from(x) - want).abs() < 1e-6,
            "frame {j}: {x}, smootherstep {want}"
        );
    }
    for _ in 0..3 {
        out0(&mut r.s);
    }
    assert!(all_are(&out0(&mut r.s), 0.5), "at the fold's weight");

    let g = r.s.graph();
    assert!(
        g.disconnect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
            .unwrap()
            .was_connected()
    );
    assert!(
        g.bus_sources(BusInputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    let down = out0(&mut r.s);
    assert!(
        (down[0] - 0.5).abs() < 1e-3,
        "leaves from the weight it had"
    );
    assert!(down[BLOCK - 1] < down[0]);
    for _ in 0..3 {
        out0(&mut r.s);
    }
    assert!(all_are(&out0(&mut r.s), 0.0), "silent after the span");
}

/// A refused bus edge changes nothing and marks nothing edited: the fold's
/// reason, a bus that is not there, a node that is not there.
#[test]
fn a_refused_bus_edge_changes_nothing() {
    let mut r = rig();
    let g = r.s.graph();
    let three = g.add(Unforkable(Src {
        values: vec![0.0; 3],
        out: unassigned(3),
    }));
    let _ = r.s.update();
    let g = r.s.graph();
    let refused = g
        .connect(BusOutputPort::Node(node_bus(three.erase())), bus(r.sink))
        .unwrap_err();
    assert_eq!(
        refused,
        ConnectError::Bus(BusError::Fold {
            from: BusOutputPort::Node(node_bus(three.erase())),
            to: node_bus(r.sink),
            error: FoldError::Unassigned {
                src: layout(3),
                dst: layout(1)
            }
        })
    );
    assert!(matches!(
        g.connect(BusOutputPort::Node(NodeBus::new(r.stereo, 4)), bus(r.sink)),
        Err(ConnectError::Bus(BusError::NoSuchBus { .. }))
    ));
    assert!(matches!(
        g.connect(
            BusOutputPort::Node(node_bus(NodeKey(u64::MAX))),
            bus(r.sink)
        ),
        Err(ConnectError::Bus(BusError::NoSuchNode { .. }))
    ));
    assert!(!g.has_uncommitted_edits());
    assert!(
        g.bus_sources(BusInputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
}

/// The weights of a bus edge's lanes are the fold's: a gain target of a lane is
/// refused by name, and removing a node takes its bus edges with it so the
/// next commit stands.
#[test]
fn a_lane_has_no_gain_target_and_a_removed_node_takes_its_bus_edges() {
    let mut r = rig();
    let g = r.s.graph();
    g.connect(BusOutputPort::Node(node_bus(r.stereo)), bus(r.sink))
        .unwrap()
        .unfaded();
    let lane = ChannelEdge::new(OutputPort::new(r.stereo, 0), InputPort::new(r.sink, 0));
    assert_eq!(
        g.gain_target(lane.source(), lane.sink()),
        Err(GainTargetError::BusLane(lane))
    );
    assert!(all_are(&out0(&mut r.s), 0.5));

    r.s.remove(r.stereo).unwrap();
    let g = r.s.graph();
    assert!(
        g.bus_sources(BusInputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    // The removed node's lanes fade out over 256 frames before they go.
    let silent = (0..64).any(|_| all_are(&out0(&mut r.s), 0.0));
    assert!(silent, "the lanes faded out");
}

// ---------------------------------------------------------------------------
// The main output as a bus sink (doc 013, "Buses", B4).
// ---------------------------------------------------------------------------

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(layout(n)).unwrap()
}

/// Both output channels of a committed block.
fn stereo_block(s: &mut Session) -> [Vec<f32>; 2] {
    let _ = s.update();
    let mut block = vec![f32::NAN; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    [0, 1].map(|c| block.iter().skip(c).step_by(2).copied().collect())
}

/// A 5.1 source (FL .5, FR .25, C .125, LFE 0, Ls .0625, Rs .03125) and a
/// declared stereo main output, committed once.
fn main_output_rig() -> (Session, NodeKey) {
    let mut s = offline();
    s.declare_main_output_topology(Some(smpte(2)))
        .expect("stereo");
    let src = s.graph().add(Unforkable(Src {
        values: vec![0.5, 0.25, 0.125, 0.0, 0.0625, 0.03125],
        out: named(6),
    }));
    let _ = s.update();
    (s, src.erase())
}

/// A 5.1 bus into the declared stereo main output renders the ITU rows (front,
/// centre at -3 dB, surround at -3 dB, no LFE) on the outputs, through
/// `declare_main_output_topology`; disconnecting it silences them and the graph is
/// marked edited both ways.
#[test]
fn a_bus_into_the_declared_main_output_renders_the_itu_rows() {
    let (mut s, src) = main_output_rig();
    let g = s.graph();
    g.connect(BusOutputPort::Node(node_bus(src)), MainOutput)
        .expect("5.1 folds to stereo")
        .unfaded();
    assert!(g.has_uncommitted_edits());
    assert_eq!(
        g.bus_sources(MainOutput).unwrap(),
        [BusOutputPort::Node(node_bus(src))]
    );
    let [l, r] = stereo_block(&mut s);
    // The unity lane is a plain addend, the others weighted, in source order.
    let left = ((0.0f32 + 0.5) + f32::MINUS_3_DB * 0.125) + f32::MINUS_3_DB * 0.0625;
    let right = ((0.0f32 + 0.25) + f32::MINUS_3_DB * 0.125) + f32::MINUS_3_DB * 0.03125;
    assert!(all_are(&l, left), "{:?}", &l[..2]);
    assert!(all_are(&r, right), "{:?}", &r[..2]);

    let g = s.graph();
    assert!(
        g.disconnect(BusOutputPort::Node(node_bus(src)), MainOutput)
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(
        !g.disconnect(BusOutputPort::Node(node_bus(src)), MainOutput)
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(g.has_uncommitted_edits());
    let [l, r] = stereo_block(&mut s);
    assert!(all_are(&l, 0.0) && all_are(&r, 0.0));
}

/// An undeclared main output refuses a main output edge by name, storing nothing and
/// marking nothing edited. (No main output is too wide for a bus: the engine's is 8
/// channels, a bus 16.)
#[test]
fn an_undeclared_main_output_refuses_a_bus_edge() {
    let mut r = rig();
    let g = r.s.graph();
    assert_eq!(
        g.connect(BusOutputPort::Node(node_bus(r.stereo)), MainOutput)
            .err(),
        Some(ConnectError::Bus(BusError::MainOutputUndeclared))
    );
    assert!(!g.has_uncommitted_edits());
    assert!(g.bus_sources(MainOutput).unwrap().is_empty());
}

/// A commit the editor refuses is an event of the update that first tried
/// it, and of no later update that fails the same way; once a commit is sent,
/// the next refusal is reported again.
#[test]
fn a_refused_commit_is_an_event_of_its_update() {
    let mut s = offline();
    s.declare_main_output_topology(Some(smpte(2)))
        .expect("stereo");
    let src = s.graph().add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: named(2),
    }));
    s.graph()
        .connect(BusOutputPort::Node(node_bus(src.erase())), MainOutput)
        .unwrap()
        .unfaded();
    let refused = |events: Vec<SessionEvent>| {
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::CommitRefused(_)))
            .count()
    };
    assert_eq!(
        refused(s.update()),
        0,
        "a stereo bus into a stereo main output commits"
    );

    // Named stereo has no place in one unassigned channel.
    s.declare_main_output_topology(Some(ChannelTopology::unassigned(layout(1)).unwrap()))
        .expect("an unassigned mono main output");
    assert_eq!(refused(s.update()), 1, "the refusal is reported once");
    assert_eq!(
        refused(s.update()),
        0,
        "and not again while it fails the same way"
    );

    s.declare_main_output_topology(Some(smpte(2)))
        .expect("stereo again");
    assert_eq!(refused(s.update()), 0, "stereo again commits");
    // A declaration made while that main output crossfade runs waits for it, so the
    // fade (240 frames) renders through first.
    for _ in 0..5 {
        let _ = out0(&mut s);
    }
    s.declare_main_output_topology(Some(ChannelTopology::unassigned(layout(1)).unwrap()))
        .expect("an unassigned mono main output");
    assert_eq!(
        refused(s.update()),
        1,
        "a refusal after a sent commit is new"
    );
}

/// A re-declaration that changes the main output under a main output edge re-lowers it at
/// the next commit (stereo to mono is `0.5 L + 0.5 R`), and one the edge no
/// longer folds to is a refused commit, the plan sent last standing.
#[test]
fn a_redeclared_main_output_relowers_its_edges_and_a_misfit_refuses_the_commit() {
    let mut s = offline();
    s.declare_main_output_topology(Some(smpte(2)))
        .expect("stereo");
    let src = s.graph().add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: named(2),
    }));
    s.graph()
        .connect(BusOutputPort::Node(node_bus(src.erase())), MainOutput)
        .unwrap()
        .unfaded();
    let [l, r] = stereo_block(&mut s);
    assert!(all_are(&l, 0.25) && all_are(&r, 0.75));

    // Same width, another topology: the main output's lanes are re-lowered, so the
    // graph is edited though no width moved (stereo named to stereo unassigned
    // is the identity, so the render is the same).
    s.declare_main_output_topology(Some(ChannelTopology::unassigned(layout(2)).unwrap()))
        .expect("an unassigned pair")
        .unfaded();
    assert!(s.graph_ref().has_uncommitted_edits());
    let [l, r] = stereo_block(&mut s);
    assert!(all_are(&l, 0.25) && all_are(&r, 0.75));

    // A cut: the render is read at the first block after it.
    s.declare_main_output_topology(Some(smpte(1)))
        .expect("mono")
        .unfaded();
    assert!(s.graph_ref().has_uncommitted_edits());
    let [l, _] = stereo_block(&mut s);
    assert!(all_are(&l, 0.5), "{:?}", &l[..2]);

    // Named stereo has no place in one unassigned channel.
    s.declare_main_output_topology(Some(ChannelTopology::unassigned(layout(1)).unwrap()))
        .expect("an unassigned mono main output");
    assert!(matches!(s.commit(), Committed::Refused(_)));
    let [l, _] = stereo_block(&mut s);
    assert!(all_are(&l, 0.5), "the plan sent last stands");
}
