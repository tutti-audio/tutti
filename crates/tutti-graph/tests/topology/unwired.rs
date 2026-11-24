//! Placing a unit at an existing key with fewer ports: the wiring to the
//! ports the new shape lacks leaves the spec and comes back to the caller,
//! the wiring to the ports it keeps stays as it was, and the next commit
//! goes through.

use std::collections::BTreeSet;
use tutti_buffers::InterleavedMut;

use tutti_graph::testing as common;

use common::{Kind, TestNode, prepare};
use tutti_buffers::{ChannelLayout, ChannelTopology, Speaker};
use tutti_graph::{
    Bus, BusOutputPort, BusRole, Buses, Cx, Edge, EdgeFade, EdgeId, Editor, EventEdge,
    EventInputPort, EventOutputPort, Executor, FeedbackFrom, InputPort, Io, Node, NodeBus, NodeKey,
    OutputPort, PortSink, PortSource, Prepare, Shape, Status, TapAlign, TapId, TapOptions, TapSpec,
    TransportPosition, Unforkable, Unwired,
};
use tutti_units::{Gain, Samples, UnitAddr, UnitParam};

/// A node of a given shape that writes silence: the editor's view of it is
/// its ports.
struct Ported(Shape);

impl Node for Ported {
    fn shape(&self) -> Shape {
        self.0
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// `audio` ports each way and `events` event ports each way.
fn ported(audio: u16, events: u16) -> Ported {
    let width = ChannelLayout::from_count(audio);
    Ported(Shape::unassigned(width, width).events(events, events))
}

const A: NodeKey = NodeKey(1);
const X: NodeKey = NodeKey(2);
const B: NodeKey = NodeKey(3);
const DELAY: Samples = Samples::new(128);

fn out(node: NodeKey, port: u16) -> OutputPort {
    OutputPort::new(node, port)
}

fn inp(node: NodeKey, port: u16) -> InputPort {
    InputPort::new(node, port)
}

fn direct(node: NodeKey, port: u16) -> Edge {
    Edge::Direct(PortSource::Node(out(node, port)))
}

fn id(sink: InputPort, from: Edge) -> EdgeId {
    EdgeId::into_port(sink, from)
}

fn eout(node: NodeKey, port: u16) -> EventOutputPort {
    EventOutputPort::new(node, port)
}

fn ein(node: NodeKey, port: u16) -> EventInputPort {
    EventInputPort::new(node, port)
}

/// `A -> X -> B`, three audio and three event ports on each, `X` the key
/// that is placed again; the wiring touches ports 0 and 2 of `X`, and a
/// main output takes `X`'s port 1.
///
/// Audio: `A.0 -> X.0` and `A.2 -> X.2` (both weighted), `X.0 -> B.0`,
/// `X.2 -> B.2`, `X.1 -> output 1`, `X.0 -> output 0`, and the feedback edges
/// `B.2 -> X.2` (into `X`) and `X.2 -> A.2` (out of `X`).
/// Events: `A.0 -> X.0`, `A.2 -> X.2`, `X.0 -> B.0`, `X.2 -> B.2` and the
/// feedback edge `X.2 -> A.1`.
fn rig() -> (Editor, Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    for key in [A, X, B] {
        ed.insert(key, "ported", Unforkable(ported(3, 3)));
    }
    for (at, from) in [
        (inp(X, 0), direct(A, 0)),
        (inp(X, 2), direct(A, 2)),
        (inp(B, 0), direct(X, 0)),
        (inp(B, 2), direct(X, 2)),
        (
            inp(X, 2),
            Edge::Feedback(FeedbackFrom::new(out(B, 2), DELAY)),
        ),
        (
            inp(A, 2),
            Edge::Feedback(FeedbackFrom::new(out(X, 2), DELAY)),
        ),
    ] {
        ed.connect_edge(EdgeId::into_port(at, from)).unwrap();
    }
    ed.wire_outputs(vec![
        PortSource::Node(out(X, 0)),
        PortSource::Node(out(X, 1)),
    ])
    .unwrap();
    ed.connect_events(ein(X, 0), EventEdge::Direct(eout(A, 0)))
        .unwrap();
    ed.connect_events(ein(X, 2), EventEdge::Direct(eout(A, 2)))
        .unwrap();
    ed.connect_events(ein(B, 0), EventEdge::Direct(eout(X, 0)))
        .unwrap();
    ed.connect_events(ein(B, 2), EventEdge::Direct(eout(X, 2)))
        .unwrap();
    ed.connect_events(ein(A, 1), EventEdge::feedback(eout(X, 2), DELAY))
        .unwrap();
    // Weighted lanes: one the shape keeps, one it loses.
    ed.set_edge_fade(id(inp(X, 0), direct(A, 0)), EdgeFade::In)
        .expect("the edge exists");
    ed.set_edge_gain(id(inp(X, 0), direct(A, 0)), Gain::new(0.25))
        .expect("the edge exists");
    ed.set_edge_gain(id(inp(X, 2), direct(A, 2)), Gain::new(0.5))
        .expect("the edge exists");
    ed.commit().expect("the wide rig commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec)
}

fn narrow() -> Ported {
    ported(1, 1)
}

/// The audio edges a one-port `X` loses are exactly those into or out of its
/// ports 1 and 2, direct or feedback, and the ones on port 0 stay.
#[test]
fn a_narrower_shape_drops_exactly_the_edges_to_missing_ports() {
    let (mut ed, _exec) = rig();
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    let dropped: BTreeSet<EdgeId> = placed.unwired.edges.iter().copied().collect();
    let expected: BTreeSet<EdgeId> = [
        id(inp(X, 2), direct(A, 2)),
        id(
            inp(X, 2),
            Edge::Feedback(FeedbackFrom::new(out(B, 2), DELAY)),
        ),
        id(inp(B, 2), direct(X, 2)),
        id(
            inp(A, 2),
            Edge::Feedback(FeedbackFrom::new(out(X, 2), DELAY)),
        ),
        EdgeId {
            sink: PortSink::Output(1),
            from: direct(X, 1),
        },
    ]
    .into();
    assert_eq!(dropped, expected);
    assert_eq!(placed.unwired.edges.len(), expected.len(), "each edge once");

    let t = &ed.spec().topology;
    assert_eq!(t.edges[&inp(X, 0)], vec![direct(A, 0)]);
    assert_eq!(t.edges[&inp(B, 0)], vec![direct(X, 0)]);
    assert!(
        !t.edges.contains_key(&inp(X, 2)),
        "an unfed port has no key"
    );
    assert!(!t.edges.contains_key(&inp(B, 2)));
    assert!(!t.edges.contains_key(&inp(A, 2)));
    assert_eq!(t.outputs[0], vec![PortSource::Node(out(X, 0))]);
    assert!(t.outputs[1].is_empty());
}

/// The event edges to a missing event port go, feedback ones included, and
/// the others stay.
#[test]
fn a_narrower_shape_drops_exactly_the_event_edges_to_missing_ports() {
    let (mut ed, _exec) = rig();
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    let dropped: BTreeSet<(EventInputPort, EventEdge)> =
        placed.unwired.event_edges.iter().copied().collect();
    let expected: BTreeSet<(EventInputPort, EventEdge)> = [
        (ein(X, 2), EventEdge::Direct(eout(A, 2))),
        (ein(B, 2), EventEdge::Direct(eout(X, 2))),
        (ein(A, 1), EventEdge::feedback(eout(X, 2), DELAY)),
    ]
    .into();
    assert_eq!(dropped, expected);
    assert_eq!(placed.unwired.event_edges.len(), expected.len());

    let events = &ed.spec().events;
    assert_eq!(events[&ein(X, 0)], vec![EventEdge::Direct(eout(A, 0))]);
    assert_eq!(events[&ein(B, 0)], vec![EventEdge::Direct(eout(X, 0))]);
    assert!(!events.contains_key(&ein(X, 2)));
    assert!(!events.contains_key(&ein(B, 2)));
    assert!(!events.contains_key(&ein(A, 1)));
}

/// An edge to a port the shape keeps is the same edge afterwards: its gain
/// entry and its declick fade are untouched, and the lane that went takes
/// its entry with it.
#[test]
fn a_surviving_edge_keeps_its_gain_and_its_fade() {
    let (mut ed, _exec) = rig();
    ed.insert(X, "ported", Unforkable(narrow()));
    assert_eq!(
        ed.edge_gain(id(inp(X, 0), direct(A, 0))),
        Some(Gain::new(0.25))
    );
    assert_eq!(
        ed.edge_fade(id(inp(X, 0), direct(A, 0))),
        Some(EdgeFade::In)
    );
    assert!(
        !ed.spec().gains.contains_key(&id(inp(X, 2), direct(A, 2))),
        "the lane of a dropped edge goes with it"
    );
    assert_eq!(ed.spec().gains.len(), 1);
}

/// A narrower shape never refuses the next commit: every edge the spec
/// still holds names a port the unit has.
#[test]
fn the_commit_after_a_narrower_shape_goes_through() {
    let (mut ed, mut exec) = rig();
    ed.insert(X, "ported", Unforkable(narrow()));
    ed.commit().expect("nothing names a port X lacks");
    exec.apply_pending();
    ed.collect();
    let plan = exec.plan().expect("applied");
    assert_eq!(plan.main_outputs(), 2);
}

/// A wider shape unwires nothing, and the wiring is bit for bit what it was.
#[test]
fn a_wider_shape_drops_nothing() {
    let (mut ed, _exec) = rig();
    let before = ed.spec().clone();
    let placed = ed.insert(X, "ported", Unforkable(ported(5, 4)));
    assert!(placed.unwired.is_empty());
    assert_eq!(placed.unwired, Unwired::default());
    let after = ed.spec();
    assert_eq!(after.topology.edges, before.topology.edges);
    assert_eq!(after.topology.outputs, before.topology.outputs);
    assert_eq!(after.events, before.events);
    assert_eq!(after.gains.len(), before.gains.len());
    ed.commit().expect("commits");
}

/// An edge whose declick fade was already taking it out (the caller
/// disconnected it) leaves with the unit's old ports but is not reported:
/// it was gone as far as the caller is concerned.
#[test]
fn an_edge_already_fading_out_is_not_reported() {
    let (mut ed, _exec) = rig();
    let fading = id(inp(X, 2), direct(A, 2));
    ed.set_edge_fade(fading, EdgeFade::Out)
        .expect("the edge exists");
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    assert!(!placed.unwired.edges.contains(&fading));
    assert!(!ed.spec().has_edge(fading));
    assert_eq!(placed.unwired.edges.len(), 4);
}

/// A param port driven from an event output the shape lacks stops being
/// driven, and an edge gain driven from one reads its base.
#[test]
fn a_driver_on_a_missing_event_output_is_released() {
    let (mut ed, _exec) = rig();
    let lane = id(inp(X, 0), direct(A, 0));
    ed.connect_edge_gain(lane, eout(X, 2)).unwrap();
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    assert_eq!(placed.unwired.gains, vec![(lane, eout(X, 2))]);
    assert_eq!(ed.spec().gains[&lane].source, None);
    assert_eq!(ed.edge_gain(lane), Some(Gain::new(0.25)));
}

fn stereo() -> ChannelTopology {
    ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).expect("stereo")
}

fn mono() -> ChannelTopology {
    ChannelTopology::unassigned(ChannelLayout::MONO).expect("mono")
}

/// A node with a main stereo bus and an aux mono bus on each side, or the
/// main bus alone.
fn bused(with_aux: bool) -> Ported {
    let side = |role| {
        let main = Buses::NONE.with(Bus::main(stereo())).expect("one bus");
        if with_aux {
            main.with(Bus::new(mono(), role)).expect("two buses")
        } else {
            main
        }
    };
    Ported(Shape::buses(side(BusRole::Sidechain), side(BusRole::Aux)))
}

/// A bus edge into, or out of, a bus the new shape lacks is dropped and
/// returned by its sink bus; the main bus's edges stay.
#[test]
fn bus_edges_to_a_missing_bus_are_dropped() {
    let (mut ed, _exec) = Editor::new(prepare(128));
    let (s, y, t) = (NodeKey(1), NodeKey(2), NodeKey(3));
    ed.insert(s, "bused", Unforkable(bused(true)));
    ed.insert(y, "bused", Unforkable(bused(true)));
    ed.insert(t, "bused", Unforkable(bused(true)));
    let feeds = [
        (BusOutputPort::Node(NodeBus::new(s, 0)), NodeBus::new(y, 0)),
        (BusOutputPort::Node(NodeBus::new(s, 1)), NodeBus::new(y, 1)),
        (BusOutputPort::Node(NodeBus::new(y, 0)), NodeBus::new(t, 0)),
        (BusOutputPort::Node(NodeBus::new(y, 1)), NodeBus::new(t, 1)),
    ];
    for (from, to) in feeds {
        ed.connect_bus(from, to).expect("declared");
    }
    let placed = ed.insert(y, "bused", Unforkable(bused(false)));
    let dropped: BTreeSet<(NodeBus, BusOutputPort)> = placed
        .unwired
        .bus_edges
        .iter()
        .map(|(to, feed)| (*to, feed.source))
        .collect();
    let expected: BTreeSet<(NodeBus, BusOutputPort)> = [feeds[1], feeds[3]]
        .into_iter()
        .map(|(from, to)| (to, from))
        .collect();
    assert_eq!(dropped, expected);
    assert_eq!(placed.unwired.bus_edges.len(), 2);
    assert!(placed.unwired.edges.is_empty());

    let kept: BTreeSet<(NodeBus, BusOutputPort)> = ed
        .spec()
        .bus_edges
        .iter()
        .flat_map(|(to, feeds)| feeds.iter().map(move |f| (*to, f.source)))
        .collect();
    assert_eq!(
        kept,
        [feeds[0], feeds[2]]
            .into_iter()
            .map(|(from, to)| (to, from))
            .collect()
    );
    ed.commit()
        .expect("the lowering names only buses that exist");
}

/// A feedback edge from a lost output into a lost input of the same key is one
/// edge, listed once.
#[test]
fn a_feedback_self_loop_on_a_lost_port_is_listed_once() {
    let (mut ed, _exec) = rig();
    let looped = id(
        inp(X, 2),
        Edge::Feedback(FeedbackFrom::new(out(X, 2), DELAY)),
    );
    ed.connect_edge(EdgeId::into_port(inp(X, 2), looped.from))
        .unwrap();
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    assert_eq!(
        placed
            .unwired
            .edges
            .iter()
            .filter(|e| **e == looped)
            .count(),
        1
    );
    assert!(!ed.spec().has_edge(looped));
    ed.commit().expect("commits");
}

/// A param port the shape does not declare, and a param port driven from an
/// event output it lost, are released and listed with the output that drove
/// them.
#[test]
fn param_drivers_on_lost_ports_are_released_and_listed() {
    let (mut ed, _exec) = rig();
    // `X` declares a param the narrow shape will not.
    let declares = Ported(ported(3, 3).0.params(&[UnitParam::Volume]));
    ed.insert(X, "ported", Unforkable(declares));
    let undeclared = tutti_graph::ParamIn {
        node: X,
        param: UnitAddr::first(UnitParam::Volume),
    };
    let mix = tutti_graph::ParamIn {
        node: B,
        param: tutti_graph::NodeMix::ADDR,
    };
    ed.connect_param(undeclared, eout(A, 0)).unwrap();
    ed.connect_param(mix, eout(X, 2)).unwrap();
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    let mut listed = placed.unwired.params.clone();
    listed.sort();
    let mut want = vec![(undeclared, eout(A, 0)), (mix, eout(X, 2))];
    want.sort();
    assert_eq!(listed, want);
    assert!(!ed.spec().params.contains_key(&undeclared));
    assert!(!ed.spec().params.contains_key(&mix));
    ed.commit().expect("nothing names a port X lacks");
}

fn stereo_bus_node(aux: Option<Bus>) -> Ported {
    let buses = |role| {
        let main = Buses::NONE.with(Bus::main(stereo())).expect("one bus");
        match aux {
            Some(_) => main
                .with(Bus::banded(stereo(), role, 2).expect("two bands"))
                .expect("two buses"),
            None => main,
        }
    };
    Ported(Shape::buses(buses(BusRole::Sidechain), buses(BusRole::Aux)))
}

/// A banded output bus the new shape lacks drops the feeds of every band into
/// the main output and into another node's bus; the main bus's feeds stay.
#[test]
fn a_lost_banded_output_bus_drops_its_feeds_by_band_and_to_the_main_output() {
    let (mut ed, _exec) = Editor::new(prepare(128));
    let (y, t) = (NodeKey(2), NodeKey(3));
    let banded = Some(Bus::new(stereo(), BusRole::Aux));
    ed.insert(y, "bused", Unforkable(stereo_bus_node(banded)));
    ed.insert(t, "bused", Unforkable(stereo_bus_node(banded)));
    ed.declare_main_output(Some(stereo()), ed.spec().main_output_id);
    let main_to_main_output = BusOutputPort::Node(NodeBus::new(y, 0));
    let band0 = BusOutputPort::Node(NodeBus::new(y, 1).with_band(0));
    let band1 = BusOutputPort::Node(NodeBus::new(y, 1).with_band(1));
    ed.connect_bus_to_main_output(main_to_main_output)
        .expect("declared");
    ed.connect_bus_to_main_output(band1)
        .expect("a band folds to the main output");
    ed.connect_bus(band0, NodeBus::new(t, 1)).expect("declared");
    let placed = ed.insert(y, "bused", Unforkable(stereo_bus_node(None)));
    assert_eq!(
        placed
            .unwired
            .main_output_buses
            .iter()
            .map(|f| f.source)
            .collect::<Vec<_>>(),
        vec![band1]
    );
    assert_eq!(
        placed
            .unwired
            .bus_edges
            .iter()
            .map(|(to, f)| (*to, f.source))
            .collect::<Vec<_>>(),
        vec![(NodeBus::new(t, 1), band0)]
    );
    assert_eq!(
        ed.spec()
            .main_output_sources
            .iter()
            .map(|f| f.source)
            .collect::<Vec<_>>(),
        vec![main_to_main_output]
    );
    ed.commit()
        .expect("the lowering names only buses that exist");
}

/// A tap lane on a port the new shape lacks reads silence and the tap goes on
/// delivering its other lanes; the commit is not refused.
#[test]
fn a_tap_lane_on_a_lost_port_reads_silence() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let konst = |width| Unforkable(TestNode::new(Kind::Const { value: 0.5, width }));
    ed.insert(X, "const", konst(2));
    ed.wire_outputs(vec![PortSource::Node(out(X, 0))]).unwrap();
    let mut tap = ed
        .open_tap(
            TapId::of(1),
            vec![PortSource::Node(out(X, 0)), PortSource::Node(out(X, 1))],
            TapOptions::new(Samples::new(256)).align(TapAlign::Raw),
        )
        .expect("the sources exist");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let placed = ed.insert(X, "const", konst(1));
    assert!(placed.unwired.is_empty(), "a tap is no edge");
    assert!(matches!(
        ed.spec().topology.taps[&TapId::of(1)],
        TapSpec::Ports { .. }
    ));
    ed.commit().expect("a tap on a lost port does not refuse");
    exec.apply_pending();
    ed.collect();
    let mut out0 = vec![0.0f32; 64];
    exec.process(64, &TransportPosition::default(), &[], &mut [&mut out0[..]]);
    let mut buf = vec![f32::NAN; 2 * 64];
    assert_eq!(
        tap.pop_into(InterleavedMut::new(&mut buf, tap.layout()).unwrap())
            .unwrap()
            .get(),
        64
    );
    assert!(buf.chunks(2).all(|f| f[0] == 0.5 && f[1] == 0.0));
}

/// A narrower re-insert while a main output crossfade is set drops the new spec's
/// edges to the lost ports and nothing of the old main output's bank: the bank is
/// built from the committed wiring over the ports the units still have, and
/// the commit carries it.
#[test]
fn a_narrower_reinsert_during_a_main_output_fade_prunes_the_spec_and_not_the_bank() {
    let (mut ed, _exec) = rig();
    ed.set_main_output_fade(Some(tutti_graph::Fade::new(Samples::new(64))));
    ed.declare_main_output(ed.spec().main_output, ed.spec().main_output_id.next());
    let placed = ed.insert(X, "ported", Unforkable(narrow()));
    let lost = EdgeId {
        sink: PortSink::Output(1),
        from: direct(X, 1),
    };
    assert!(placed.unwired.edges.contains(&lost));
    assert_eq!(
        placed
            .unwired
            .edges
            .iter()
            .filter(|e| matches!(e.sink, PortSink::Output(_)))
            .count(),
        1,
        "only the new main output's own output edge: nothing of the bank is in the spec"
    );
    ed.commit().expect("commits with the bank");
    let plan = ed.base().expect("sent");
    assert!(
        plan.main_output_bank().is_some(),
        "the old main output's surviving output is carried"
    );
    assert_eq!(
        plan.rendered_outputs(),
        4,
        "the new main output's two channels and the old main output's two"
    );
}
