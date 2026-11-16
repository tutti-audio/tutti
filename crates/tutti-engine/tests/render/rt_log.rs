//! What the graph's nodes log on the audio thread reaches the control side
//! through the engine's `RtLogReaders`: merged by frame, counted when lost,
//! and kept across an executor's resizing.

use std::sync::Arc;

use tutti_buffers::InterleavedMut;
use tutti_engine::{Engine, RtLogReaders};
use tutti_graph::{
    Cx, Editor, Executor, Io, LogFilter, Node, NodeKey, OutPort, Prepare, RtLevel, RtRecord,
    RtValue, Shape, Source, Status, TransportPosition, Unforkable, WorkerPool,
};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, Frame, MaxBlock, SampleRate, Samples, Tail};

static SITE: &str = "site";

const ALL: LogFilter = LogFilter::new(Some(RtLevel::Trace), Some(RtLevel::Trace));

/// A silent source that logs one record at each of `offsets` per block.
struct Logger {
    offsets: Vec<usize>,
}

impl Node for Logger {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for &o in &self.offsets {
            let at = io.offset(o).expect("inside the block");
            io.log(RtLevel::Warn, SITE, at, &[("offset", RtValue::from(o))]);
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One logger node per entry of `loggers`, each an output.
fn graph(loggers: &[Vec<usize>], max_block: usize) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples(max_block)),
    ));
    let mut outputs = Vec::new();
    for (i, offsets) in loggers.iter().enumerate() {
        let key = NodeKey(i as u64 + 1);
        ed.insert(
            key,
            "logger",
            Unforkable(Logger {
                offsets: offsets.clone(),
            }),
        );
        outputs.push(Source::Node(OutPort { node: key, port: 0 }));
    }
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    (ed, exec)
}

fn render(exec: &mut Executor, frames: usize, outs: usize) {
    let mut bufs = vec![vec![0.0f32; frames]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(frames, &TransportPosition::default(), &[], &mut refs);
}

fn collect(readers: &RtLogReaders) -> (Vec<RtRecord>, u64) {
    let mut out = Vec::new();
    let lost = readers.drain(ALL, |r| out.push(*r));
    (out, lost)
}

#[test]
fn an_engine_renders_a_nodes_records_to_its_readers_from_the_block_after_the_filter_is_stored() {
    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = graph(&[vec![3]], 64);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let readers = engine.rt_log();
    let block = |engine: &Engine| {
        let mut buf = vec![0.0f32; 64];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO).unwrap());
    };
    block(&engine);
    let (before, _) = collect(&readers);
    assert!(before.is_empty(), "the gate was off for the first block");
    block(&engine);
    let (after, lost) = collect(&readers);
    assert_eq!(lost, 0);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].source(), NodeKey(1));
    assert_eq!(after[0].fields(), &[("offset", RtValue::U64(3))]);
    assert_eq!(after[0].frame(), Frame::new(64 + 3));
}

#[test]
fn a_drain_hands_the_records_of_every_node_in_frame_order_whatever_order_they_ran_in() {
    // The first node logs late in the block, the second early: the ring holds
    // them in the order they ran.
    let (mut ed, mut exec) = graph(&[vec![50], vec![5]], 64);
    exec.apply_pending();
    ed.collect();
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    render(&mut exec, 64, 2);
    let (records, _) = collect(&readers);
    let frames: Vec<_> = records.iter().map(RtRecord::frame).collect();
    assert_eq!(frames, [Frame::new(5), Frame::new(50)]);
}

#[test]
fn a_drain_across_a_pools_rings_is_ordered_by_frame_and_complete() {
    let offsets: Vec<Vec<usize>> = (0..6).map(|i| vec![i, 20 + i]).collect();
    let (mut ed, mut exec) = graph(&offsets, 64);
    let pool = WorkerPool::builder()
        .num_threads(4)
        .build()
        .expect("a worker pool");
    exec.set_pool(Some(Arc::new(pool)));
    exec.apply_pending();
    ed.collect();
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    for _ in 0..8 {
        render(&mut exec, 64, 6);
    }
    let (records, lost) = collect(&readers);
    assert_eq!(lost, 0);
    assert_eq!(records.len(), 8 * 12);
    assert!(records.windows(2).all(|w| w[0].frame() <= w[1].frame()));
}

#[test]
fn records_past_a_rings_room_are_counted_once_in_the_drain_that_sees_them() {
    // Four distinct records a block (offsets 0..4) into 256 slots.
    let (mut ed, mut exec) = graph(&[vec![0, 1, 2, 3]], 8);
    exec.apply_pending();
    ed.collect();
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    for _ in 0..70 {
        render(&mut exec, 8, 1);
    }
    let (records, lost) = collect(&readers);
    assert_eq!((records.len(), lost), (256, 4 * 70 - 256));
    assert_eq!(collect(&readers).1, 0, "a loss is reported once");
}

#[test]
fn resizing_the_executor_delivers_what_the_old_rings_held_and_reads_the_new_ones() {
    let (mut ed, mut exec) = graph(&[vec![1]], 64);
    exec.apply_pending();
    ed.collect();
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    render(&mut exec, 64, 1);
    let pool = WorkerPool::builder()
        .num_threads(3)
        .build()
        .expect("a worker pool");
    exec.set_pool(Some(Arc::new(pool)));
    readers.adopt(&mut exec);
    render(&mut exec, 64, 1);
    let (records, lost) = collect(&readers);
    assert_eq!(lost, 0);
    let frames: Vec<_> = records.iter().map(RtRecord::frame).collect();
    assert_eq!(
        frames,
        [Frame::new(1), Frame::new(64 + 1)],
        "the block before the resize, and the one after"
    );
}
