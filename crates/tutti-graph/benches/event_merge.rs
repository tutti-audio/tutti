//! The event merge of one block: `k` sources, each writing `n` events to its
//! port, fan into one sink input, which the executor merges by `(offset,
//! source order)`.
//!
//! The number it puts on the event size: what a merge of 32-byte events costs
//! (`midi/<k>x<n>`), and what the same graph costs when the sources write
//! param events beside them (`midi_with_params`, `params`).
//!
//! ```text
//! cargo bench -p tutti-graph --bench event_merge
//! ```

use std::hint::black_box;
use std::num::NonZeroU32;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventOut, Executor, Io, Node, NodeKey, Prepare, Shape,
    Status, TransportPosition, Unforkable,
};
use tutti_types::MaxBlock;
use tutti_ump::Ump;
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 1024;

/// What a source writes.
#[derive(Clone, Copy)]
enum Mix {
    /// `n` MIDI events.
    Midi,
}

struct Source {
    n: u32,
    mix: Mix,
}

impl Node for Source {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(NonZeroU32::new(self.n * 2).expect("a capacity"))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.event_out(0);
        for i in 0..self.n as usize {
            let at = cx.env.offset(i * BLOCK / self.n as usize).expect("inside");
            match self.mix {
                Mix::Midi => {
                    let _ = out.push(Event::midi(at, Ump([0x2090_3c64, 0, 0, 0])));
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Sink;

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        black_box(io.events(0).len());
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(k: u64, n: u32, mix: Mix) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples(BLOCK)),
    ));
    ed.insert(NodeKey(100), "sink", Unforkable(Sink));
    for i in 0..k {
        ed.insert(NodeKey(i + 1), "source", Unforkable(Source { n, mix }));
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(100),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(i + 1),
                port: 0,
            }),
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("event_merge");
    let transport = TransportPosition::default();
    for (k, n) in [(2u64, 64u32), (4, 256), (8, 256)] {
        g.throughput(Throughput::Elements(k * u64::from(n)));
        let mut exec = rig(k, n, Mix::Midi);
        g.bench_with_input(BenchmarkId::new("midi", format!("{k}x{n}")), &n, |b, _| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
