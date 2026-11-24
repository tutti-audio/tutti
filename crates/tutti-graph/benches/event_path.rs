//! The event path of one block: a source writing events to a port, through
//! a merge, into a sink that reads them.
//!
//! `not_full/<n>`: the source writes `n` note-ons into a port of room for
//! them (no refusal), the hot path. `at_capacity/<n>`: it writes `n` note-ons
//! and `n` note-offs to a port of `n`, so the note-ons fill it and the
//! note-offs go on in the room past it. `over/<n>`: it writes `4 n` note-ons
//! to a port of `n`, the refusal path, 3 `n` of them counted. `idle/<k>`: `k`
//! sinks with an event input and nothing wired, skipped once idle.
//! `dense/<k>x<n>`: `k` sources of `n` events each into one sink.
//!
//! ```text
//! cargo bench -p tutti-graph --bench event_path
//! ```

use std::hint::black_box;
use tutti_buffers::MaxBlock;
use tutti_ump::Ump;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventInputPort, EventOutputPort, Executor, InEvents, Io, Node,
    OutEvents, Prepare, Shape, Status, TransportPosition, Unforkable,
};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 1024;

struct Source {
    out: OutEvents,
    cap: u32,
    ons: u32,
    offs: u32,
}

impl Source {
    fn new(cap: u32, ons: u32, offs: u32) -> Self {
        Self {
            out: Self::declared(cap)
                .event_output(0)
                .expect("one event output"),
            cap,
            ons,
            offs,
        }
    }

    fn declared(cap: u32) -> Shape {
        Shape::EMPTY
            .events(0, 1)
            .event_capacity(std::num::NonZeroU32::new(cap).expect("a capacity"))
    }
}

impl Node for Source {
    fn shape(&self) -> Shape {
        Self::declared(self.cap)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.event_out(self.out);
        let total = (self.ons + self.offs) as usize;
        for i in 0..self.ons + self.offs {
            let word = if i < self.ons {
                0x2090_3c64
            } else {
                0x2080_3c00
            };
            let at = cx.env.offset(i as usize * BLOCK / total).expect("inside");
            let _ = out.push(Event::midi(at, Ump([word, 0, 0, 0])));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Sink(InEvents);

impl Sink {
    const SHAPE: Shape = Shape::EMPTY.events(1, 0);

    fn new() -> Self {
        Self(Self::SHAPE.event_input(0).expect("one event input"))
    }
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        black_box(io.events(self.0).len());
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A sink with one event input that has nothing to do: it says `Idle`, so with
/// nothing wired it is skipped after its first call, and the block costs what
/// the executor does for an event input with nothing in it.
struct IdleSink;

impl Node for IdleSink {
    fn shape(&self) -> Shape {
        Sink::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Idle
    }
    fn reset(&mut self) {}
}

fn rig(source: Source) -> Executor {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(BLOCK)),
    ));
    ed.insert(NodeKey(1), "source", Unforkable(source));
    ed.insert(NodeKey(2), "sink", Unforkable(Sink::new()));
    ed.connect_events(
        EventInputPort::new(NodeKey(2), 0),
        EventEdge::Direct(EventOutputPort::new(NodeKey(1), 0)),
    )
    .unwrap();
    ed.commit().expect("commits");
    let mut exec = exec;
    exec.apply_pending();
    ed.collect();
    exec
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("event_path");
    let transport = TransportPosition::default();
    for n in [64u32, 256, 1000] {
        g.throughput(Throughput::Elements(u64::from(n)));
        let cases = [
            ("not_full", n * 2, n, 0),
            ("at_capacity", n, n, n),
            ("over", n, 4 * n, 0),
        ];
        for (name, cap, ons, offs) in cases {
            let mut exec = rig(Source::new(cap, ons, offs));
            g.bench_with_input(BenchmarkId::new(name, n), &n, |b, _| {
                b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
            });
        }
    }
    // The per-call cost of an event-output node: `k` sources of 16 events
    // each into a port of 64 (nothing refused), no consumer.
    for k in [1u64, 8, 32] {
        let (mut ed, mut exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(BLOCK)),
        ));
        for i in 0..k {
            ed.insert(NodeKey(i + 1), "source", Unforkable(Source::new(64, 16, 0)));
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        g.throughput(Throughput::Elements(k));
        g.bench_with_input(BenchmarkId::new("nodes", k), &k, |b, _| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    // The event input of a node with nothing wired: `k` idle sinks.
    for k in [1u64, 64, 512] {
        let (mut ed, mut exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(BLOCK)),
        ));
        for i in 0..k {
            ed.insert(NodeKey(i + 1), "idle", Unforkable(IdleSink));
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        g.throughput(Throughput::Elements(k));
        g.bench_with_input(BenchmarkId::new("idle", k), &k, |b, _| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    // An event-dense input: `k` sources of `n` note-ons each fan into one sink,
    // so the sink's input holds `k * n` events a block.
    for (k, n) in [(8u64, 256u32)] {
        let (mut ed, mut exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(BLOCK)),
        ));
        ed.insert(NodeKey(100), "sink", Unforkable(Sink::new()));
        for i in 0..k {
            ed.insert(
                NodeKey(i + 1),
                "source",
                Unforkable(Source::new(n * 2, n, 0)),
            );
            ed.connect_events(
                EventInputPort::new(NodeKey(100), 0),
                EventEdge::Direct(EventOutputPort::new(NodeKey(i + 1), 0)),
            )
            .unwrap();
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        g.throughput(Throughput::Elements(k * u64::from(n)));
        g.bench_with_input(BenchmarkId::new("dense", format!("{k}x{n}")), &n, |b, _| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
