//! The compiler's passes, one claim at a time: order, cycles, the latency
//! solve against `tutti_graph`, colouring, coarsening, placement.
//!
//! Thirteen fixed shapes (`shapes()`) cover the latency cases.
//! `tutti_graph` is the pure pass the plan's delays must equal.

use tutti_graph::testing as common;

use std::collections::BTreeMap;

use common::{Harness, Kind, SpecBehaviour, input_signal};
use tutti_buffers::ChannelLayout;
use tutti_graph::{
    AudioDelayKey, BusOutputPort, CompileError, CycleEdge, EventEdge, EventInputPort,
    EventOutputPort, GraphInvalid, GraphSpec, Op, PortKind, Shape, Shapes, Unforkable, compile,
};
use tutti_graph::{Edge, FeedbackFrom, InputPort, Invalid, NodeSpec, OutputPort, PortSource};
use tutti_graph::{NodeBus, NodeKey, ParamIn, Topology, delays, latency_plan};
use tutti_units::{Latency, SampleRate, Samples, Tail, UnitParam};

/// The `MaxBlock` every plan in this file is compiled for.
const PREP: usize = 128;

const A: NodeKey = NodeKey(10);
const B: NodeKey = NodeKey(20);
const C: NodeKey = NodeKey(30);
const D: NodeKey = NodeKey(40);

fn spec(kind: &str, ins: u16, outs: u16) -> NodeSpec {
    NodeSpec::new(
        kind,
        ChannelLayout::from_count(ins),
        ChannelLayout::from_count(outs),
    )
    .tail(Tail::None)
}

fn at(node: NodeKey, port: u16) -> InputPort {
    InputPort::new(node, port)
}

fn out(node: NodeKey, port: u16) -> OutputPort {
    OutputPort::new(node, port)
}

fn edge(t: &mut Topology, sink: InputPort, from: OutputPort) {
    t.edges
        .insert(sink, vec![Edge::Direct(PortSource::Node(from))]);
}

/// `topology_compile.rs`'s thirteen shapes, verbatim.
fn shapes() -> Vec<(&'static str, Topology)> {
    let mut all = Vec::new();

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.set_outputs(vec![PortSource::Node(out(B, 0))]);
    all.push(("chain", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    t.set_outputs(vec![
        PortSource::Node(out(B, 0)),
        PortSource::Node(out(C, 0)),
    ]);
    all.push(("fan_out", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("dc", 0, 1));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(C, 1), out(B, 0));
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    all.push(("mix_fan_in", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    t.nodes.insert(D, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(D, 0), out(B, 0));
    edge(&mut t, at(D, 1), out(C, 0));
    t.set_outputs(vec![PortSource::Node(out(D, 0))]);
    all.push(("diamond", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(512)));
    t.nodes.insert(C, spec("gain", 1, 1));
    t.nodes.insert(D, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(D, 0), out(B, 0));
    edge(&mut t, at(D, 1), out(C, 0));
    t.set_outputs(vec![PortSource::Node(out(D, 0))]);
    all.push(("pdc_diamond", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(128)));
    t.nodes
        .insert(C, spec("gain", 1, 1).latency(Samples::new(64)));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    all.push(("serial_latency", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 2));
    t.set_outputs(vec![
        PortSource::Node(out(A, 0)),
        PortSource::Node(out(A, 1)),
    ]);
    all.push(("multi_output", t));

    let mut t = Topology {
        inputs: ChannelLayout::STEREO,
        ..Topology::default()
    };
    t.nodes.insert(A, spec("fan", 2, 2));
    t.edges
        .insert(at(A, 0), vec![Edge::Direct(PortSource::MainInput(0))]);
    t.edges
        .insert(at(A, 1), vec![Edge::Direct(PortSource::MainInput(1))]);
    t.set_outputs(vec![
        PortSource::Node(out(A, 0)),
        PortSource::Node(out(A, 1)),
    ]);
    all.push(("main_input", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.set_outputs(vec![PortSource::Node(out(B, 0))]);
    all.push(("zero_sourced_port", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.set_outputs(vec![PortSource::Node(out(A, 0)), PortSource::Zero]);
    all.push(("silent_output_channel", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 6));
    t.nodes.insert(B, spec("fan", 6, 6));
    for port in 0..6 {
        edge(&mut t, at(B, port), out(A, port));
    }
    t.set_outputs((0..6).map(|p| PortSource::Node(out(B, p))));
    all.push(("six_channel", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(
        B,
        spec("gain", 1, 1).tail(Tail::Finite(Samples::new(2_048))),
    );
    t.nodes
        .insert(C, spec("gain", 1, 1).tail(Tail::Finite(Samples::new(256))));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    all.push(("tail_chain", t));

    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(999)));
    edge(&mut t, at(B, 0), out(A, 0));
    t.set_outputs(vec![PortSource::Node(out(A, 0))]);
    all.push(("orphan_branch", t));

    all
}

/// A `Spec` test node per spec — its declared latency and tail, its kind's
/// behaviour.
fn kinds_for(t: &Topology) -> BTreeMap<NodeKey, Kind> {
    t.nodes
        .iter()
        .map(|(&k, s)| {
            let behaviour = match s.kind.as_str() {
                "dc" => SpecBehaviour::Dc(0.5),
                "gain" => SpecBehaviour::Gain(0.75),
                "sum" => SpecBehaviour::Sum,
                _ => SpecBehaviour::Fan,
            };
            (
                k,
                Kind::Spec {
                    behaviour,
                    ins: s.inputs.count() as usize,
                    outs: s.outputs.count() as usize,
                    latency: s.latency.get(),
                    tail: s.tail,
                },
            )
        })
        .collect()
}

/// The shapes a spec implies, without building any node.
fn shapes_of_spec(t: &Topology) -> Shapes {
    t.nodes
        .iter()
        .map(|(&k, s)| {
            (
                k,
                Shape::buses(
                    tutti_graph::Buses::unassigned(s.inputs),
                    tutti_graph::Buses::unassigned(s.outputs),
                )
                .latency(Latency::new(s.latency))
                .tail(s.tail),
            )
        })
        .collect()
}

/// The nodes in the order the plan runs them: its node ops, in op order.
fn order(plan: &tutti_graph::Plan) -> Vec<NodeKey> {
    plan.ops()
        .iter()
        .filter_map(|op| match *op {
            Op::Node { unit, .. } => Some(plan.units()[unit.index()].key),
            _ => None,
        })
        .collect()
}

fn compiled(t: &Topology) -> tutti_graph::Plan {
    let valid = GraphSpec::new(t.clone()).validate().expect("valid");
    compile(&valid, &shapes_of_spec(t), &common::prepare(PREP), None)
        .expect("compiles")
        .0
}

// ---------------------------------------------------------------------------
// Fan-in.
// ---------------------------------------------------------------------------

/// One source stays an alias (no `Op::Sum`), none is silence, two or more at
/// an input port or a main output channel are one sum op; 65 are a chain
/// of two.
#[test]
fn one_source_is_an_alias_and_a_fan_in_is_a_sum_op() {
    let sums = |t: &Topology| {
        let valid = GraphSpec::new(t.clone()).validate().expect("valid");
        let (plan, _) =
            compile(&valid, &shapes_of_spec(t), &common::prepare(PREP), None).expect("compiles");
        plan.ops()
            .iter()
            .filter(|o| matches!(o, tutti_graph::Op::Sum { .. }))
            .count()
    };
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("dc", 0, 1));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(C, 0), out(A, 0));
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    assert_eq!(sums(&t), 0, "one source and none are aliases");

    t.connect(at(C, 0), Edge::Direct(PortSource::Node(out(B, 0))));
    assert_eq!(sums(&t), 1, "two sources at a port");

    assert!(t.disconnect(at(C, 0), Edge::Direct(PortSource::Node(out(B, 0)))));
    assert_eq!(sums(&t), 0, "back to one source: an alias again");
    t.outputs = vec![vec![
        PortSource::Node(out(C, 0)),
        PortSource::Node(out(A, 0)),
    ]];
    assert_eq!(sums(&t), 1, "two sources at an output channel");

    // 65 addends: 64 in the first op, the chain's result and one more.
    let mut t = Topology::default();
    t.nodes.insert(C, spec("sum", 1, 1));
    for k in 0..65u32 {
        let n = NodeKey(100 + u64::from(k));
        t.nodes.insert(n, spec("dc", 0, 1));
        t.connect(at(C, 0), Edge::Direct(PortSource::Node(out(n, 0))));
    }
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    assert_eq!(sums(&t), 2, "a chain of two ops");
}

// ---------------------------------------------------------------------------
// Order.
// ---------------------------------------------------------------------------

/// With no event edges, the compiler's one order *is* `Topology::topo_order`.
#[test]
fn the_order_is_topology_topo_order() {
    assert_eq!(shapes().len(), 13, "the shape list is the coverage");
    for (name, t) in shapes() {
        assert_eq!(
            order(&compiled(&t)),
            t.topo_order().expect("acyclic"),
            "{name}"
        );
    }
}

/// Ties break toward the smaller key even when the dependents arrive through
/// different edge kinds. `X` feeds `B` by audio and `A` by events; both
/// become ready together, and `A < B` must run first.
#[test]
fn ties_break_by_key_across_edge_kinds() {
    let x = NodeKey(1);
    let (a, b) = (NodeKey(5), NodeKey(9));
    let mut t = Topology::default();
    t.nodes.insert(x, spec("src", 0, 1));
    t.nodes.insert(a, spec("ev", 0, 0));
    t.nodes.insert(b, spec("gain", 1, 1));
    edge(&mut t, at(b, 0), out(x, 0));
    let mut g = GraphSpec::new(t);
    g.connect_events(
        EventInputPort::new(a, 0),
        EventEdge::Direct(EventOutputPort::new(x, 0)),
    );
    let shapes: Shapes = [
        (
            x,
            Shape::source_unassigned(ChannelLayout::MONO).events(0, 1),
        ),
        (a, Shape::EMPTY.events(1, 0)),
        (b, Shape::through_unassigned(ChannelLayout::MONO)),
    ]
    .into();
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(order(&plan), [x, a, b]);
}

/// An event input is read through a view its node op writes: the view is
/// never the slot it is merged from (the op reads the one and writes the
/// other), each input has a view of its own, and an input nothing feeds
/// reads the empty slot.
#[test]
fn an_event_inputs_view_is_its_own_slot_written_by_the_node_op() {
    let (x, a) = (NodeKey(1), NodeKey(5));
    let mut t = Topology::default();
    t.nodes.insert(x, spec("src", 0, 0));
    t.nodes.insert(a, spec("ev", 0, 0));
    let mut g = GraphSpec::new(t);
    g.connect_events(
        EventInputPort::new(a, 0),
        EventEdge::Direct(EventOutputPort::new(x, 0)),
    );
    let shapes: Shapes = [
        (x, Shape::EMPTY.events(0, 1)),
        (a, Shape::EMPTY.events(2, 0)),
    ]
    .into();
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    let (src, views): (Vec<_>, Vec<_>) = plan
        .ops()
        .iter()
        .find_map(|op| match *op {
            Op::Node {
                unit,
                event_src,
                event_in,
                ..
            } if plan.units()[unit.index()].key == a => Some((
                plan.event_list()[event_src.range()].to_vec(),
                plan.event_list()[event_in.range()].to_vec(),
            )),
            _ => None,
        })
        .expect("the node op");
    assert_eq!((src.len(), views.len()), (2, 2), "a view for each input");
    assert!(
        views.iter().all(|v| !src.contains(v)),
        "a view is not its source"
    );
    assert_ne!(views[0], views[1], "each input its own view");
    assert_eq!(
        src[1],
        tutti_graph::EventSlotId::EMPTY,
        "an unwired input reads the empty slot"
    );
    plan.verify().expect("sound");
}

// ---------------------------------------------------------------------------
// Latency: the plan's delays are the solve's gaps.
// ---------------------------------------------------------------------------

/// Output channel 0 runs through a 512-frame node and channel 1 is dry: the
/// dry channel is delayed by exactly 512 at the output, the latent one by
/// nothing, and the pre-roll is 0 and 512.
#[test]
fn a_dry_output_channel_is_delayed_to_the_latent_one() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(512)));
    edge(&mut t, at(B, 0), out(A, 0));
    t.set_outputs(vec![
        PortSource::Node(out(B, 0)),
        PortSource::Node(out(A, 0)),
    ]);
    let plan = compiled(&t);
    let delays: Vec<_> = plan.audio_delays().iter().map(|d| (d.key, d.len)).collect();
    assert_eq!(
        delays,
        vec![(
            AudioDelayKey::Output {
                channel: 1,
                from: PortSource::Node(out(A, 0)),
            },
            Samples::new(512)
        )]
    );
    assert_eq!(plan.total_latency().samples(), Samples::new(512));
    assert_eq!(plan.compensation(), &[Samples::new(0), Samples::new(512)]);
}

/// The solve against an analytic figure: a diamond whose branches carry 3 and
/// 7 frames merges at 7, so the short branch is delayed by exactly 4 into the
/// merge, nothing else is delayed, and the plan's total latency is 7.
#[test]
fn a_diamond_of_3_and_7_delays_the_short_branch_by_4() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(3)));
    t.nodes
        .insert(C, spec("gain", 1, 1).latency(Samples::new(7)));
    t.nodes.insert(D, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(A, 0));
    edge(&mut t, at(D, 0), out(B, 0));
    edge(&mut t, at(D, 1), out(C, 0));
    t.set_outputs(vec![PortSource::Node(out(D, 0))]);
    let plan = compiled(&t);
    assert!(plan.event_delays().is_empty(), "nothing else is delayed");
    let delays: Vec<_> = plan.audio_delays().iter().map(|d| (d.key, d.len)).collect();
    assert_eq!(
        delays,
        vec![(
            AudioDelayKey::Edge {
                at: at(D, 0),
                from: PortSource::Node(out(B, 0)),
            },
            Samples::new(4)
        )]
    );
    assert_eq!(plan.total_latency().samples(), Samples::new(7));
    assert_eq!(plan.compensation(), &[Samples::new(0)]);
}

/// What shape `name`'s output channel `c` holds at `frame`: the nodes'
/// behaviours (`kinds_for`: a `dc` is 0.5, a `gain` 0.75, a `sum` adds, a
/// `fan` copies) with the ring PDC puts on the shorter path of a latent merge.
fn figure(name: &str, c: usize, frame: u64) -> f32 {
    let (dc, gain) = (0.5f32, 0.75f32);
    match name {
        "chain" | "fan_out" => dc * gain,
        "mix_fan_in" => dc + dc,
        "diamond" => dc * gain + dc * gain,
        // The 512-frame latency is declared, not realised: the latent branch
        // sounds at once and the other waits in its ring.
        "pdc_diamond" => dc * gain + if frame >= 512 { dc * gain } else { 0.0 },
        "serial_latency" | "tail_chain" => dc * gain * gain,
        "multi_output" | "zero_sourced_port" | "orphan_branch" | "six_channel" => dc,
        "silent_output_channel" => [dc, 0.0][c],
        // Input channel `c` is the run's input scaled by 2^-c.
        "main_input" => input_signal::<f32>(frame, 1)[0] * 0.5f32.powi(c as i32),
        other => unreachable!("{other} has no figure"),
    }
}

/// The rings the plan inserts actually align the paths: rendered through the
/// executor, each of the thirteen shapes holds its figure sample for sample,
/// at a block size that makes the 512-frame ring span blocks.
#[test]
fn the_shapes_render_their_figures() {
    for (name, t) in shapes() {
        let kinds = kinds_for(&t);
        let valid = GraphSpec::new(t.clone()).validate().expect("valid");
        let mut harness = Harness::new(128);
        harness.switch(&valid, &kinds);
        let mut frame = 0u64;
        for _ in 0..12 {
            let n = 100;
            let got = harness.block(n, &input_signal(frame, n));
            for (c, channel) in got.iter().enumerate() {
                for (i, &y) in channel.iter().enumerate() {
                    let f = frame + i as u64;
                    assert_eq!(y, figure(name, c, f), "{name}, channel {c}, frame {f}");
                }
            }
            frame += n as u64;
        }
    }
}

// ---------------------------------------------------------------------------
// Cycles.
// ---------------------------------------------------------------------------

fn gain_kinds(keys: &[NodeKey]) -> Shapes {
    keys.iter()
        .map(|&k| {
            (
                k,
                Shape::through_unassigned(ChannelLayout::MONO).events(1, 1),
            )
        })
        .collect()
}

fn two_node_spec() -> GraphSpec {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("n", 1, 1));
    t.nodes.insert(B, spec("n", 1, 1));
    GraphSpec::new(t)
}

/// A purely audio cycle is the value's own error, reported by
/// `Topology::validate` exactly as before.
#[test]
fn an_audio_cycle_is_invalid() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    edge(&mut g.topology, at(A, 0), out(B, 0));
    let errs = g.validate().expect_err("cyclic");
    assert!(
        errs.iter()
            .any(|e| matches!(e, GraphInvalid::Topology(Invalid::Cycle { .. })))
    );
}

/// A cycle that runs through an event edge is only visible to the compiler,
/// and its error names every edge in the cycle — by port.
#[test]
fn a_cycle_through_an_event_edge_is_an_error_naming_its_edges() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    g.connect_events(
        EventInputPort::new(A, 0),
        EventEdge::Direct(EventOutputPort::new(B, 0)),
    );
    let valid = g.validate().expect("the value alone cannot see it");
    let err =
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).expect_err("cyclic");
    assert_eq!(
        err,
        CompileError::Cycle {
            edges: vec![
                CycleEdge::Audio(at(B, 0)),
                CycleEdge::Event {
                    at: EventInputPort::new(A, 0),
                    from: EventOutputPort::new(B, 0)
                },
            ]
        }
    );
}

/// A lane's param edge that closes a cycle through an audio edge is found on
/// the spec alone, with the edges `compile` reports, where
/// `Topology::validate` (audio edges only) passes. A feedback edge cuts it,
/// and an acyclic spec has none.
#[test]
fn a_spec_names_the_cycle_a_param_edge_closes() {
    let param = ParamIn {
        node: A,
        param: UnitParam::Cutoff.into(),
    };
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    assert_eq!(g.cycle(), None, "a chain is acyclic");
    g.connect_param(param, EventOutputPort::new(B, 0));
    g.topology
        .validate()
        .expect("the audio edges alone are acyclic");
    let want = vec![
        CycleEdge::Audio(at(B, 0)),
        CycleEdge::Param {
            at: param,
            from: EventOutputPort::new(B, 0),
        },
    ];
    assert_eq!(g.cycle(), Some(want));

    // The edges are the ones `compile` reports for an event edge closing the
    // same loop (`a_cycle_through_an_event_edge_is_an_error_naming_its_edges`).
    let mut e = two_node_spec();
    edge(&mut e.topology, at(B, 0), out(A, 0));
    e.connect_events(
        EventInputPort::new(A, 0),
        EventEdge::Direct(EventOutputPort::new(B, 0)),
    );
    let valid = e.validate().expect("the value alone cannot see it");
    assert_eq!(
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None),
        Err(CompileError::Cycle {
            edges: e.cycle().expect("a cycle")
        }),
        "one search: compile reports the edges the spec does"
    );

    let mut f = two_node_spec();
    edge(&mut f.topology, at(B, 0), out(A, 0));
    f.topology.edges.insert(
        at(A, 0),
        vec![Edge::Feedback(FeedbackFrom::one_block(
            out(B, 0),
            Samples::new(PREP),
        ))],
    );
    assert_eq!(f.cycle(), None, "a feedback edge reads the past");
}

/// A bus edge not yet lowered is a dependency at node level: A feeding B's bus
/// and B driving A's param is a cycle the spec names, and without the param
/// edge there is none. A feed from the main input is no dependency.
#[test]
fn a_spec_names_the_cycle_an_unlowered_bus_edge_closes() {
    let param = ParamIn {
        node: A,
        param: UnitParam::Cutoff.into(),
    };
    let (source, sink) = (NodeBus::new(A, 0), NodeBus::new(B, 0));
    let mut g = two_node_spec();
    g.connect_bus(BusOutputPort::Node(source), sink);
    assert_eq!(g.cycle(), None, "a bus chain is acyclic");
    g.connect_bus(BusOutputPort::MainInput, NodeBus::new(A, 0));
    assert_eq!(g.cycle(), None, "the main input is no node");
    g.connect_param(param, EventOutputPort::new(B, 0));
    let mut want = vec![
        CycleEdge::Param {
            at: param,
            from: EventOutputPort::new(B, 0),
        },
        CycleEdge::Bus {
            at: sink,
            from: BusOutputPort::Node(source),
        },
    ];
    want.sort();
    assert_eq!(g.cycle(), Some(want));
}

/// An event edge from a node to itself is a cycle too.
#[test]
fn an_event_self_loop_is_a_cycle() {
    let mut g = two_node_spec();
    g.connect_events(
        EventInputPort::new(A, 0),
        EventEdge::Direct(EventOutputPort::new(A, 0)),
    );
    let valid = g.validate().expect("valid value");
    assert!(matches!(
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None),
        Err(CompileError::Cycle { .. })
    ));
}

/// Feedback edges — audio and event — break those same cycles, and become
/// captures read next block.
#[test]
fn feedback_edges_break_cycles() {
    let mut g = two_node_spec();
    edge(&mut g.topology, at(B, 0), out(A, 0));
    g.topology.edges.insert(
        at(A, 0),
        vec![Edge::Feedback(FeedbackFrom::one_block(
            out(B, 0),
            Samples::new(PREP),
        ))],
    );
    g.connect_events(
        EventInputPort::new(A, 0),
        EventEdge::feedback(EventOutputPort::new(B, 0), Samples::new(PREP)),
    );
    let valid = g.validate().expect("feedback breaks the cycle");
    let (plan, _) =
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).expect("compiles");
    plan.verify().expect("sound");
    assert_eq!(plan.audio_feedback().len(), 1);
    assert_eq!(plan.event_feedback().len(), 1);
    let captures = plan
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Capture { .. } | Op::EventCapture { .. }))
        .count();
    assert_eq!(captures, 2);
}

// ---------------------------------------------------------------------------
// Shape checks.
// ---------------------------------------------------------------------------

/// A unit whose shape disagrees with its spec, a node with no shape, an event
/// port past a node's declared ports, and an over-wide node are all compile
/// errors naming the node.
#[test]
fn shape_disagreements_are_errors() {
    let g = two_node_spec();
    let valid = g.validate().unwrap();
    let mut shapes = gain_kinds(&[A, B]);
    shapes.insert(
        A,
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::STEREO),
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
        ),
    );
    assert!(matches!(
        compile(&valid, &shapes, &common::prepare(PREP), None),
        Err(CompileError::WidthMismatch { node: A, .. })
    ));

    let shapes = gain_kinds(&[A]);
    assert_eq!(
        compile(&valid, &shapes, &common::prepare(PREP), None).unwrap_err(),
        CompileError::MissingShape { node: B }
    );

    let mut g = two_node_spec();
    g.connect_events(
        EventInputPort::new(A, 3),
        EventEdge::Direct(EventOutputPort::new(B, 0)),
    );
    let valid = g.validate().unwrap();
    assert_eq!(
        compile(&valid, &gain_kinds(&[A, B]), &common::prepare(PREP), None).unwrap_err(),
        CompileError::EventPortOutOfRange {
            at: EventInputPort::new(A, 3),
            from: None
        }
    );

    let mut t = Topology::default();
    t.nodes.insert(A, spec("wide", 0, 65));
    let valid = GraphSpec::new(t).validate().unwrap();
    let shapes: Shapes = [(A, Shape::source_unassigned(ChannelLayout::from_count(65)))].into();
    assert_eq!(
        compile(&valid, &shapes, &common::prepare(PREP), None).unwrap_err(),
        CompileError::TooManyPorts { node: A, count: 65 }
    );
}

// ---------------------------------------------------------------------------
// Colouring.
// ---------------------------------------------------------------------------

/// A long serial chain reuses slots — it does not need one per edge.
#[test]
fn a_chain_reuses_slots() {
    let mut t = Topology::default();
    let keys: Vec<NodeKey> = (0..40).map(NodeKey).collect();
    t.nodes.insert(keys[0], spec("dc", 0, 1));
    for w in keys.windows(2) {
        t.nodes.insert(w[1], spec("gain", 1, 1));
        edge(&mut t, at(w[1], 0), out(w[0], 0));
    }
    t.set_outputs(vec![PortSource::Node(out(keys[39], 0))]);
    let plan = compiled(&t);
    plan.verify().expect("sound");
    // Zero slot + at most two alternating coloured slots.
    assert!(
        plan.slots(PortKind::Audio) <= 3,
        "{} slots for a chain",
        plan.slots(PortKind::Audio)
    );
}

/// The case serial liveness gets wrong. In serial order `[A, B, C]`, `A`'s
/// value is dead after `B`, so a serial allocator hands its slot to `C`. But
/// `C` does not depend on `A` or `B` — a parallel executor may run `C`
/// alongside `B`, and `C` would overwrite what `B` is reading. The partial
/// order forbids the share.
#[test]
fn colouring_respects_the_partial_order_not_the_serial_one() {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 1, 1));
    t.nodes.insert(C, spec("dc", 0, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    t.set_outputs(vec![
        PortSource::Node(out(B, 0)),
        PortSource::Node(out(C, 0)),
    ]);
    let plan = compiled(&t);
    assert_eq!(order(&plan), [A, B, C], "the serial order the test assumes");
    let slot_of = |key: NodeKey| {
        let unit = plan.units().iter().position(|u| u.key == key).unwrap();
        plan.ops()
            .iter()
            .find_map(|op| match *op {
                Op::Node {
                    unit: u, audio_out, ..
                } if u.index() == unit => Some(plan.audio_list()[audio_out.range().start]),
                _ => None,
            })
            .unwrap()
    };
    assert_ne!(slot_of(A), slot_of(C), "C may run beside B, which reads A");
    plan.verify().expect("sound");
}

fn in_place_spec(ports_read_twice: bool) -> (GraphSpec, Shapes) {
    let mut t = Topology::default();
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes.insert(B, spec("gain", 2, 2));
    edge(&mut t, at(B, 0), out(A, 0));
    if ports_read_twice {
        edge(&mut t, at(B, 1), out(A, 0));
    }
    t.set_outputs(vec![
        PortSource::Node(out(B, 0)),
        PortSource::Node(out(B, 1)),
    ]);
    let shapes: Shapes = [
        (A, Shape::source_unassigned(ChannelLayout::MONO)),
        (
            B,
            Shape::through_unassigned(ChannelLayout::STEREO).in_place(),
        ),
    ]
    .into();
    (GraphSpec::new(t), shapes)
}

/// In-place aliasing happens when it is legal: the node opted in, the input's
/// last reader is this node, and it reads the value on one port only.
#[test]
fn in_place_aliasing_happens_exactly_when_legal() {
    let (g, shapes) = in_place_spec(false);
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert!(plan.in_place(B).get(0), "sole reader, one port: aliased");

    let (g, shapes) = in_place_spec(true);
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0, "read on two ports: not aliased");

    // Not opted in: never aliased.
    let (g, mut shapes) = in_place_spec(false);
    shapes.insert(B, Shape::through_unassigned(ChannelLayout::STEREO));
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0);

    // Another reader that may run concurrently: not aliased.
    let (mut g, shapes) = in_place_spec(false);
    let mut shapes = shapes;
    g.topology.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut g.topology, at(C, 0), out(A, 0));
    g.topology.outputs.push(vec![PortSource::Node(out(C, 0))]);
    common::declare_spec_outputs(&mut g);
    shapes.insert(C, Shape::through_unassigned(ChannelLayout::MONO));
    let plan = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap()
    .0;
    assert_eq!(plan.in_place(B).0, 0, "C still reads A's slot");
}

// ---------------------------------------------------------------------------
// Coarsening and placement.
// ---------------------------------------------------------------------------

/// A pure chain coarsens to one task; a diamond does not.
#[test]
fn chains_coarsen_into_one_task() {
    let mut t = Topology::default();
    let keys: Vec<NodeKey> = (0..10).map(NodeKey).collect();
    t.nodes.insert(keys[0], spec("dc", 0, 1));
    for w in keys.windows(2) {
        t.nodes.insert(w[1], spec("gain", 1, 1));
        edge(&mut t, at(w[1], 0), out(w[0], 0));
    }
    t.set_outputs(vec![PortSource::Node(out(keys[9], 0))]);
    let plan = compiled(&t);
    assert_eq!(plan.tasks().len(), 1, "10 nodes + output op, one chain");
    assert_eq!(plan.task_activation(), &[0]);

    let diamond = shapes()
        .into_iter()
        .find(|(n, _)| *n == "diamond")
        .unwrap()
        .1;
    let plan = compiled(&diamond);
    assert!(plan.tasks().len() > 1);
    // Every op is in exactly one task.
    let mut seen: Vec<usize> = plan.task_ops().iter().map(|o| o.index()).collect();
    seen.sort();
    assert_eq!(seen, (0..plan.ops().len()).collect::<Vec<_>>());
}

/// Placement diffs keys, not shapes: a surviving key keeps its index, a
/// removed one retires, a regenerated one is replaced in place, and a new one
/// takes the lowest free index.
#[test]
fn the_delta_moves_only_what_changed() {
    let mut t = Topology::default();
    for k in [A, B, C] {
        t.nodes.insert(k, spec("dc", 0, 1));
    }
    let shapes = shapes_of_spec(&t);
    let (first, delta) = compile(
        &GraphSpec::new(t.clone()).validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .unwrap();
    assert_eq!(delta.insert().len(), 3);
    let idx = |p: &tutti_graph::Plan, k| p.unit(k).unwrap().idx;

    let mut g = GraphSpec::new(t);
    g.topology.nodes.remove(&B);
    g.topology.nodes.insert(D, spec("dc", 0, 1));
    g.generations.insert(C, 1);
    let mut shapes = shapes_of_spec(&g.topology);
    shapes.remove(&B);
    let (second, delta) = compile(
        &g.validate().unwrap(),
        &shapes,
        &common::prepare(PREP),
        Some(&first),
    )
    .unwrap();

    assert_eq!(idx(&second, A), idx(&first, A), "A kept its place");
    assert_eq!(delta.retire().len(), 1);
    assert_eq!(delta.retire()[0].key, B);
    assert_eq!(delta.replace().len(), 1);
    assert_eq!(delta.replace()[0].0.key, C);
    assert_eq!(delta.replace()[0].1.generation, 1);
    assert_eq!(idx(&second, C), idx(&first, C), "replaced in place");
    assert_eq!(delta.insert().len(), 1);
    assert_eq!(delta.insert()[0].key, D);
    assert_eq!(idx(&second, D), idx(&first, B), "D reuses B's freed index");
    assert_eq!(delta.store_len(), 3);
}

/// A `PortSource::MainInput` input that merges with a latent path **is** delayed
/// to align, like any other merge-point source, and the compiler and
/// `latency::delays` agree on it.
#[test]
fn a_main_input_merging_with_a_latent_path_is_delayed() {
    let mut t = Topology {
        inputs: ChannelLayout::MONO,
        ..Topology::default()
    };
    t.nodes.insert(A, spec("dc", 0, 1));
    t.nodes
        .insert(B, spec("gain", 1, 1).latency(Samples::new(48)));
    t.nodes.insert(C, spec("sum", 2, 1));
    edge(&mut t, at(B, 0), out(A, 0));
    edge(&mut t, at(C, 0), out(B, 0));
    t.edges
        .insert(at(C, 1), vec![Edge::Direct(PortSource::MainInput(0))]);
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);

    let plan = compiled(&t);
    let key = AudioDelayKey::Edge {
        at: at(C, 1),
        from: PortSource::MainInput(0),
    };
    assert_eq!(plan.audio_delay(key), Samples::new(48));
    // The pure pass: the main input departs at zero, the node arrives at 48.
    assert_eq!(
        delays(&t, SampleRate::SR_48K).arrival(C),
        Samples::new(48),
        "latency::delays aligns the main input to the arrival the plan does"
    );

    let mut harness = Harness::new(128);
    harness.switch(
        &GraphSpec::new(t.clone()).validate().unwrap(),
        &kinds_for(&t),
    );
    // The gain's 0.375 sounds at once; the main input arrives 48 frames late.
    let mut frame = 0u64;
    for _ in 0..6 {
        let got = harness.block(100, &input_signal(frame, 100));
        for (i, &y) in got[0].iter().enumerate() {
            let f = frame + i as u64;
            let late = f
                .checked_sub(48)
                .map_or(0.0, |m| input_signal::<f32>(m, 1)[0]);
            assert_eq!(y, 0.375 + late, "frame {f}");
        }
        frame += 100;
    }
}

/// **An event source counts toward arrival in both solves.** A node whose
/// audio input is dry and whose events come from a latent node arrives at
/// that node's departure: the compiler delays the audio port to it, and
/// `latency::plan` over the whole spec (`GraphSpec`'s `LatencyGraph`, which
/// lists event and param sources) reports the same compensation, where over
/// the topology alone it would miss the event path.
#[test]
fn an_event_source_counts_toward_arrival_in_both_solves() {
    // A (64 frames latent, emits events) ─events─▶ C;  B (dry) ─audio─▶ C.
    let mut t = Topology::default();
    t.nodes
        .insert(A, spec("emit", 0, 0).latency(Samples::new(64)));
    t.nodes.insert(B, spec("dc", 0, 1));
    t.nodes.insert(C, spec("gain", 1, 1));
    edge(&mut t, at(C, 0), out(B, 0));
    t.set_outputs(vec![PortSource::Node(out(C, 0))]);
    let mut g = GraphSpec::new(t.clone());
    g.connect_events(
        EventInputPort::new(C, 0),
        EventEdge::Direct(EventOutputPort::new(A, 0)),
    );
    let mut shapes = shapes_of_spec(&t);
    shapes.insert(
        A,
        Shape::EMPTY
            .latency(Latency::new(Samples::new(64)))
            .tail(Tail::None)
            .events(0, 1),
    );
    shapes.insert(
        C,
        Shape::through_unassigned(ChannelLayout::MONO)
            .tail(Tail::None)
            .events(1, 0),
    );
    let plan = compile(
        &g.validate().expect("valid"),
        &shapes,
        &common::prepare(PREP),
        None,
    )
    .expect("compiles")
    .0;

    let key = AudioDelayKey::Edge {
        at: at(C, 0),
        from: PortSource::Node(out(B, 0)),
    };
    assert_eq!(
        plan.audio_delay(key),
        Samples::new(64),
        "the dry port waits for the events"
    );
    let comp = latency_plan(&g, SampleRate::SR_48K);
    assert_eq!(comp.total(), Samples::new(64));
    assert_eq!(plan.total_latency().samples(), comp.total());
    assert_eq!(plan.compensation(), comp.channels());
    let solved = delays(&g, SampleRate::SR_48K);
    assert_eq!(
        solved.departure(B).align_to(solved.arrival(C)),
        Samples::new(64),
        "and the pass gives the dry source the same gap"
    );
    // Over the topology alone the event path is invisible.
    assert!(latency_plan(&t, SampleRate::SR_48K).is_empty());
}

/// N3: a shape whose latency or tail disagrees with its spec is refused, as
/// a width mismatch is — one of them is stale, and compiling either would
/// make `latency::plan` over the value disagree with the plan.
#[test]
fn a_shape_disagreeing_on_latency_or_tail_is_refused() {
    let mut t = Topology::default();
    t.nodes
        .insert(A, spec("gain", 1, 1).latency(Samples::new(10)));
    let valid = GraphSpec::new(t).validate().unwrap();
    let late: Shapes = [(
        A,
        Shape::through_unassigned(ChannelLayout::MONO).latency(Latency::new(Samples::new(12))),
    )]
    .into();
    assert!(matches!(
        compile(&valid, &late, &common::prepare(PREP), None),
        Err(CompileError::LatencyMismatch { node: A, .. })
    ));
    let tailed: Shapes = [(
        A,
        Shape::through_unassigned(ChannelLayout::MONO)
            .latency(Latency::new(Samples::new(10)))
            .tail(Tail::Unbounded),
    )]
    .into();
    assert!(matches!(
        compile(&valid, &tailed, &common::prepare(PREP), None),
        Err(CompileError::TailMismatch { node: A, .. })
    ));
}

// ---------------------------------------------------------------------------
// Event resolution.
// ---------------------------------------------------------------------------

/// An automation source `A` feeding sink `B`'s event input, with `B`
/// declaring `sink` resolution; the edge marked `required` when given.
fn automation_into(
    sink: tutti_graph::Resolution,
    required: Option<tutti_graph::Resolution>,
) -> Result<(), CompileError> {
    let mut g = two_node_spec();
    let (lane, target) = (EventOutputPort::new(A, 0), EventInputPort::new(B, 0));
    g.connect_events(target, EventEdge::Direct(lane));
    if let Some(r) = required {
        g.require_resolution(target, lane, r);
    }
    let mono = || Shape::through_unassigned(ChannelLayout::MONO);
    let shapes: Shapes = [
        (A, mono().events(0, 1)),
        (B, mono().events(1, 0).event_resolution(sink)),
    ]
    .into();
    let valid = g.validate().expect("valid");
    compile(&valid, &shapes, &common::prepare(PREP), None).map(|_| ())
}

/// The rule: a marked edge is refused, by name, when its sink declares a
/// coarser resolution; an unmarked edge never is; a fine enough sink always
/// compiles.
#[test]
fn a_sample_accurate_edge_into_a_block_node_is_refused() {
    use tutti_graph::Resolution::{Block, Frames, Sample};
    assert_eq!(
        automation_into(Block, Some(Sample)),
        Err(CompileError::ResolutionTooCoarse {
            at: EventInputPort::new(B, 0),
            from: EventOutputPort::new(A, 0),
            required: Sample,
            sink: Block,
        })
    );
    assert_eq!(automation_into(Block, None), Ok(()), "unmarked: allowed");
    assert_eq!(automation_into(Sample, Some(Sample)), Ok(()));
    assert_eq!(automation_into(Frames(8), Some(Frames(16))), Ok(()));
    assert!(automation_into(Frames(8), Some(Sample)).is_err());
    assert!(automation_into(Frames(8), Some(Frames(4))).is_err());
    assert_eq!(automation_into(Block, Some(Block)), Ok(()));
}

/// A mark with no edge under it is a stale mark, and invalid: it would
/// otherwise pass unchecked forever.
#[test]
fn a_resolution_mark_without_its_edge_is_invalid() {
    let mut g = two_node_spec();
    let (from, at) = (EventOutputPort::new(A, 0), EventInputPort::new(B, 0));
    g.require_resolution(at, from, tutti_graph::Resolution::Sample);
    assert_eq!(
        g.validate().err(),
        Some(vec![GraphInvalid::RequirementWithoutEdge { at, from }])
    );
}

/// The output list is as wide as the declared main output, and a spec whose
/// list is not is invalid whichever of the two moved.
#[test]
fn an_output_list_as_wide_as_the_declared_main_output_is_valid_and_another_is_not() {
    let mut g = two_node_spec();
    assert_eq!(
        g.topology.outputs.len(),
        usize::from(g.main_output.width().count())
    );
    assert!(g.validate().is_ok());

    g.topology.outputs.push(Vec::new());
    assert_eq!(
        g.validate().err(),
        Some(vec![GraphInvalid::OutputWidth {
            outputs: g.topology.outputs.len(),
            declared: g.topology.outputs.len() - 1,
        }])
    );

    g.topology.outputs.truncate(1);
    g.main_output = tutti_buffers::ChannelSet::default_for(ChannelLayout::from_count(3));
    assert_eq!(
        g.validate().err(),
        Some(vec![GraphInvalid::OutputWidth {
            outputs: 1,
            declared: 3,
        }])
    );
}

/// A node promises sample accuracy unless it declares otherwise.
#[test]
fn nodes_default_to_sample_resolution() {
    use tutti_graph::Resolution;
    let s = Shape::through_unassigned(ChannelLayout::MONO);
    assert_eq!(s.event_resolution, Resolution::Sample);
}

/// Removing a node removes the resolution marks on its edges with it, so a
/// removal never leaves a stale mark that fails the next commit.
#[test]
fn removing_a_node_removes_its_resolution_marks() {
    let (mut ed, _exec) = tutti_graph::Editor::new(common::prepare(PREP));
    ed.insert(
        A,
        "emit",
        Unforkable(common::TestNode::new(Kind::Emitter {
            period: 4,
            phase: 0,
        })),
    );
    ed.insert(
        B,
        "consume",
        Unforkable(common::TestNode::new(Kind::Consumer { inputs: 1 })),
    );
    let (from, at) = (EventOutputPort::new(A, 0), EventInputPort::new(B, 0));
    ed.connect_events(at, EventEdge::Direct(from)).unwrap();
    ed.require_resolution(at, from, tutti_graph::Resolution::Sample)
        .unwrap();
    ed.commit().expect("the consumer honours samples");
    ed.remove(A);
    ed.commit().expect("commits");
    assert!(ed.spec().required_resolution.is_empty());
}

/// Disconnecting a marked edge drops its mark with it, so the disconnect
/// cannot wedge every later commit with `RequirementWithoutEdge`; a mark can
/// also be dropped alone.
#[test]
fn disconnecting_an_edge_drops_its_resolution_mark() {
    let (mut ed, _exec) = tutti_graph::Editor::new(common::prepare(PREP));
    ed.insert(
        A,
        "emit",
        Unforkable(common::TestNode::new(Kind::Emitter {
            period: 4,
            phase: 0,
        })),
    );
    ed.insert(
        B,
        "consume",
        Unforkable(common::TestNode::new(Kind::Consumer { inputs: 1 })),
    );
    let (from, at) = (EventOutputPort::new(A, 0), EventInputPort::new(B, 0));
    ed.connect_events(at, EventEdge::Direct(from)).unwrap();
    ed.require_resolution(at, from, tutti_graph::Resolution::Sample)
        .unwrap();
    ed.commit().expect("commits");
    assert!(ed.disconnect_events(at, tutti_graph::EventEdge::Direct(from)));
    assert!(
        !ed.disconnect_events(at, tutti_graph::EventEdge::Direct(from)),
        "already gone"
    );
    ed.commit().expect("commits");
    assert!(ed.spec().required_resolution.is_empty());

    // `clear_resolution`: the edge stays, the requirement goes.
    let mut g = two_node_spec();
    g.connect_events(at, EventEdge::Direct(from));
    g.require_resolution(at, from, tutti_graph::Resolution::Sample);
    g.clear_resolution(at, from);
    let mono = || Shape::through_unassigned(ChannelLayout::MONO);
    let shapes: Shapes = [
        (A, mono().events(0, 1)),
        (
            B,
            mono()
                .events(1, 0)
                .event_resolution(tutti_graph::Resolution::Block),
        ),
    ]
    .into();
    let valid = g.validate().expect("valid");
    assert!(compile(&valid, &shapes, &common::prepare(PREP), None).is_ok());
}
