//! Audio fan-in, the views:
//! the latency and tail views read a **set** of sources per input port and per
//! output channel. Every topology here is built by hand and driven through the
//! pure passes, never the executor (`tests/mixing/fan_in.rs` drives the
//! compiled op and holds the sum's order).
//!
//! Figures are analytic: an arrival is the latest departure over every source
//! of every port, and a source's delay is that arrival minus its own
//! departure.

use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Edge, FeedbackFrom, InputPort, NodeKey, NodeSpec, OutputPort, PortSource, Topology, delays,
    graph_tail, latency_plan,
};
use tutti_units::{SampleRate, Samples, Tail};

const BLOCK: usize = 64;

fn key(n: u32) -> NodeKey {
    NodeKey(n.into())
}

fn at(node: NodeKey, port: u16) -> InputPort {
    InputPort::new(node, port)
}

fn out(node: NodeKey, port: u16) -> OutputPort {
    OutputPort::new(node, port)
}

fn direct(node: NodeKey) -> Edge {
    Edge::Direct(PortSource::Node(out(node, 0)))
}

fn spec(ins: u16, outs: u16) -> NodeSpec {
    NodeSpec::new(
        "n",
        ChannelLayout::from_count(ins),
        ChannelLayout::from_count(outs),
    )
}

fn node(t: &mut Topology, k: NodeKey, ins: u16, lat: usize, tail: usize) {
    t.nodes.insert(
        k,
        spec(ins, 1)
            .latency(Samples::new(lat))
            .tail(Tail::Finite(Samples::new(tail))),
    );
}

/// Sources of latency 300, 100 and 0, and a zero-latency sink `S` with two
/// audio ports.
const SLOW: NodeKey = NodeKey(1);
const MID: NodeKey = NodeKey(2);
const DRY: NodeKey = NodeKey(3);
const SINK: NodeKey = NodeKey(9);

fn sources() -> Topology {
    let mut t = Topology::default();
    node(&mut t, SLOW, 0, 300, 0);
    node(&mut t, MID, 0, 100, 0);
    node(&mut t, DRY, 0, 0, 0);
    node(&mut t, SINK, 2, 0, 0);
    t
}

/// How long `from`'s output waits to meet `to`'s arrival.
fn waits(d: &tutti_graph::Delays<NodeKey>, from: NodeKey, to: NodeKey) -> Samples {
    d.departure(from).align_to(d.arrival(to))
}

/// How long `from`'s output waits to meet the output time.
fn waits_to_output(d: &tutti_graph::Delays<NodeKey>, from: NodeKey) -> Samples {
    d.departure(from).align_to(d.compensation().total())
}

/// **A fan-in port aligns each source on its own.** Port 0 sums `SLOW`
/// (300), `MID` (100) and `DRY` (0), port 1 reads `MID`: the sink arrives at
/// the latest departure over all its sources at all its ports (300), and
/// every source but the slowest is delayed by its own gap to it, so each
/// reaches the sum at the same time. The sum adds no latency of its own.
#[test]
fn each_source_of_a_fan_in_port_is_delayed_to_the_arrival() {
    let mut t = sources();
    for from in [SLOW, MID, DRY] {
        t.connect(at(SINK, 0), direct(from));
    }
    t.connect(at(SINK, 1), direct(MID));
    t.set_outputs([PortSource::Node(out(SINK, 0))]);

    let d = delays(&t, SampleRate::SR_48K);
    assert_eq!(
        d.arrival(SINK),
        Samples::new(300),
        "the latest of all sources"
    );
    assert_eq!(d.departure(SINK), Samples::new(300), "a sum has no latency");
    assert_eq!(
        [SLOW, MID, DRY].map(|from| waits(&d, from, SINK)),
        [0, 200, 300].map(Samples::new)
    );
    assert_eq!(
        latency_plan(&t, SampleRate::SR_48K).total(),
        Samples::new(300)
    );
}

/// A main input in a fan-in is aligned like any other source: it arrives at
/// zero, so it waits for the slowest.
#[test]
fn a_main_input_source_of_a_fan_in_waits_for_the_slowest() {
    let mut t = sources();
    t.inputs = ChannelLayout::MONO;
    t.connect(at(SINK, 0), direct(MID));
    t.connect(at(SINK, 0), Edge::Direct(PortSource::MainInput(0)));
    t.set_outputs([PortSource::Node(out(SINK, 0))]);

    let d = delays(&t, SampleRate::SR_48K);
    // The main input departs at zero.
    assert_eq!(d.arrival(SINK), Samples::new(100));
}

/// A feedback addend carries last block's value: it is `Feed::None`, adds
/// nothing to the arrival and is never aligned.
#[test]
fn a_feedback_addend_is_not_aligned_and_does_not_delay() {
    let mut t = sources();
    t.connect(at(SINK, 0), direct(MID));
    t.connect(
        at(SINK, 0),
        Edge::Feedback(FeedbackFrom::new(out(SLOW, 0), Samples::new(BLOCK))),
    );
    let d = delays(&t, SampleRate::SR_48K);
    assert_eq!(d.arrival(SINK), Samples::new(100));
}

/// **A fan-in output channel aligns each source to the output time**, and the
/// channel's arrival is its latest source's departure. Channel 0 sums `SLOW`
/// and `DRY`, channel 1 is `MID`: the output time is 300, `DRY` and `MID` are
/// delayed by their own gaps, and channel 1's pre-roll is the gap of its
/// latest source.
#[test]
fn each_source_of_a_fan_in_output_is_delayed_to_the_output_time() {
    let mut t = sources();
    t.set_outputs([
        PortSource::Node(out(SLOW, 0)),
        PortSource::Node(out(MID, 0)),
    ]);
    t.outputs[0].push(PortSource::Node(out(DRY, 0)));

    let d = delays(&t, SampleRate::SR_48K);
    assert_eq!(waits_to_output(&d, DRY), Samples::new(300));
    assert_eq!(waits_to_output(&d, MID), Samples::new(200));
    let c = d.compensation();
    assert_eq!(c.total(), Samples::new(300));
    assert_eq!(c.channels(), &[Samples::new(0), Samples::new(200)]);
}

/// **A sum passes the longest tail among its sources**, not their sum, at a
/// port and at an output channel, and the sink's own tail follows. Tails
/// exclude latency, so the sources' latencies change nothing.
#[test]
fn the_tail_of_a_sum_is_the_longest_of_its_sources() {
    let mut t = Topology::default();
    node(&mut t, key(1), 0, 0, 2000);
    node(&mut t, key(2), 0, 700, 3000);
    node(&mut t, SINK, 1, 0, 100);
    t.connect(at(SINK, 0), direct(key(1)));
    t.connect(at(SINK, 0), direct(key(2)));
    t.set_outputs([PortSource::Node(out(SINK, 0))]);
    assert_eq!(
        graph_tail(&t, SampleRate::SR_48K).samples(),
        Some(Samples::new(3100))
    );

    // The same two into one output channel.
    t.set_outputs([PortSource::Node(out(key(1), 0))]);
    t.outputs[0].push(PortSource::Node(out(key(2), 0)));
    assert_eq!(
        graph_tail(&t, SampleRate::SR_48K).samples(),
        Some(Samples::new(3000))
    );
}
