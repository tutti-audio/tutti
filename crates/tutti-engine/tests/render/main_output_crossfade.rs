//! A main output crossfade on the parallel executor renders the serial executor's
//! samples bit for bit: the old main output's channels are outputs of the plan like
//! the main output's own, claimed and written by the same tasks.

use crate::support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::Engine;
use tutti_graph::{Edge, Editor, Fade, ForkByClone, InputPort, NodeKey, OutputPort};
use tutti_graph::{Pool, PortSource, Prepare, WorkerPool};
use tutti_timeline::Transport;
use tutti_units::{Hz, SampleRate, Samples, Seconds};

/// A pool that counts the jobs it runs, so a test knows blocks spread.
struct Counting(WorkerPool, Arc<AtomicUsize>);

impl Pool for Counting {
    fn participants(&self) -> usize {
        self.0.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.run(wake, job);
    }
    fn waits(&self) -> tutti_rt::BlockWaits {
        self.0.waits()
    }
}

/// Six sines, each through its own gain (keys 11 to 16), the main output's two
/// channels taking `pair`.
fn wire(ed: &mut Editor, pair: [u64; 2]) {
    ed.spec_mut().topology.set_outputs(
        pair.map(|k| PortSource::Node(OutputPort::new(NodeKey(k), 0)))
            .to_vec(),
    );
}

/// 64-frame stereo blocks of a graph whose main output moves from channels (11, 12)
/// to (15, 16) after two blocks, with the crossfade over the declick length
/// (four blocks), on a pool of four if `pooled`. Returns the samples and how
/// many jobs the pool ran.
fn render(pooled: bool) -> (Vec<f32>, usize) {
    let rate = SampleRate::SR_48K;
    let (mut ed, mut exec) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples::new(64))));
    let jobs = Arc::new(AtomicUsize::new(0));
    if pooled {
        let pool = WorkerPool::builder()
            .num_threads(4)
            .build()
            .expect("a worker pool");
        exec.set_pool(Some(Arc::new(Counting(pool, Arc::clone(&jobs)))));
    }
    for k in 0..6u64 {
        ed.insert(
            NodeKey(k + 1),
            "sine",
            ForkByClone(support::Sine::new(Hz::new(220.0 * (k + 1) as f64))),
        );
        ed.insert(
            NodeKey(k + 11),
            "gain",
            ForkByClone(support::Gain(0.1 * (k + 1) as f32)),
        );
        ed.spec_mut().topology.edges.insert(
            InputPort::new(NodeKey(k + 11), 0),
            vec![Edge::Direct(PortSource::Node(OutputPort::new(
                NodeKey(k + 1),
                0,
            )))],
        );
    }
    wire(&mut ed, [11, 12]);
    ed.commit().expect("commits");
    let engine = Engine::new(&Transport::new(rate), &mut ed, exec).expect("within limits");
    ed.set_main_output_fade(Some(Fade::seconds(Seconds::new(0.005), rate)));
    let control = engine.output_control();
    let mut heard = Vec::new();
    let mut block = vec![0.0f32; 64 * 2];
    for n in 0..9 {
        if n == 2 {
            let id = ed.spec().main_output_id.next();
            control
                .publish_main_output(id, None, ChannelLayout::STEREO)
                .expect("places");
            ed.spec_mut().main_output_id = id;
            wire(&mut ed, [15, 16]);
            ed.commit().expect("commits");
            assert!(
                ed.base().is_some_and(|p| p.main_output_bank().is_some()),
                "the plan carries the old main output's channels"
            );
        }
        engine.process(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        ed.collect();
        heard.extend_from_slice(&block);
    }
    (heard, jobs.load(Ordering::Relaxed))
}

/// The main output crossfade's samples are the serial render's, bit for bit, and
/// the blocks did spread over the pool.
#[test]
fn a_main_output_crossfade_is_bit_identical_on_the_parallel_executor() {
    let (serial, _) = render(false);
    let (parallel, jobs) = render(true);
    assert!(jobs > 0, "the pool ran jobs: the plan spread");
    assert!(
        serial.iter().any(|&x| x != 0.0),
        "the graph rendered something"
    );
    assert_eq!(
        serial.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        parallel.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
    );
}
