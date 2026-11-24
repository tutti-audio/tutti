//! A node reads which of its audio ports are wired from the compiled plan:
//! an input with a source, an output with an edge, a tap or a meter
//! consuming it. The mask follows connects, disconnects and taps at the
//! commit that applies them, is the same on the parallel executor, and the
//! commit reports exactly the nodes whose answer moved.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_graph::testing as common;

use common::{Kind, TestNode, WORKERS, prepare};
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::{
    Bus, BusOutputPort, BusRole, Buses, Cx, Edge, EdgeFade, EdgeId, Editor, Executor, FeedbackFrom,
    InputPort, Io, Node, NodeBus, NodeKey, OutputPort, PortSource, Prepare, Rewired, Shape, Status,
    TapAlign, TapId, TapOptions, Unforkable, Wired, WiredMask,
};
use tutti_units::{Samples, Tail};

const SRC: NodeKey = NodeKey(1);
const PROBE: NodeKey = NodeKey(2);
const BYSTANDER: NodeKey = NodeKey(3);

/// The masks a [`Probe`] read in its last block.
#[derive(Default)]
struct Seen {
    main_in: AtomicU64,
    main_out: AtomicU64,
    aux_in: AtomicU64,
    aux_out: AtomicU64,
}

/// What a step of the scenario read: main inputs, main outputs, aux input,
/// aux output.
type Read = (u64, u64, u64, u64);

impl Seen {
    fn read(&self) -> Read {
        (
            self.main_in.load(Ordering::Relaxed),
            self.main_out.load(Ordering::Relaxed),
            self.aux_in.load(Ordering::Relaxed),
            self.aux_out.load(Ordering::Relaxed),
        )
    }
}

/// A stereo main bus and a mono aux bus on each side; records its wired
/// masks every block.
struct Probe {
    seen: Arc<Seen>,
}

fn stereo() -> ChannelTopology {
    ChannelTopology::unassigned(ChannelLayout::STEREO).expect("stereo")
}

fn mono() -> ChannelTopology {
    ChannelTopology::unassigned(ChannelLayout::MONO).expect("mono")
}

fn side(role: BusRole) -> Buses {
    Buses::NONE
        .with(Bus::main(stereo()))
        .and_then(|b| b.with(Bus::new(mono(), role)))
        .expect("two buses")
}

fn probe_shape() -> Shape {
    Shape::buses(side(BusRole::Sidechain), side(BusRole::Aux)).tail(Tail::Unbounded)
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        probe_shape()
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let shape = probe_shape();
        let (aux_in, aux_out) = (
            shape.input_bus(1).expect("aux in"),
            shape.output_bus(1).expect("aux out"),
        );
        self.seen
            .main_in
            .store(io.wired_inputs().0, Ordering::Relaxed);
        self.seen
            .main_out
            .store(io.wired_outputs().0, Ordering::Relaxed);
        self.seen
            .aux_in
            .store(io.wired_bus_in(aux_in).0, Ordering::Relaxed);
        self.seen
            .aux_out
            .store(io.wired_bus_out(aux_out).0, Ordering::Relaxed);
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn out(node: NodeKey, port: u16) -> OutputPort {
    OutputPort::new(node, port)
}

fn wired(inputs: u64, outputs: u64) -> Wired {
    Wired {
        inputs: WiredMask(inputs),
        outputs: WiredMask(outputs),
    }
}

fn render(exec: &mut Executor, outs: usize) {
    common::block(exec, 16, outs);
}

/// Commits, applies and renders one block, returning what the commit
/// reported and what the probe read.
fn step(ed: &mut Editor, exec: &mut Executor, seen: &Seen, outs: usize) -> (Vec<Rewired>, Read) {
    let report = ed.commit().expect("commits");
    render(exec, outs);
    ed.collect();
    (report, seen.read())
}

/// The scenario: a two-channel source, a probe and a bystander nothing
/// touches, the probe's wiring changed one commit at a time. Returns the
/// steps' reports and reads.
fn scenario(workers: usize) -> Vec<(Vec<Rewired>, Read)> {
    let (mut ed, mut exec) = Editor::new(prepare(16));
    common::pooled(&mut exec, workers);
    let seen = Arc::new(Seen::default());
    ed.insert(
        SRC,
        "src",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 2,
        })),
    );
    ed.insert(
        PROBE,
        "probe",
        Unforkable(Probe {
            seen: Arc::clone(&seen),
        }),
    );
    ed.insert(
        BYSTANDER,
        "bystander",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
    );
    let at = |port| InputPort::new(PROBE, port);
    let from = |port| Edge::Direct(PortSource::Node(out(SRC, port)));
    let mut steps = Vec::new();

    // Nothing wired.
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    // Nothing changed: the commit sends nothing and reports nothing.
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    // Main input 0, then the aux input.
    ed.connect_edge(EdgeId::into_port(at(0), from(0))).unwrap();
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    ed.connect_edge(EdgeId::into_port(at(2), from(1))).unwrap();
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    // A tap alone consumes main output 1; a main output takes aux out.
    let tap = ed
        .open_tap(
            TapId::of(1),
            vec![PortSource::Node(out(PROBE, 1))],
            TapOptions::new(Samples::new(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    ed.wire_outputs(vec![PortSource::Node(out(PROBE, 2))])
        .unwrap();
    steps.push(step(&mut ed, &mut exec, &seen, 1));
    // Back to nothing: the last source of each input goes, the output
    // leaves, the tap closes.
    ed.disconnect_edge(EdgeId::into_port(at(0), from(0)));
    ed.disconnect_edge(EdgeId::into_port(at(2), from(1)));
    ed.wire_outputs(vec![]).unwrap();
    ed.close_tap(TapId::of(1)).expect("open");
    drop(tap);
    steps.push(step(&mut ed, &mut exec, &seen, 0));
    steps
}

fn rewired(node: NodeKey, was: Wired, now: Wired) -> Rewired {
    Rewired { node, was, now }
}

#[test]
fn a_node_reads_its_ports_unwired_then_wired_then_unwired() {
    let steps = scenario(1);
    let reads: Vec<Read> = steps.iter().map(|s| s.1).collect();
    assert_eq!(
        reads,
        [
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            // `SRC.0 -> PROBE.in0`: main input 0.
            (0b01, 0, 0, 0),
            // `SRC.1 -> PROBE.in2`: the aux input, bit 0 of its bus, and
            // not a main bit.
            (0b01, 0, 0b1, 0),
            // A tap on `PROBE.out1` alone: main output 1.
            (0b01, 0b10, 0b1, 0),
            // A main output on `PROBE.out2`: the aux output's bit 0.
            (0b01, 0b10, 0b1, 0b1),
            // Every edge, the output and the tap gone.
            (0, 0, 0, 0),
        ]
    );
}

#[test]
fn a_commit_reports_the_nodes_whose_wiring_moved_and_no_others() {
    let steps = scenario(1);
    let reports: Vec<&[Rewired]> = steps.iter().map(|s| s.0.as_slice()).collect();
    assert_eq!(reports[0], [], "new nodes with no wiring are not rewired");
    assert_eq!(
        reports[1],
        [],
        "a commit that sends nothing reports nothing"
    );
    // The source gains a consumer and the probe a source; the bystander,
    // which the commit does not touch, is absent.
    assert_eq!(
        reports[2],
        [
            rewired(SRC, wired(0, 0), wired(0, 0b01)),
            rewired(PROBE, wired(0, 0), wired(0b001, 0)),
        ]
    );
    assert_eq!(
        reports[3],
        [
            rewired(SRC, wired(0, 0b01), wired(0, 0b11)),
            rewired(PROBE, wired(0b001, 0), wired(0b101, 0)),
        ]
    );
    assert_eq!(
        reports[4],
        [rewired(PROBE, wired(0b101, 0), wired(0b101, 0b010))],
        "a tap is a consumer"
    );
    assert_eq!(
        reports[5],
        [rewired(PROBE, wired(0b101, 0b010), wired(0b101, 0b110))]
    );
    assert_eq!(
        reports[6],
        [
            rewired(SRC, wired(0, 0b11), wired(0, 0)),
            rewired(PROBE, wired(0b101, 0b110), wired(0, 0)),
        ],
        "the last source and the last consumer going is a change"
    );
}

#[test]
fn the_parallel_executor_reads_the_one_participant_masks() {
    common::agree_across_workers(WORKERS, scenario, Clone::clone);
}

/// A hand-built `Io` has no plan: every port reads wired, so a node that
/// gates on the mask renders as if connected.
#[test]
fn a_hand_built_io_reads_every_port_wired() {
    let seen = Arc::new(Seen::default());
    let node = Probe {
        seen: Arc::clone(&seen),
    };
    let rate = tutti_units::SampleRate::SR_48K;
    let mut node = tutti_graph::contract::prepared(node, rate, 16);
    let block: &[f32] = &[0.0; 16];
    let planes = [block; 3];
    let inputs = tutti_buffers::Planes::new(&planes).unwrap();
    tutti_graph::contract::drive(&mut node, rate, inputs, &[]);
    assert_eq!(seen.read(), (0b11, 0b11, 0b1, 0b1));
}

const OTHER: NodeKey = NodeKey(4);

/// A probe, a second probe, and an executor: the probe pair's two `Seen`s.
fn pair() -> (Editor, Executor, Arc<Seen>, Arc<Seen>) {
    let (mut ed, exec) = Editor::new(prepare(16));
    let (a, b) = (Arc::new(Seen::default()), Arc::new(Seen::default()));
    ed.insert(
        PROBE,
        "a",
        Unforkable(Probe {
            seen: Arc::clone(&a),
        }),
    );
    ed.insert(
        OTHER,
        "b",
        Unforkable(Probe {
            seen: Arc::clone(&b),
        }),
    );
    (ed, exec, a, b)
}

/// A bus edge lowers to one edge per channel: every port of both buses
/// reads wired, the aux bus's by its own handle.
#[test]
fn a_bus_edge_marks_every_channel_port_of_its_bus() {
    let (mut ed, mut exec, a, b) = pair();
    ed.connect_bus(
        BusOutputPort::Node(NodeBus::new(PROBE, 0)),
        NodeBus::new(OTHER, 0),
    )
    .expect("stereo to stereo");
    ed.connect_bus(
        BusOutputPort::Node(NodeBus::new(PROBE, 1)),
        NodeBus::new(OTHER, 1),
    )
    .expect("mono to mono");
    let report = ed.commit().expect("commits");
    render(&mut exec, 0);
    assert_eq!(b.read(), (0b11, 0, 0b1, 0), "OTHER's inputs, both buses");
    assert_eq!(a.read(), (0, 0b11, 0, 0b1), "PROBE's outputs, both buses");
    assert_eq!(
        report,
        [
            rewired(PROBE, wired(0, 0), wired(0, 0b111)),
            rewired(OTHER, wired(0, 0), wired(0b111, 0)),
        ]
    );
}

/// A feedback edge is a source of its input and a consumer of its output.
#[test]
fn a_feedback_edge_marks_the_input_and_the_source_output() {
    let (mut ed, mut exec, a, _b) = pair();
    ed.connect_edge(EdgeId::into_port(
        InputPort::new(PROBE, 1),
        Edge::Feedback(FeedbackFrom::new(out(PROBE, 0), Samples::new(16))),
    ))
    .unwrap();
    let report = ed.commit().expect("commits");
    render(&mut exec, 0);
    assert_eq!(a.read(), (0b10, 0b01, 0, 0));
    assert_eq!(report, [rewired(PROBE, wired(0, 0), wired(0b10, 0b01))]);
}

/// Ports past 31 and up to 63: a 64-port node reads bit 63 on both sides.
#[test]
fn a_shape_with_64_ports_reads_bit_63() {
    struct Wide(Arc<Seen>);
    impl Node for Wide {
        fn shape(&self) -> Shape {
            let all = ChannelLayout::from_count(64);
            Shape::unassigned(all, all).tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
            self.0.main_in.store(io.wired_inputs().0, Ordering::Relaxed);
            self.0
                .main_out
                .store(io.wired_outputs().0, Ordering::Relaxed);
            Status::Silent
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(16));
    let seen = Arc::new(Seen::default());
    ed.insert(
        SRC,
        "src",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 64,
        })),
    );
    ed.insert(PROBE, "wide", Unforkable(Wide(Arc::clone(&seen))));
    ed.connect_edge(EdgeId::into_port(
        InputPort::new(PROBE, 63),
        Edge::Direct(PortSource::Node(out(SRC, 63))),
    ))
    .unwrap();
    let _tap = ed
        .open_tap(
            TapId::of(1),
            vec![PortSource::Node(out(PROBE, 63))],
            TapOptions::new(Samples::new(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    let _ = ed.commit().expect("commits");
    render(&mut exec, 0);
    assert_eq!(seen.main_in.load(Ordering::Relaxed), 1 << 63);
    assert_eq!(seen.main_out.load(Ordering::Relaxed), 1 << 63);
}

fn heard_edge() -> (Editor, Executor, Arc<Seen>, EdgeId) {
    let (mut ed, mut exec) = Editor::new(prepare(16));
    let seen = Arc::new(Seen::default());
    ed.insert(
        SRC,
        "src",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 2,
        })),
    );
    ed.insert(
        PROBE,
        "probe",
        Unforkable(Probe {
            seen: Arc::clone(&seen),
        }),
    );
    let from = Edge::Direct(PortSource::Node(out(SRC, 0)));
    let sink = InputPort::new(PROBE, 0);
    ed.connect_edge(EdgeId::into_port(sink, from)).unwrap();
    let _ = ed.commit().expect("commits");
    render(&mut exec, 0);
    ed.collect();
    let id = EdgeId::into_port(sink, from);
    (ed, exec, seen, id)
}

/// A fade-out keeps its edge in the spec until the fade ends, so the input
/// reads wired through it; the commit that removes the edge reports the loss.
#[test]
fn a_fade_out_keeps_the_input_wired_until_the_commit_that_removes_the_edge() {
    let (mut ed, mut exec, seen, id) = heard_edge();
    assert_eq!(seen.read().0, 0b1);
    ed.set_edge_fade(id, EdgeFade::Out).expect("an edge");
    assert!(
        ed.commit().expect("commits").is_empty(),
        "the fade's lane moves no port's wiring"
    );
    for _ in 0..(common::declick() / 16 + 2) {
        render(&mut exec, 0);
    }
    ed.collect();
    assert_eq!(seen.read().0, 0b1, "fading, still wired");
    assert!(ed.spec().has_edge(id), "held until the fade is reported");
    assert!(
        ed.finish_fades(),
        "the fade ended and the edge left the spec"
    );
    let report = ed.commit().expect("commits");
    render(&mut exec, 0);
    assert_eq!(seen.read().0, 0, "wired no more");
    assert_eq!(
        report,
        [
            rewired(SRC, wired(0, 0b1), wired(0, 0)),
            rewired(PROBE, wired(0b1, 0), wired(0, 0)),
        ]
    );
}

/// A fade-in edge is in the spec from its commit: wired at once.
#[test]
fn a_fade_in_counts_as_wired_at_once() {
    let (mut ed, mut exec, seen, _) = heard_edge();
    let sink = InputPort::new(PROBE, 1);
    let from = Edge::Direct(PortSource::Node(out(SRC, 1)));
    ed.connect_edge(EdgeId::into_port(sink, from)).unwrap();
    let id = EdgeId::into_port(sink, from);
    ed.set_edge_fade(id, EdgeFade::In).expect("an edge");
    let report = ed.commit().expect("commits");
    render(&mut exec, 0);
    assert_eq!(
        seen.read().0,
        0b11,
        "the faded-in port is wired in its first block"
    );
    assert_eq!(report.len(), 2);
}
