//! The engine's load and block counts, end to end: `Engine::process` and
//! `process_segment` time the whole call against the block's budget.
//!
//! The figures come from a node that spins for a known duration, so the
//! load has a floor no scheduler can lower: a spin of `k` budgets is a load
//! of at least `k`. Where a test also needs a ceiling it is generous and
//! says why, because wall time on a shared machine has no upper bound.
//! The smoothing arithmetic is pinned exactly, on constructed durations, by
//! the unit tests beside it (`load/tests.rs`).

use std::time::Duration;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Editor, Prepare, Unforkable};
use tutti_graph::{OutputPort, PortSource};
use tutti_platform::Instant;
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// A mono source that busy-waits `spin` per graph block, then writes zeros.
struct Spin {
    spin: Duration,
}

impl tutti_graph::Node for Spin {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let end = Instant::now() + self.spin;
        while Instant::now() < end {
            std::hint::spin_loop();
        }
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// An engine over one `Spin`, with graph blocks of at most `max_block`.
fn engine(spin: Duration, max_block: usize) -> (Engine, Transport, Editor) {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(max_block)),
    ));
    ed.insert(NodeKey(1), "spin", Unforkable(Spin { spin }));
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    (engine, transport, ed)
}

fn budget(frames: usize) -> Duration {
    Duration::from_secs_f64(frames as f64 / RATE)
}

/// A block whose render spins for twice its budget reads a load of at least
/// two (the first block seeds the mean) and counts one block over budget.
#[test]
fn a_block_past_its_deadline_reads_its_load_and_counts_once() {
    let (engine, _t, _ed) = engine(budget(64) * 2, 64);
    let (load, blocks) = (engine.load(), engine.blocks());
    let mut out = vec![0.0f32; 64];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    let r = blocks.get();
    assert_eq!((r.blocks, r.over_budget), (1, 1));
    assert!(load.get().get() >= 2.0, "{:?}", load.get());
}

/// A call of 8 192 frames over a 4 096-frame `MaxBlock` is two graph blocks,
/// each spinning 1.5 budgets of 4 096 frames, so the call's load is at least
/// 1.5 whichever entry point renders it, and each call counts one block over
/// budget. The floor is the one thing no scheduler can lower; the budget's
/// size is pinned analytically by `load/tests.rs`.
#[test]
fn a_call_of_two_graph_blocks_reads_at_least_its_spin_and_counts_each_call_once() {
    let (engine, _t, _ed) = engine(budget(4096).mul_f64(1.5), 4096);
    let (load, blocks) = (engine.load(), engine.blocks());
    let mut out = vec![0.0f32; 8192];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    let seeded = load.get().get();
    assert!(seeded >= 1.5, "process: {seeded}");
    assert_eq!(blocks.get().over_budget, 1);
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    let smoothed = load.get().get();
    assert!(smoothed >= 1.5, "segment: {smoothed}");
    let r = blocks.get();
    assert_eq!((r.blocks, r.over_budget), (2, 2));
}

/// Recording the load is allocation-free: the gate runs `process` and
/// `process_segment` (each recording a block) and the reads.
#[test]
fn recording_the_load_is_allocation_free() {
    let (engine, _t, mut ed) = engine(Duration::ZERO, 64);
    let mut out = vec![0.0f32; 64];
    // Installing the commit allocates on the first block, before the gate.
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    ed.collect();
    let (load, blocks) = (engine.load(), engine.blocks());
    let before = blocks.get().blocks;
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
        engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
        let _ = (load.get(), blocks.get());
    }
    assert_eq!(blocks.get().blocks - before, 32);
}

/// A mono source that also writes `per_block` events to its one event
/// output each block, at the default capacity.
struct Burst {
    per_block: usize,
}

impl Burst {
    const SHAPE: tutti_graph::Shape =
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO).with_events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl tutti_graph::Node for Burst {
    fn shape(&self) -> tutti_graph::Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.0);
        let out = io.event_out(Self::OUT);
        for _ in 0..self.per_block {
            // A note-on; a full port refuses it, which is the point.
            let _ = out.push(tutti_graph::Event::midi(
                tutti_graph::Offset::ZERO,
                tutti_ump::Ump([0x2090_3c7f, 0, 0, 0]),
            ));
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// Events a node writes past its port's capacity reach the block counts: a
/// node writing 100 more than the default capacity each block adds 100
/// dropped per device block, from `process` and `process_segment` both.
#[test]
fn events_past_a_ports_capacity_reach_the_block_counts() {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(64)),
    ));
    let per_block = tutti_graph::DEFAULT_EVENT_CAPACITY + 100;
    ed.insert(NodeKey(1), "burst", Unforkable(Burst { per_block }));
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let blocks = engine.blocks();
    let mut out = vec![0.0f32; 64];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    assert_eq!(blocks.get().dropped_events, 100);
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    assert_eq!(blocks.get().dropped_events, 200);
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap());
    assert_eq!(blocks.get().dropped_events, 300);
}
