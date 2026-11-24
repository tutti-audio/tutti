//! The executor on this crate's filters at 1, 2, 4 and 8 participants.
//!
//! Shapes, each one block of `FRAMES` at 48 kHz, driven through the
//! `Executor` directly (no engine fold, no transport):
//!
//! - `wide/<lanes>x<depth>` — `lanes` independent chains of `depth`
//!   `SvfFilterNode`s, each off its own oscillator, summed by one
//!   fan-in on the output (the graph's `Op::Sum`): the shape that parallelises (every chain is one
//!   fused task, and `lanes` of them are ready at once).
//! - `deep/<depth>` — one chain of `depth` filters: nothing to spread (one
//!   task), so a pool must cost nothing here. A one-task plan runs on the
//!   calling thread alone.
//! - `tiny/<lanes>` — `lanes` single cheap gains off one input, summed:
//!   tasks too small to be worth dispatching. The gains declare
//!   `Cost::Light`, so the cost model packs them into a few tasks, which
//!   still spread over the pool. `tiny/<lanes>-undeclared` is the same graph
//!   with gains that declare nothing: every gain its own task — where a pool
//!   loses most.
//! - `mixed/<lanes>x<depth>x<fan>` — `lanes` chains of `depth` filters,
//!   each fanned out to `fan` light gains, every gain a source of the one
//!   output's fan-in (the graph sums `lanes * fan` addends): heavy work with
//!   cheap siblings, where packing the siblings (and not dispatching each)
//!   matters. `-undeclared` again for the gains declaring nothing.
//!
//! `workers` is the participant count, the calling thread included; `1` is
//! no pool. The pool is `tutti_graph::WorkerPool`
//! with its default waits, so a block's helpers are usually still spinning
//! from the last block (criterion runs blocks back to back); the wake cost
//! of a parked helper is not in these numbers.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench parallel_render
//! ```
//!
//! Eight participants on a four-core machine
//! oversubscribe it and are reported as measured, not dropped.

use std::hint::black_box;
use std::sync::Arc;
use tutti_buffers::MaxBlock;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_graph::WorkerPool;
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_graph::{Editor, Executor, Prepare, TransportPosition, Unforkable};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_units::{Hz, Q, SampleRate, Samples};

const SR: f64 = 48_000.0;
const FRAMES: usize = 256;
const WORKERS: [usize; 4] = [1, 2, 4, 8];

/// Adds `from`'s port 0 to the sources of main output 0: the graph sums them.
fn out(ed: &mut Editor, from: NodeKey) {
    ed.connect_edge(tutti_graph::EdgeId {
        sink: tutti_graph::PortSink::Output(0),
        from: tutti_graph::Edge::Direct(PortSource::Node(OutputPort::new(from, 0))),
    })
    .expect("the edge wires");
}

fn wire(ed: &mut Editor, node: NodeKey, port: u16, from: NodeKey) {
    ed.connect_edge(tutti_graph::EdgeId {
        sink: tutti_graph::PortSink::Port(InputPort::new(node, port)),
        from: Edge::Direct(PortSource::Node(OutputPort::new(from, 0))),
    })
    .expect("the edge wires");
}

/// `lanes` chains of `depth` filters (depth 0: a lone oscillator per lane),
/// summed; on a pool of `workers` participants when more than one.
fn wide(lanes: usize, depth: usize, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(FRAMES)),
    ));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(
            WorkerPool::builder()
                .num_threads(workers)
                .build()
                .expect("a worker pool"),
        )));
    }
    // One silent main output, which each lane's end joins as a source: the
    // graph's fan-in sums them.
    ed.wire_outputs([PortSource::Zero])
        .expect("the sources exist");
    for lane in 0..lanes {
        let base = 1_000 * (lane as u64 + 1);
        let osc = NodeKey(base);
        ed.insert(osc, "osc", Osc::sine(Hz::new(110.0 + lane as f64)));
        let mut last = osc;
        for i in 0..depth {
            let k = NodeKey(base + 1 + i as u64);
            ed.insert(
                k,
                "svf",
                SvfFilterNode::<f32>::from(
                    SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                        .freq(Hz::new(500.0 + (lane * depth + i) as f64 * 3.0))
                        .q(Q::new(0.7)),
                ),
            );
            wire(&mut ed, k, 0, last);
            last = k;
        }
        out(&mut ed, last);
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1 && lanes > 1);
    // The editor is dropped here; the executor keeps its plan.
    exec
}

/// A gain: one multiply per sample, the cheapest node there is. It
/// declares `Cost::Light` when `light`, and nothing (`Cost::Heavy`, the
/// default) otherwise: every gain a task of its own and any two independent ones spread.
struct Gain {
    gain: f32,
    light: bool,
}

impl tutti_graph::Node for Gain {
    fn shape(&self) -> tutti_graph::Shape {
        let s = tutti_graph::Shape::through_unassigned(ChannelLayout::MONO);
        if self.light {
            s.cost(tutti_graph::Cost::Light)
        } else {
            s
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (ins, mut outs) = io.split();
        for (y, x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *y = x * self.gain;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

fn gain(ed: &mut Editor, k: NodeKey, i: usize, light: bool) {
    ed.insert(
        k,
        "gain",
        Unforkable(Gain {
            gain: 0.5 + i as f32 * 1e-3,
            light,
        }),
    );
}

/// `lanes` single gains off one input, summed.
fn tiny(lanes: usize, light: bool, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(FRAMES)),
    ));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(
            WorkerPool::builder()
                .num_threads(workers)
                .build()
                .expect("a worker pool"),
        )));
    }
    ed.resize_inputs(ChannelLayout::MONO);
    // One silent main output, which each lane's end joins as a source: the
    // graph's fan-in sums them.
    ed.wire_outputs([PortSource::Zero])
        .expect("the sources exist");
    for lane in 0..lanes {
        let k = NodeKey(lane as u64 + 1);
        gain(&mut ed, k, lane, light);
        ed.connect_edge(tutti_graph::EdgeId {
            sink: tutti_graph::PortSink::Port(InputPort::new(k, 0)),
            from: Edge::Direct(PortSource::MainInput(0)),
        })
        .expect("the edge wires");
        out(&mut ed, k);
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    // Declared light, the gains pack into two tasks of eight (four tasks in
    // all); undeclared, a task per gain. Either spreads over a pool.
    assert_eq!(exec.is_parallel(), workers > 1);
    exec
}

/// `lanes` chains of `depth` filters, each fanned out to `fan` gains that
/// all join the output's fan-in: heavy chains with cheap
/// siblings hanging off them. Declared light, the cost model packs each
/// lane's gains; undeclared, each gain is a task.
fn mixed(lanes: usize, depth: usize, fan: usize, light: bool, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(FRAMES)),
    ));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(
            WorkerPool::builder()
                .num_threads(workers)
                .build()
                .expect("a worker pool"),
        )));
    }
    // One silent main output, which each lane's end joins as a source: the
    // graph's fan-in sums them.
    ed.wire_outputs([PortSource::Zero])
        .expect("the sources exist");
    for lane in 0..lanes {
        let base = 1_000 * (lane as u64 + 1);
        let osc = NodeKey(base);
        ed.insert(osc, "osc", Osc::sine(Hz::new(110.0 + lane as f64)));
        let mut last = osc;
        for i in 0..depth {
            let k = NodeKey(base + 1 + i as u64);
            ed.insert(
                k,
                "svf",
                SvfFilterNode::<f32>::from(
                    SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                        .freq(Hz::new(500.0 + (lane * depth + i) as f64 * 3.0))
                        .q(Q::new(0.7)),
                ),
            );
            wire(&mut ed, k, 0, last);
            last = k;
        }
        for j in 0..fan {
            let k = NodeKey(base + 500 + j as u64);
            gain(&mut ed, k, j, light);
            wire(&mut ed, k, 0, last);
            out(&mut ed, k);
        }
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    exec
}

fn block(exec: &mut Executor, input: &[f32], out: &mut [f32]) {
    let t = TransportPosition::default();
    exec.process(FRAMES, &t, &[input], &mut [out]);
}

fn group(c: &mut Criterion, name: &str, cases: &[(String, &dyn Fn(usize) -> Executor)]) {
    let mut g = c.benchmark_group(name);
    g.throughput(Throughput::Elements(FRAMES as u64));
    let input = vec![0.25f32; FRAMES];
    let mut out = vec![0.0f32; FRAMES];
    for (shape, build) in cases {
        for workers in WORKERS {
            let mut exec = build(workers);
            g.bench_with_input(BenchmarkId::new(shape, workers), &workers, |b, _| {
                b.iter(|| block(&mut exec, black_box(&input), black_box(&mut out)));
            });
        }
    }
    g.finish();
}

fn bench_wide(c: &mut Criterion) {
    let w8: &dyn Fn(usize) -> Executor = &|w| wide(8, 8, w);
    let w32: &dyn Fn(usize) -> Executor = &|w| wide(32, 8, w);
    let w64: &dyn Fn(usize) -> Executor = &|w| wide(64, 16, w);
    group(
        c,
        "wide",
        &[
            ("8x8".into(), w8),
            ("32x8".into(), w32),
            ("64x16".into(), w64),
        ],
    );
}

fn bench_deep(c: &mut Criterion) {
    let d: &dyn Fn(usize) -> Executor = &|w| wide(1, 128, w);
    group(c, "deep", &[("128".into(), d)]);
}

fn bench_tiny(c: &mut Criterion) {
    let light: &dyn Fn(usize) -> Executor = &|w| tiny(16, true, w);
    let undeclared: &dyn Fn(usize) -> Executor = &|w| tiny(16, false, w);
    group(
        c,
        "tiny",
        &[("16".into(), light), ("16-undeclared".into(), undeclared)],
    );
}

fn bench_mixed(c: &mut Criterion) {
    let light: &dyn Fn(usize) -> Executor = &|w| mixed(8, 4, 8, true, w);
    let undeclared: &dyn Fn(usize) -> Executor = &|w| mixed(8, 4, 8, false, w);
    group(
        c,
        "mixed",
        &[
            ("8x4x8".into(), light),
            ("8x4x8-undeclared".into(), undeclared),
        ],
    );
}

criterion_group!(benches, bench_wide, bench_deep, bench_tiny, bench_mixed);
criterion_main!(benches);
