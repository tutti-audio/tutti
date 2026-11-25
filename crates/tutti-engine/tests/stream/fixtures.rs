//! Shared fixtures for this area's tests.
//!
//! The rolling-graph fixture: a sine through a filter, rendered by an
//! `Engine` whole blocks at a time.
//!
//! `no_alloc/block_no_alloc.rs` keeps its own copy, deliberately: every line
//! it runs before the gate has to be auditable in one file.

use tutti_graph::testing::WireDeclared;

use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_buffers::SampleFormat;
use tutti_engine::{AudioCallbackState, Engine, OutputSpec};
use tutti_graph::EdgeId;
use tutti_graph::NodeKey;
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_graph::{Editor, Prepare};
use tutti_nodes::testing::{Const, Osc};
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_timeline::{MotionCommand, Transport};
use tutti_units::{Bpm, Hz, Q, SampleRate, Samples};

pub(crate) const SAMPLE_RATE: f64 = 48_000.0;

/// An engine over `transport` rendering the graph `build` wires into
/// a fresh editor (prepared for 512-frame blocks at [`SAMPLE_RATE`]). The
/// editor is leaked: a test process is the whole lifetime, and nothing here
/// commits again.
pub(crate) fn graph_engine(transport: &Transport, build: impl FnOnce(&mut Editor)) -> Engine {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    build(&mut ed);
    ed.commit().expect("commits");
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    Box::leak(Box::new(ed));
    engine
}

/// Every one of `outputs` main outputs reads `node`'s output 0.
pub(crate) fn fan(ed: &mut Editor, node: NodeKey, outputs: usize) {
    ed.wire_declared_outputs(vec![PortSource::Node(OutputPort::new(node, 0)); outputs])
        .unwrap();
}

/// A rolling transport driving a sine through a filter, at `outputs` width.
///
/// The graph has to actually render: a graph with nothing wired renders
/// silence, and any assertion over it is vacuous.
pub(crate) fn rolling_state(outputs: usize) -> (Transport, Arc<AudioCallbackState>) {
    let transport = Transport::new(SampleRate::of(SAMPLE_RATE));
    let engine = graph_engine(&transport, |ed| {
        let (source, filter) = (NodeKey(1), NodeKey(2));
        ed.insert(source, "sine", Osc::sine(Hz::new(220.0)));
        ed.insert(
            filter,
            "filter",
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                    .freq(Hz::new(2_000.0))
                    .q(Q::new(0.7)),
            ),
        );
        ed.connect_edge(EdgeId::into_port(
            InputPort::new(filter, 0),
            Edge::Direct(PortSource::Node(OutputPort::new(source, 0))),
        ))
        .unwrap();
        fan(ed, filter, outputs);
    });
    let state = AudioCallbackState::new(engine);

    transport.settings().set_tempo(Bpm::of(120.0));
    let _ = transport.motion().send(MotionCommand::Play);

    (transport, Arc::new(state))
}

/// A graph emitting a constant `level` on **every** output channel.
///
/// DC rather than a tone, for the reason `tutti-export`'s `dc_net` gives: every
/// frame carries the same known value, so an expected result can be computed
/// in closed form instead of sampled. That is what makes the sample-format
/// matrix assertable.
pub(crate) fn dc_state(level: f32, outputs: usize) -> Arc<AudioCallbackState> {
    let engine = graph_engine(&Transport::new(SampleRate::of(SAMPLE_RATE)), |ed| {
        ed.insert(NodeKey(1), "dc", Const::mono(level));
        fan(ed, NodeKey(1), outputs);
    });
    Arc::new(AudioCallbackState::new(engine))
}

/// A spec with no device behind it, at `channels` width and `format`.
pub(crate) fn spec(channels: usize, format: SampleFormat) -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(SAMPLE_RATE),
        ChannelLayout::try_from(channels).expect("a test width is small"),
        format,
    )
}
