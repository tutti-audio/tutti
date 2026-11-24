//! A fixture graph for this crate's integration tests: a constant level on
//! every output channel.
//!
//! DC rather than a tone, for the reason `tutti-export`'s `dc_net` gives: every
//! frame carries the same known value, so an expected result can be computed in
//! closed form instead of sampled. That is what makes the sample-format matrix
//! assertable.

use std::sync::Arc;
use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::{AudioCallbackState, Engine};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, ForkByClone, Io, Node, OutputPort, PortSource, Prepare, Shape, Status,
};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

pub const SAMPLE_RATE: f64 = 48_000.0;

/// A graph emitting a constant `level` on **every** one of `outputs` channels.
pub fn dc_state<F: Float>(level: F, outputs: usize) -> Arc<AudioCallbackState<F>> {
    let transport = Transport::new(SampleRate::of(SAMPLE_RATE));
    let (mut ed, exec) = Editor::<F>::with_format(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "dc", ForkByClone(Dc(level.to_f64())));
    ed.wire_outputs(vec![
        PortSource::Node(OutputPort::new(NodeKey(1), 0));
        outputs
    ])
    .expect("the sources exist");
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    // The editor outlives nothing here: a test process is the whole lifetime,
    // and nothing commits again.
    Box::leak(Box::new(ed));
    Arc::new(AudioCallbackState::new(engine))
}

/// A constant source in any sample format.
#[derive(Clone)]
struct Dc(f64);

impl<F: Float> Node<F> for Dc {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        io.output(0).fill(F::from_f64(self.0));
        Status::Modified
    }
    fn reset(&mut self) {}
}
