//! The event merge of one block: `k` sources, each writing `n` events to its
//! port, fan into one sink input, which the executor merges by `(offset,
//! source order)`.
//!
//! The number it puts on the event size: what a merge of 32-byte events costs
//! (`midi/<k>x<n>`), what the same merge costs when the sources write param
//! events (`params/<k>x<n>`, their `f64` payload in a buffer of its own), and
//! both together (`mixed/<k>x<n>`: `n` of each), so a MIDI event does not pay
//! for a param payload and the param buffer's merge is one more pass.
//!
//! ```text
//! cargo bench -p tutti-graph --bench event_merge
//! ```

use std::hint::black_box;
use std::num::NonZeroU32;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::MaxBlock;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventInputPort, EventOutputPort, Executor, InEvents, Io, Node,
    NodeKey, OutEvents, ParamEvent, ParamSegment, Prepare, Shape, Status, TransportPosition,
    Unforkable,
};
use tutti_ump::Ump;
use tutti_units::{ParamId, SampleRate, Samples};

const BLOCK: usize = 1024;

/// What a source writes.
#[derive(Clone, Copy)]
enum Mix {
    /// `n` MIDI events.
    Midi,
    /// `n` param events.
    Params,
    /// `n` MIDI events and `n` param events.
    Mixed,
}

struct Source {
    out: OutEvents,
    n: u32,
    mix: Mix,
}

impl Source {
    fn new(n: u32, mix: Mix) -> Self {
        Self {
            out: Self::declared(n, mix)
                .event_output(0)
                .expect("one event output"),
            n,
            mix,
        }
    }

    fn declared(n: u32, mix: Mix) -> Shape {
        let shape = Shape::EMPTY
            .events(0, 1)
            .event_capacity(NonZeroU32::new(n * 2).expect("a capacity"));
        match mix {
            Mix::Midi => shape,
            Mix::Params | Mix::Mixed => shape.param_events(),
        }
    }
}

impl Node for Source {
    fn shape(&self) -> Shape {
        Self::declared(self.n, self.mix)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.event_out(self.out);
        for i in 0..self.n as usize {
            let at = cx.env.offset(i * BLOCK / self.n as usize).expect("inside");
            if matches!(self.mix, Mix::Midi | Mix::Mixed) {
                let _ = out.push(Event::midi(at, Ump([0x2090_3c64, 0, 0, 0])));
            }
            if matches!(self.mix, Mix::Params | Mix::Mixed) {
                let seg = ParamSegment::by_id(ParamId::new(1), 0.0, 1.0, Samples::new(32));
                let _ = out.push_param(ParamEvent::segment(at, seg));
            }
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
        black_box(io.events(self.0).len() + io.events(self.0).params().len());
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(k: u64, n: u32, mix: Mix) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(BLOCK)),
    ));
    ed.insert(NodeKey(100), "sink", Unforkable(Sink::new()));
    for i in 0..k {
        ed.insert(NodeKey(i + 1), "source", Unforkable(Source::new(n, mix)));
        ed.connect_events(
            EventInputPort::new(NodeKey(100), 0),
            EventEdge::Direct(EventOutputPort::new(NodeKey(i + 1), 0)),
        )
        .expect("the edge wires");
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
        for (name, mix) in [
            ("midi", Mix::Midi),
            ("params", Mix::Params),
            ("mixed", Mix::Mixed),
        ] {
            let mut exec = rig(k, n, mix);
            g.bench_with_input(BenchmarkId::new(name, format!("{k}x{n}")), &n, |b, _| {
                b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
