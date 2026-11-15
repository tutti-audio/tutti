//! A fixture graph for this crate's integration tests: a constant level on
//! every output channel.
//!
//! DC rather than a tone, for the reason `tutti-export`'s `dc_net` gives: every
//! frame carries the same known value, so an expected result can be computed in
//! closed form instead of sampled. That is what makes the sample-format matrix
//! assertable.

use std::sync::Arc;
use tutti_engine::{AudioCallbackState, Engine};
use tutti_graph::NodeKey;
use tutti_graph::{Editor, OutPort, Prepare, Source};
use tutti_nodes::testing::Const;
use tutti_transport::Transport;
use tutti_types::MaxBlock;
use tutti_types::{SampleRate, Samples};

pub const SAMPLE_RATE: f64 = 48_000.0;

/// A graph emitting a constant `level` on **every** one of `outputs` channels.
pub fn dc_state(level: f32, outputs: usize) -> Arc<AudioCallbackState> {
    let transport = Transport::new(SampleRate::of(SAMPLE_RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples(512)),
    ));
    ed.insert(NodeKey(1), "dc", Const::mono(level));
    ed.spec_mut().topology.set_outputs(vec![
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0
        });
        outputs
    ]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    // The editor outlives nothing here: a test process is the whole lifetime,
    // and nothing commits again.
    Box::leak(Box::new(ed));
    Arc::new(AudioCallbackState::new(engine))
}
