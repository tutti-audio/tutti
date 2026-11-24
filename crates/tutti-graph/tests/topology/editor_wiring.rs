//! The editor's typed wiring calls: each does its write (read back through the
//! editor's public reads, or heard in a render with an analytic figure), and a
//! node, port or channel that does not exist fails at the call, the graph
//! as it was.

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{
    Cx, Edge, EdgeGainError, EdgeId, Editor, EventEdge, EventInputPort, EventOutputPort, Executor,
    FeedbackFrom, InputPort, Io, MissingSource, Node, NodeKey, OutputPort, ParamIn, Prepare,
    Resolution, Shape, Status, TransportPosition, Unforkable, WireError,
};
use tutti_graph::{PortSink, PortSource};
use tutti_units::{Beat, Bpm, SampleRate, Samples, UnitParam};

/// A mono source holding one level.
struct Level(f32);

impl Node for Level {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A mono node times a factor.
struct Times(f32);

impl Node for Times {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = i * self.0;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node with one event input and one event output, and no audio.
struct Events;

impl Node for Events {
    fn shape(&self) -> Shape {
        Shape::EMPTY.events(1, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

const BLOCK: usize = 64;

fn editor() -> (Editor, Executor) {
    Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(BLOCK)),
    ))
}

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn out(node: u64) -> PortSource {
    PortSource::Node(OutputPort::new(key(node), 0))
}

fn into_output(channel: u16, from: PortSource) -> EdgeId {
    EdgeId {
        sink: PortSink::Output(channel),
        from: Edge::Direct(from),
    }
}

fn into_port(node: u64, from: PortSource) -> EdgeId {
    EdgeId {
        sink: PortSink::Port(InputPort::new(key(node), 0)),
        from: Edge::Direct(from),
    }
}

/// Commits, installs and renders one block of the two main output channels.
fn render(ed: &mut Editor, exec: &mut Executor) -> [Vec<f32>; 2] {
    ed.commit().expect("commits");
    exec.apply_pending();
    let (mut left, mut right) = (vec![0.0_f32; BLOCK], vec![0.0_f32; BLOCK]);
    let transport = TransportPosition::new(false, Bpm::of(120.0), Beat::new(0.0), None);
    exec.process(BLOCK, &transport, &[], &mut [&mut left, &mut right]);
    [left, right]
}

/// `connect_edge` and `disconnect_edge` are heard: a source into a channel is
/// its level, a source through a node into the other is the level times the
/// node's factor, and a wire taken out is silence. A wire already there is
/// not new, and one that is not there is not removed.
#[test]
fn connect_and_disconnect_edge_are_heard_at_the_outputs() {
    let (mut ed, mut exec) = editor();
    ed.insert(key(1), "level", Unforkable(Level(0.25)));
    ed.insert(key(2), "times", Unforkable(Times(4.0)));
    assert!(ed.resize_outputs(2));
    assert_eq!(ed.connect_edge(into_output(0, out(1))), Ok(true));
    assert_eq!(ed.connect_edge(into_output(0, out(1))), Ok(false));
    assert_eq!(ed.connect_edge(into_port(2, out(1))), Ok(true));
    assert_eq!(ed.connect_edge(into_output(1, out(2))), Ok(true));
    let [left, right] = render(&mut ed, &mut exec);
    assert!(left.iter().all(|&s| s == 0.25), "left is the level");
    assert!(
        right.iter().all(|&s| s == 1.0),
        "right is the level times 4"
    );

    assert!(ed.disconnect_edge(into_output(0, out(1))));
    assert!(!ed.disconnect_edge(into_output(0, out(1))));
    assert!(
        !ed.disconnect_edge(into_output(9, out(1))),
        "no such channel"
    );
    let [left, _] = render(&mut ed, &mut exec);
    assert!(left.iter().all(|&s| s == 0.0), "left is silent");
}

/// A wire that names a node, port or channel that does not exist fails at
/// the call with the error that names it, and the spec is as it was.
#[test]
fn connect_edge_refuses_what_does_not_exist() {
    let (mut ed, _exec) = editor();
    ed.insert(key(1), "level", Unforkable(Level(0.25)));
    ed.insert(key(2), "times", Unforkable(Times(4.0)));
    ed.resize_outputs(1);
    let before = ed.spec().clone();

    assert_eq!(
        ed.connect_edge(into_output(0, out(7))),
        Err(WireError::Missing(MissingSource::Node { node: key(7) })),
    );
    assert_eq!(
        ed.connect_edge(into_output(0, PortSource::Node(OutputPort::new(key(1), 3)))),
        Err(WireError::Missing(MissingSource::Port {
            node: key(1),
            port: 3,
            width: 1,
        })),
    );
    assert_eq!(
        ed.connect_edge(into_output(0, PortSource::MainInput(0))),
        Err(WireError::Missing(MissingSource::MainInput {
            channel: 0,
            width: 0,
        })),
    );
    assert_eq!(
        ed.connect_edge(into_output(1, out(1))),
        Err(WireError::NoOutputChannel {
            channel: 1,
            width: 1,
        }),
    );
    assert_eq!(
        ed.connect_edge(into_port(9, out(1))),
        Err(WireError::Missing(MissingSource::Node { node: key(9) })),
    );
    let past = EdgeId {
        sink: PortSink::Port(InputPort::new(key(2), 1)),
        from: Edge::Direct(out(1)),
    };
    assert_eq!(
        ed.connect_edge(past),
        Err(WireError::NoInputPort {
            node: key(2),
            port: 1,
            width: 1,
        }),
    );
    let feedback = EdgeId {
        sink: PortSink::Output(0),
        from: Edge::Feedback(FeedbackFrom::new(
            OutputPort::new(key(1), 0),
            Samples::new(128),
        )),
    };
    assert_eq!(
        ed.connect_edge(feedback),
        Err(WireError::FeedbackIntoOutput { channel: 0 }),
    );
    assert_eq!(*ed.spec(), before, "a refused call writes nothing");
}

/// `resize_outputs` makes channels silent and cuts them with their sources and
/// gain entries; it says whether the width moved.
#[test]
fn resize_outputs_cuts_a_channel_with_its_sources_and_gain() {
    let (mut ed, _exec) = editor();
    ed.insert(key(1), "level", Unforkable(Level(0.25)));
    assert!(ed.resize_outputs(2));
    assert!(!ed.resize_outputs(2));
    let second = into_output(1, out(1));
    ed.connect_edge(second).expect("wires");
    ed.set_edge_gain(second, tutti_units::Gain::new(0.5))
        .expect("an edge of the spec");
    assert!(ed.spec().gains.contains_key(&second));

    assert!(ed.resize_outputs(1));
    assert_eq!(ed.spec().topology.outputs.len(), 1);
    assert!(
        ed.spec().gains.is_empty(),
        "the cut channel's gain went with it"
    );
}

/// `connect_events` and `disconnect_events` write and take out one edge:
/// removing the direct edge leaves a feedback edge from the same source (and
/// its resolution mark), and the last edge of a source takes its mark.
#[test]
fn event_edges_are_exact_and_the_last_takes_its_mark() {
    let (mut ed, _exec) = editor();
    ed.insert(key(1), "events", Unforkable(Events));
    ed.insert(key(2), "events", Unforkable(Events));
    let at = EventInputPort::new(key(2), 0);
    let from = EventOutputPort::new(key(1), 0);
    let direct = EventEdge::Direct(from);
    let feedback = EventEdge::feedback(from, Samples::new(128));
    ed.connect_events(at, direct).expect("wires");
    ed.connect_events(at, feedback).expect("wires");
    assert_eq!(ed.spec().events[&at].len(), 2);
    ed.spec_mut()
        .require_resolution(at, from, Resolution::Sample);

    assert!(ed.disconnect_events(at, direct));
    assert_eq!(ed.spec().events[&at], vec![feedback]);
    assert!(
        ed.spec().required_resolution.contains_key(&(at, from)),
        "the source still feeds the port"
    );
    assert!(!ed.disconnect_events(at, direct));
    assert!(ed.disconnect_events(at, feedback));
    assert!(ed.spec().events.is_empty());
    assert!(ed.spec().required_resolution.is_empty());
}

/// An event edge from or into a node or port that does not exist fails at
/// the call, naming it.
#[test]
fn connect_events_refuses_what_does_not_exist() {
    let (mut ed, _exec) = editor();
    ed.insert(key(1), "events", Unforkable(Events));
    ed.insert(key(2), "level", Unforkable(Level(0.0)));
    let before = ed.spec().clone();
    let from = EventOutputPort::new(key(1), 0);
    let direct = EventEdge::Direct;

    assert_eq!(
        ed.connect_events(EventInputPort::new(key(8), 0), direct(from)),
        Err(WireError::Missing(MissingSource::Node { node: key(8) })),
    );
    assert_eq!(
        ed.connect_events(EventInputPort::new(key(1), 1), direct(from)),
        Err(WireError::Missing(MissingSource::EventInputPort {
            node: key(1),
            port: 1,
            width: 1,
        })),
    );
    assert_eq!(
        ed.connect_events(
            EventInputPort::new(key(1), 0),
            direct(EventOutputPort::new(key(2), 0)),
        ),
        Err(WireError::Missing(MissingSource::EventPort {
            node: key(2),
            port: 0,
            width: 0,
        })),
    );
    assert_eq!(*ed.spec(), before);
}

/// A driven param and a driven edge gain are written and taken out by their
/// source; an edge gain declares its lane, and a missing edge, node or
/// source is refused at the call.
#[test]
fn param_and_edge_gain_sources_are_written_and_taken_out() {
    let (mut ed, _exec) = editor();
    ed.insert(key(1), "level", Unforkable(Level(0.25)));
    ed.insert(key(2), "events", Unforkable(Events));
    ed.resize_outputs(1);
    let wire = into_output(0, out(1));
    ed.connect_edge(wire).expect("wires");
    let driver = EventOutputPort::new(key(2), 0);

    let at = ParamIn {
        node: key(1),
        param: UnitParam::Volume.into(),
    };
    ed.connect_param(at, driver).expect("wires");
    assert_eq!(ed.spec().params[&at], driver);
    assert!(!ed.disconnect_param(at, EventOutputPort::new(key(2), 5)));
    assert!(ed.disconnect_param(at, driver));
    assert!(ed.spec().params.is_empty());
    assert_eq!(
        ed.connect_param(ParamIn { node: key(9), ..at }, driver),
        Err(WireError::Missing(MissingSource::Node { node: key(9) })),
    );

    assert!(ed.edge_gain_cell(wire).is_none());
    ed.connect_edge_gain(wire, driver).expect("wires");
    assert_eq!(ed.spec().gains[&wire].source, Some(driver));
    assert!(ed.edge_gain_cell(wire).is_some(), "the lane is declared");
    assert!(ed.disconnect_edge_gain(wire, driver));
    assert_eq!(ed.spec().gains[&wire].source, None);
    assert!(ed.clear_edge_gain(wire));
    assert!(!ed.clear_edge_gain(wire));

    let ghost = into_output(0, out(2));
    assert_eq!(
        ed.connect_edge_gain(ghost, driver),
        Err(WireError::Gain(EdgeGainError::NoSuchEdge { id: ghost })),
    );
    assert_eq!(
        ed.connect_edge_gain(wire, EventOutputPort::new(key(1), 0)),
        Err(WireError::Missing(MissingSource::EventPort {
            node: key(1),
            port: 0,
            width: 0,
        })),
    );
}

/// `declare_main_output` writes the declaration and the generation the next
/// commit carries.
#[test]
fn declare_main_output_writes_the_declaration_and_its_generation() {
    let (mut ed, _exec) = editor();
    let topology = tutti_buffers::ChannelTopology::STEREO;
    let id = ed.spec().main_output_id.next();
    ed.declare_main_output(Some(topology), id);
    assert_eq!(ed.spec().main_output, Some(topology));
    assert_eq!(ed.spec().main_output_id, id);
}
